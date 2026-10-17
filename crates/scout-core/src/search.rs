//! The search engine: plan once, fan out over repos, verify candidates with ripgrep's
//! own searcher, stream results in a stable order.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use grep_printer::{Standard, StandardBuilder, Summary, SummaryBuilder, SummaryKind};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use termcolor::NoColor;

use crate::ports::{DocId, RepoSource, Shard, UnitId};
use crate::query::{PlanCase, Query, plan};
use crate::trigram::{ALWAYS_SEARCH, READER_MODE};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Sensitive,
    Insensitive,
    Smart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Lines,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Compare every indexed file's size/mtime with the source; search changed and new
    /// files from the source, drop deleted ones. Results equal a fresh rg run.
    Strict,
    /// Search the index as of the last `scout index`.
    Off,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub pattern: String,
    pub fixed_strings: bool,
    pub case: CaseMode,
    pub word: bool,
    pub line_number: bool,
    pub output: OutputMode,
    /// Per-file match limit, like rg's `-m`.
    pub max_count: Option<u64>,
    pub threads: usize,
    pub freshness: Freshness,
}

/// One repo ready to search: its source (for display paths and freshness) and its
/// opened shards.
pub struct RepoHandle {
    pub name: String,
    pub source: Box<dyn RepoSource>,
    pub shards: Vec<Box<dyn Shard>>,
    /// The snapshot recorded at index time, handed back to the source for fast listing.
    pub snapshot: Option<Vec<u8>>,
    /// `RepoIndex::listing_digest` of the opened index.
    pub listing_digest: Option<u128>,
}

#[derive(Debug, Default, Clone)]
pub struct SearchStats {
    pub repos: usize,
    pub shards: usize,
    pub docs: u64,
    /// Docs that survived the trigram filter and were verified with the regex engine.
    pub candidates: u64,
    pub matched_files: u64,
    /// Files searched from the source because they changed since indexing.
    pub dirty_files: u64,
    /// Repos whose source had to examine everything to answer (see `Listing::full_scan`).
    pub full_scans: u64,
    /// Repos whose source proved nothing changed since indexing, searched like `off`.
    pub unchanged_repos: u64,
    /// Thread time summed over repos: listing the source, building the indexed-path map.
    pub list_time: Duration,
    pub map_time: Duration,
    pub plan: String,
    pub elapsed: Duration,
}

impl SearchStats {
    fn merge(&mut self, o: &SearchStats) {
        self.shards += o.shards;
        self.docs += o.docs;
        self.candidates += o.candidates;
        self.matched_files += o.matched_files;
        self.dirty_files += o.dirty_files;
        self.full_scans += o.full_scans;
        self.unchanged_repos += o.unchanged_repos;
        self.list_time += o.list_time;
        self.map_time += o.map_time;
    }
}

/// What a search produced besides its output.
#[derive(Debug, Default)]
pub struct SearchReport {
    pub stats: SearchStats,
    /// Snapshots the sources asked to store, by index into the searched repos. Storing
    /// them is what lets the next search start from the current state.
    pub snapshots: Vec<(usize, Vec<u8>)>,
}

/// One repo's result: its output, stats and the snapshot its source wants stored.
type RepoResult = (Vec<u8>, SearchStats, Option<Vec<u8>>);

/// Runs a search over `repos`, writing rg-formatted output to `out`.
pub fn search(opts: &SearchOptions, repos: &[RepoHandle], out: &mut dyn Write) -> Result<SearchReport> {
    let started = Instant::now();
    if opts.threads == 0 {
        return Err(Error::Invalid("threads must be > 0".into()));
    }
    let pattern = if opts.fixed_strings { regex_syntax::escape(&opts.pattern) } else { opts.pattern.clone() };
    let plan_case = if opts.case == CaseMode::Sensitive { PlanCase::Sensitive } else { PlanCase::Insensitive };
    let query = plan(&pattern, plan_case)?;
    let matcher = build_matcher(&pattern, opts)?;

    let next = AtomicUsize::new(0);
    let workers = opts.threads.min(repos.len()).max(1);
    let (tx, rx) = mpsc::channel::<(usize, Result<RepoResult>)>();
    let mut stats = SearchStats { repos: repos.len(), plan: format!("{query:?}"), ..SearchStats::default() };
    let mut snapshots = Vec::new();

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, query, matcher) = (&next, &query, matcher.clone());
            scope.spawn(move || {
                let mut searcher = build_searcher(opts);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(repo) = repos.get(i) else { break };
                    let r = search_repo(repo, query, &matcher, &mut searcher, opts);
                    if tx.send((i, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Reorder buffer: print repo i only after 0..i, so output is deterministic while
        // later repos keep searching in parallel.
        let mut pending: HashMap<usize, Vec<u8>> = HashMap::new();
        let mut emit = 0usize;
        for (i, r) in rx {
            let (buf, s, snapshot) = r.map_err(|e| Error::Invalid(format!("repo {}: {e}", repos[i].name)))?;
            stats.merge(&s);
            snapshots.extend(snapshot.map(|b| (i, b)));
            pending.insert(i, buf);
            while let Some(buf) = pending.remove(&emit) {
                out.write_all(&buf).map_err(|e| Error::io("<stdout>", e))?;
                emit += 1;
            }
        }
        Ok(())
    })?;
    stats.elapsed = started.elapsed();
    Ok(SearchReport { stats, snapshots })
}

/// Mirrors rg's default (non-multiline) matcher configuration so results are identical.
fn build_matcher(pattern: &str, opts: &SearchOptions) -> Result<RegexMatcher> {
    let mut b = RegexMatcherBuilder::new();
    b.multi_line(true)
        .unicode(true)
        .octal(false)
        .line_terminator(Some(b'\n'))
        .dot_matches_new_line(false)
        .ban_byte(Some(b'\x00'))
        .word(opts.word);
    match opts.case {
        CaseMode::Sensitive => b.case_insensitive(false),
        CaseMode::Insensitive => b.case_insensitive(true),
        CaseMode::Smart => b.case_smart(true),
    };
    b.build(pattern).map_err(|e| Error::Pattern(e.to_string()))
}

fn build_searcher(opts: &SearchOptions) -> Searcher {
    SearcherBuilder::new()
        .line_number(opts.line_number)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .max_matches(opts.max_count)
        .build()
}

enum Printer {
    Lines(Standard<NoColor<Vec<u8>>>),
    Summary(Summary<NoColor<Vec<u8>>>),
}

impl Printer {
    fn new(mode: OutputMode) -> Self {
        let buf = NoColor::new(Vec::new());
        match mode {
            OutputMode::Lines => Self::Lines(StandardBuilder::new().heading(false).path(true).build(buf)),
            OutputMode::FilesWithMatches => {
                Self::Summary(SummaryBuilder::new().kind(SummaryKind::PathWithMatch).build(buf))
            }
            OutputMode::Count => Self::Summary(SummaryBuilder::new().kind(SummaryKind::Count).build(buf)),
        }
    }

    /// Searches one document; returns whether it matched.
    ///
    /// `Slice` is the fast path for indexed text. `Reader` replays rg's buffered reading
    /// (binary detection per 64 KiB fill, warning line) and is used wherever rg's exact
    /// binary behaviour matters: late-NUL docs and files read fresh from the source.
    fn search(&mut self, searcher: &mut Searcher, matcher: &RegexMatcher, doc: Doc<'_>) -> Result<bool> {
        let r = match self {
            Self::Lines(p) => {
                let mut sink = p.sink_with_path(matcher, doc.path);
                run(searcher, matcher, doc, &mut sink).map(|()| sink.has_match())
            }
            Self::Summary(p) => {
                let mut sink = p.sink_with_path(matcher, doc.path);
                run(searcher, matcher, doc, &mut sink).map(|()| sink.has_match())
            }
        };
        r.map_err(|e| Error::io(doc.path, e))
    }

    fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Lines(p) => p.into_inner().into_inner(),
            Self::Summary(p) => p.into_inner().into_inner(),
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Slice,
    Reader,
}

#[derive(Clone, Copy)]
struct Doc<'a> {
    path: &'a str,
    content: &'a [u8],
    mode: Mode,
}

fn run<S: grep_searcher::Sink<Error = std::io::Error>>(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    doc: Doc<'_>,
    sink: S,
) -> std::io::Result<()> {
    match doc.mode {
        Mode::Slice => searcher.search_slice(matcher, doc.content, sink),
        // A reader search's output depends on the line buffer's capacity: rg quits without
        // output when the *first* fill already contains the NUL, and the buffer grows on
        // long lines but never shrinks. A reused searcher would read a whole late-NUL file
        // in one fill and print nothing, where a fresh rg (64 KiB buffer) prints the lines
        // before the NUL. So each reader doc gets a clone of `searcher`, whose line buffer
        // is still pristine: slice searches never touch it (and never fall back to the
        // reader, because UTF-16 BOM docs, the only ones needing transcoding, are always
        // `Reader`). The clone copies ~72 KiB, paid only for binary/BOM/dirty docs.
        Mode::Reader => searcher.clone().search_reader(matcher, doc.content, sink),
    }
}

fn display_path(root: &str, rel: &str) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    let mut s = String::with_capacity(root.len() + rel.len() + 1);
    s.push_str(root);
    s.push(sep);
    if sep == '/' {
        s.push_str(rel);
    } else {
        s.extend(rel.chars().map(|c| if c == '/' { sep } else { c }));
    }
    s
}

/// Which indexed docs must be ignored (changed/deleted) and which source files must be
/// searched directly (changed/new), per strict freshness.
struct Overlay {
    /// What the source wants stored for next time.
    snapshot: Option<Vec<u8>>,
    masked: Vec<Vec<bool>>,
    dirty: Vec<String>,
}

/// The fast path of strict freshness: a repo whose current listing provably equals the
/// indexed one needs no overlay. Skips building the indexed-path map and the listing,
/// which are most of strict mode's cost for repos nobody touched.
fn unchanged(repo: &RepoHandle, stats: &mut SearchStats) -> Result<bool> {
    let (Some(snapshot), Some(digest)) = (&repo.snapshot, repo.listing_digest) else {
        return Ok(false);
    };
    let t = Instant::now();
    let same = repo.source.unchanged(snapshot, digest)?;
    stats.list_time += t.elapsed();
    stats.unchanged_repos += u64::from(same);
    Ok(same)
}

fn overlay(repo: &RepoHandle, stats: &mut SearchStats) -> Result<Overlay> {
    let t = Instant::now();
    let mut masked: Vec<Vec<bool>> = repo.shards.iter().map(|s| vec![true; s.doc_count() as usize]).collect();
    // Keys borrow from the shard (an mmap for the native backend): no per-doc allocation.
    let mut indexed: HashMap<std::borrow::Cow<'_, str>, (usize, DocId, u64, u64)> = HashMap::new();
    for (si, shard) in repo.shards.iter().enumerate() {
        for doc in 0..shard.doc_count() {
            let m = shard.meta(doc)?;
            indexed.insert(m.path, (si, doc, m.size, m.mtime_ns));
        }
    }
    stats.map_time += t.elapsed();
    let t = Instant::now();
    let listing = repo.source.list(repo.snapshot.as_deref())?;
    stats.full_scans += u64::from(listing.full_scan);
    let listed = listing.files;
    stats.list_time += t.elapsed();
    let mut dirty = Vec::new();
    for f in listed {
        match indexed.get(f.path.as_str()) {
            Some(&(si, doc, size, mtime)) if size == f.size && mtime == f.mtime_ns => {
                masked[si][doc as usize] = false;
            }
            // Changed (or binary-skipped) file: search the current version from source.
            _ => dirty.push(f.path),
        }
    }
    Ok(Overlay { snapshot: listing.snapshot, masked, dirty })
}

fn search_repo(
    repo: &RepoHandle,
    query: &Query,
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    opts: &SearchOptions,
) -> Result<RepoResult> {
    let mut stats = SearchStats { shards: repo.shards.len(), ..SearchStats::default() };
    let mut printer = Printer::new(opts.output);
    let root = repo.source.display_root();
    let mut overlay = match opts.freshness {
        Freshness::Strict if unchanged(repo, &mut stats)? => None,
        Freshness::Strict => Some(overlay(repo, &mut stats)?),
        Freshness::Off => None,
    };

    for (si, shard) in repo.shards.iter().enumerate() {
        stats.docs += u64::from(shard.doc_count());
        let always = to_docs(shard.as_ref(), &shard.postings(ALWAYS_SEARCH)?)?;
        let reader = to_docs(shard.as_ref(), &shard.postings(READER_MODE)?)?;
        let candidates = match candidates(shard.as_ref(), query)? {
            Some(units) => union(&to_docs(shard.as_ref(), &units)?, &always),
            None => (0..shard.doc_count()).collect(),
        };
        for doc in candidates {
            if overlay.as_ref().is_some_and(|o| o.masked[si][doc as usize]) {
                continue;
            }
            stats.candidates += 1;
            let meta = shard.meta(doc)?;
            let content = shard.content(doc)?;
            let mode = if reader.binary_search(&doc).is_ok() { Mode::Reader } else { Mode::Slice };
            let path = display_path(root, &meta.path);
            if printer.search(searcher, matcher, Doc { path: &path, content: &content, mode })? {
                stats.matched_files += 1;
            }
        }
    }
    if let Some(o) = &overlay {
        for rel in &o.dirty {
            stats.dirty_files += 1;
            let content = repo.source.read(rel)?;
            let path = display_path(root, rel);
            if printer.search(searcher, matcher, Doc { path: &path, content: &content, mode: Mode::Reader })? {
                stats.matched_files += 1;
            }
        }
    }
    let snapshot = overlay.as_mut().and_then(|o| o.snapshot.take());
    Ok((printer.into_bytes(), stats, snapshot))
}

/// Maps sorted unit ids to the sorted, distinct docs they belong to. A doc's units are
/// contiguous, so duplicates are always adjacent.
fn to_docs(shard: &dyn Shard, units: &[UnitId]) -> Result<Vec<DocId>> {
    let mut docs: Vec<DocId> = Vec::with_capacity(units.len());
    for &u in units {
        let d = shard.unit_doc(u)?;
        if docs.last() != Some(&d) {
            docs.push(d);
        }
    }
    Ok(docs)
}

/// Evaluates `query` against a shard's units. `None` means "every unit".
pub fn candidates(shard: &dyn Shard, query: &Query) -> Result<Option<Vec<UnitId>>> {
    match query {
        Query::All => Ok(None),
        Query::Tri(t) => shard.postings(*t).map(Some),
        Query::And(parts) => {
            let mut lists = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(l) = candidates(shard, p)? {
                    if l.is_empty() {
                        return Ok(Some(l));
                    }
                    lists.push(l);
                }
            }
            lists.sort_unstable_by_key(Vec::len);
            let mut it = lists.into_iter();
            let Some(first) = it.next() else {
                return Ok(None);
            };
            Ok(Some(it.fold(first, |acc, l| intersect(&acc, &l))))
        }
        Query::Or(parts) => {
            let mut acc: Vec<UnitId> = Vec::new();
            for p in parts {
                match candidates(shard, p)? {
                    None => return Ok(None),
                    Some(l) => acc = union(&acc, &l),
                }
            }
            Ok(Some(acc))
        }
    }
}

/// Set operations on sorted id lists; used for both unit and doc ids.
fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j, mut out) = (0, 0, Vec::with_capacity(a.len().min(b.len())));
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j, mut out) = (0, 0, Vec::with_capacity(a.len() + b.len()));
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_ops() {
        assert_eq!(intersect(&[1, 3, 5, 7], &[3, 4, 7]), vec![3, 7]);
        assert_eq!(union(&[1, 5], &[2, 5, 9]), vec![1, 2, 5, 9]);
    }

    fn lines_output(searcher: &mut Searcher, matcher: &RegexMatcher, content: &[u8]) -> String {
        let mut printer = Printer::new(OutputMode::Lines);
        printer.search(searcher, matcher, Doc { path: "f", content, mode: Mode::Reader }).unwrap();
        String::from_utf8(printer.into_bytes()).unwrap()
    }

    /// Regression: a late-NUL doc searched after a doc with a huge line used to print
    /// nothing, because the shared line buffer had grown past the NUL offset.
    #[test]
    fn reader_output_does_not_depend_on_previous_docs() {
        let opts = SearchOptions {
            pattern: "needle".into(),
            fixed_strings: false,
            case: CaseMode::Sensitive,
            word: false,
            line_number: true,
            max_count: None,
            output: OutputMode::Lines,
            freshness: Freshness::Off,
            threads: 1,
        };
        let matcher = build_matcher(&opts.pattern, &opts).unwrap();
        // NUL past the first 64 KiB fill, so a fresh searcher reports the earlier match.
        let mut late_nul = b"needle\n".to_vec();
        late_nul.extend(std::iter::repeat_n(b'x', 100 * 1024));
        late_nul.extend(b"\n\0");
        let expected = lines_output(&mut build_searcher(&opts), &matcher, &late_nul);
        assert!(expected.contains("f:1:needle"), "{expected}");

        let mut searcher = build_searcher(&opts);
        let huge_line = vec![b'y'; 4 << 20];
        lines_output(&mut searcher, &matcher, &huge_line);
        assert_eq!(lines_output(&mut searcher, &matcher, &late_nul), expected);
    }
}
