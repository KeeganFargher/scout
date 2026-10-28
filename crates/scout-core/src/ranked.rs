//! `--rank`: the same documents verified by the same matcher as [`crate::search::search`],
//! then the matching files in score order (docs/ranking.md). The set of files equals rg's;
//! only their order, and how many are shown, differ.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};

use crate::query::{PlanCase, plan};
use crate::rank::{self, Corpus, FileFeatures, HEAD_BYTES, TermHits, Weights};
use crate::search::{
    CaseMode, Doc, Loc, Mode, Printer, RepoHandle, SearchOptions, SearchStats, build_matcher, build_searcher,
    check_deadline, in_repo, walk_repo,
};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct RankOptions {
    /// Files to print, best first.
    pub top: usize,
    pub weights: Weights,
}

/// What a ranked search found: every matching file's features in rank order (the printed
/// ones first), and what scoring was relative to, so the evaluation harness can re-score
/// them under other weights.
#[derive(Debug)]
pub struct RankedReport {
    pub stats: SearchStats,
    pub snapshots: Vec<(usize, Vec<u8>)>,
    /// The query's terms (top-level alternation branches), in pattern order.
    pub terms: Vec<String>,
    pub corpus: Corpus,
    /// Every matching file, in rank order.
    pub files: Vec<FileFeatures>,
}

/// A matching file and where to read it again for printing.
struct Found {
    features: FileFeatures,
    repo: usize,
    loc: OwnedLoc,
}

enum OwnedLoc {
    Indexed { shard: usize, doc: crate::ports::DocId, mode: Mode },
    Dirty(String),
}

type RepoFound = (Vec<Found>, SearchStats, Option<Vec<u8>>);

/// # Panics
///
/// Only by re-raising a worker thread's panic.
pub fn search_ranked(
    opts: &SearchOptions,
    rank: &RankOptions,
    repos: &[&RepoHandle],
    out: &mut dyn Write,
) -> Result<RankedReport> {
    let started = Instant::now();
    if opts.threads == 0 {
        return Err(Error::Invalid("threads must be > 0".into()));
    }
    if rank.top == 0 {
        return Err(Error::Invalid("top must be > 0".into()));
    }
    let pattern = if opts.fixed_strings { regex_syntax::escape(&opts.pattern) } else { opts.pattern.clone() };
    let plan_case = if opts.case == CaseMode::Sensitive { PlanCase::Sensitive } else { PlanCase::Insensitive };
    let query = plan(&pattern, plan_case)?;
    let matcher = build_matcher(&pattern, opts)?;
    let terms = split_terms(&pattern, opts.fixed_strings)?;
    let term_opts = SearchOptions { case: resolved_case(opts.case, &pattern), ..opts.clone() };
    let term_matchers = terms.iter().map(|t| build_matcher(t, &term_opts)).collect::<Result<Vec<_>>>()?;

    // Phase 1: verify and collect features, in parallel over repos like the plain search.
    let next = AtomicUsize::new(0);
    let workers = opts.threads.min(repos.len()).max(1);
    let (tx, rx) = mpsc::channel::<(usize, Result<RepoFound>)>();
    let mut stats = SearchStats { repos: repos.len(), plan: format!("{query:?}"), ..SearchStats::default() };
    let mut snapshots = Vec::new();
    let mut found = Vec::new();
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, query, matcher, term_matchers) = (&next, &query, matcher.clone(), &term_matchers);
            scope.spawn(move || {
                let mut searcher = build_searcher(opts);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(repo) = repos.get(i) else { break };
                    let r = collect_repo(i, repo, query, &matcher, term_matchers, &mut searcher, opts);
                    if tx.send((i, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Consumed in repo order, so the error reported is the first repo's, not the first
        // to arrive: a failure reads the same whatever the scheduling.
        let mut slots: Vec<Option<Result<RepoFound>>> = (0..repos.len()).map(|_| None).collect();
        for (i, r) in rx {
            slots[i] = Some(r);
        }
        for (i, r) in slots.into_iter().enumerate() {
            // Every repo was claimed and sent unless a worker panicked, which `scope`
            // re-raises.
            let (f, s, snapshot) = r.expect("every repo has a result").map_err(|e| in_repo(repos[i], e))?;
            stats.merge(&s);
            snapshots.extend(snapshot.map(|b| (i, b)));
            found.extend(f);
        }
        Ok(())
    })?;

    // Score and order. Ties break on path, so the order never depends on thread timing.
    let features: Vec<FileFeatures> = found.iter().map(|f| f.features.clone()).collect();
    let corpus = Corpus::of(&features, terms.len(), stats.docs);
    let order = rank::collapse_copies(&features, rank::order(&features, &corpus, &rank.weights));

    // Phase 2: print the top files exactly as the plain search would print each.
    let mut searcher = build_searcher(opts);
    let mut printer = Printer::new(opts.output);
    let mut buf = Vec::new();
    for &i in order.iter().take(rank.top) {
        let f = &found[i];
        let repo = repos[f.repo];
        let path = &f.features.path;
        match &f.loc {
            OwnedLoc::Indexed { shard, doc, mode } => {
                let content = repo.shards[*shard].content(*doc, &mut buf)?;
                printer.search(&mut searcher, &matcher, Doc { path, content, mode: *mode, first_line: 1 })?;
            }
            OwnedLoc::Dirty(rel) => {
                let content = repo.source.read(rel)?;
                printer.search(
                    &mut searcher,
                    &matcher,
                    Doc { path, content: &content, mode: Mode::Reader, first_line: 1 },
                )?;
            }
        }
        if let Some(cap) = opts.limits.output_bytes
            && printer.len() > cap
        {
            return Err(Error::OutputLimit(cap));
        }
    }
    out.write_all(&printer.into_bytes()).map_err(|e| Error::io("<stdout>", e))?;

    let mut by_rank: Vec<Option<FileFeatures>> = features.into_iter().map(Some).collect();
    let files = order.iter().filter_map(|&i| by_rank[i].take()).collect();
    stats.elapsed = started.elapsed();
    Ok(RankedReport { stats, snapshots, terms, corpus, files })
}

fn collect_repo(
    index: usize,
    repo: &RepoHandle,
    query: &crate::query::Query,
    matcher: &RegexMatcher,
    term_matchers: &[RegexMatcher],
    searcher: &mut Searcher,
    opts: &SearchOptions,
) -> Result<RepoFound> {
    let mut stats = SearchStats { shards: repo.shards.len(), ..SearchStats::default() };
    let mut found = Vec::new();
    // Verification runs inside the walk's callback here (no separate phase as in the plain
    // search), so `--stats` verify time is the callback's time.
    let mut verify = std::time::Duration::ZERO;
    let snapshot = walk_repo(repo, query, opts, &mut stats, |loc, rel, doc| {
        check_deadline(opts)?;
        let t = std::time::Instant::now();
        let features = features(searcher, matcher, term_matchers, rel, doc);
        verify += t.elapsed();
        let Some(features) = features? else {
            return Ok(false);
        };
        let loc = match loc {
            Loc::Indexed { shard, doc, mode } => OwnedLoc::Indexed { shard, doc, mode },
            Loc::Dirty(rel) => OwnedLoc::Dirty(rel.to_string()),
        };
        found.push(Found { features, repo: index, loc });
        Ok(true)
    })?;
    stats.verify_time = verify;
    Ok((found, stats, snapshot))
}

/// The features of `doc`, or `None` when it does not match. Matching runs through the same
/// searcher and matcher as the plain search, so binary handling (and hence the matched
/// set) is rg's.
fn features(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    term_matchers: &[RegexMatcher],
    rel: &str,
    doc: Doc<'_>,
) -> Result<Option<FileFeatures>> {
    let mut sink = TermSink { terms: term_matchers, hits: vec![TermHits::default(); term_matchers.len()], lines: 0 };
    searcher.set_first_line_number(doc.first_line);
    let r = match doc.mode {
        Mode::Slice => searcher.search_slice(matcher, doc.content, &mut sink),
        // A fresh clone per reader doc, for the reason given in `search::run`.
        Mode::Reader => searcher.clone().search_reader(matcher, doc.content, &mut sink),
    };
    r.map_err(|e| Error::io(doc.path, e))?;
    if sink.lines == 0 {
        return Ok(None);
    }
    let (dirs, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let mut hits = sink.hits;
    for (h, m) in hits.iter_mut().zip(term_matchers) {
        h.basename = is_match(m, name.as_bytes())?;
        h.path = !dirs.is_empty() && is_match(m, dirs.as_bytes())?;
    }
    let content = doc.content;
    let lines = memchr::memchr_iter(b'\n', content).count() as u64 + 1;
    let mut longest = 0usize;
    let mut start = 0usize;
    for nl in memchr::memchr_iter(b'\n', content).chain(std::iter::once(content.len())) {
        longest = longest.max(nl - start);
        start = nl + 1;
    }
    let head = &content[..content.len().min(HEAD_BYTES)];
    Ok(Some(FileFeatures {
        path: doc.path.to_string(),
        bytes: content.len() as u64,
        noise: rank::classify(rel, head, content.len() as u64, lines, longest as u64),
        content_hash: xxhash_rust::xxh3::xxh3_64(content),
        terms: hits,
    }))
}

fn is_match(m: &RegexMatcher, haystack: &[u8]) -> Result<bool> {
    m.is_match(haystack).map_err(|e| Error::Invalid(format!("term matcher: {e}")))
}

/// Counts each term's matches on every line the search matched.
struct TermSink<'a> {
    terms: &'a [RegexMatcher],
    hits: Vec<TermHits>,
    lines: u64,
}

impl Sink for TermSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> std::result::Result<bool, Self::Error> {
        self.lines += 1;
        let line = mat.bytes();
        let defined = rank::defined_name(line);
        for (m, h) in self.terms.iter().zip(self.hits.iter_mut()) {
            m.find_iter(line, |found| {
                h.matches += 1;
                let before = found.start().checked_sub(1).map(|i| line[i]);
                let after = line.get(found.end()).copied();
                if !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte) {
                    h.word += 1;
                }
                if defined.as_ref().is_some_and(|d| found.start() < d.end && d.start < found.end()) {
                    h.def += 1;
                }
                true
            })
            .map_err(|e| std::io::Error::other(format!("term matcher: {e}")))?;
        }
        Ok(true)
    }
}

/// Non-ASCII bytes count as word bytes: they belong to identifiers in the languages that
/// allow them, and splitting a UTF-8 sequence would never be a word boundary anyway. `$`
/// too: in JS and PHP `$foo` is its own identifier, not a whole-word `foo`.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Smart case decides once for the whole pattern, as rg does; the terms follow it rather
/// than each deciding again (a lowercase term of a mixed-case pattern stays sensitive).
fn resolved_case(case: CaseMode, pattern: &str) -> CaseMode {
    if case != CaseMode::Smart {
        return case;
    }
    // Escapes (`\W`, `\S`, `\D`, `\p{Lu}`) are not literal uppercase.
    let mut literal_upper = false;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        literal_upper |= c.is_uppercase();
    }
    if literal_upper { CaseMode::Sensitive } else { CaseMode::Insensitive }
}

/// The query's terms: the top-level alternation's branches, sliced from the pattern as
/// written. A leading flag group (`(?i)a|b`) belongs to the first branch syntactically
/// but applies to all, so every branch gets it.
fn split_terms(pattern: &str, fixed: bool) -> Result<Vec<String>> {
    use regex_syntax::ast::Ast;
    if fixed {
        return Ok(vec![pattern.to_string()]);
    }
    let ast = regex_syntax::ast::parse::Parser::new().parse(pattern).map_err(|e| Error::Pattern(e.to_string()))?;
    let Ast::Alternation(alt) = &ast else {
        return Ok(vec![pattern.to_string()]);
    };
    let slice = |a: &Ast| &pattern[a.span().start.offset..a.span().end.offset];
    let flags = match alt.asts.first() {
        Some(Ast::Concat(c)) => match c.asts.first() {
            Some(f @ Ast::Flags(_)) => slice(f).to_string(),
            _ => String::new(),
        },
        Some(f @ Ast::Flags(_)) => slice(f).to_string(),
        _ => String::new(),
    };
    Ok(alt
        .asts
        .iter()
        .enumerate()
        .map(|(i, a)| if i == 0 || flags.is_empty() { slice(a).to_string() } else { format!("{flags}{}", slice(a)) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_are_the_top_level_branches() {
        assert_eq!(split_terms("foo|bar(baz|qux)|\\|x", false).unwrap(), ["foo", "bar(baz|qux)", "\\|x"]);
        assert_eq!(split_terms("(?i)foo|bar", false).unwrap(), ["(?i)foo", "(?i)bar"]);
        assert_eq!(split_terms("(a|b)c", false).unwrap(), ["(a|b)c"]);
        assert_eq!(split_terms("a|b", true).unwrap(), ["a|b"]);
    }

    #[test]
    fn smart_case_resolves_once() {
        assert_eq!(resolved_case(CaseMode::Smart, "foo|Bar"), CaseMode::Sensitive);
        assert_eq!(resolved_case(CaseMode::Smart, "foo\\W+bar"), CaseMode::Insensitive);
        assert_eq!(resolved_case(CaseMode::Insensitive, "Foo"), CaseMode::Insensitive);
    }
}
