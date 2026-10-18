//! The search engine: plan once, find each repo's candidates, verify them in chunks spread
//! over every thread with ripgrep's own searcher, stream results in a stable order.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use grep_printer::{Standard, StandardBuilder, Summary, SummaryBuilder, SummaryKind};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use termcolor::NoColor;

use crate::ports::{DocId, RepoSource, Shard, UnitId};
use crate::query::{PlanCase, Query, plan};
use crate::trigram::Trigram;
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
    pub limits: Limits,
}

/// Bounds on one search, for callers answering queries they do not control (the server).
/// Checked after every document, so a search stops within one document of crossing
/// either, before its output is buffered whole. The CLI sets none: it streams to a pipe.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    /// Output bytes across all repos, counted as produced, not as written.
    pub output_bytes: Option<usize>,
    pub deadline: Option<Instant>,
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
    /// Shards whose trigram filter rejected the query, so planning read none of their
    /// dictionary or postings (E24); their always-searched docs are still verified.
    pub shards_filtered: usize,
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
    /// Thread time summed: reading postings into candidates, and verifying candidates
    /// (reading content, matching, printing). Shows which side of the index a query pays.
    pub postings_time: Duration,
    pub verify_time: Duration,
    pub plan: String,
    pub elapsed: Duration,
}

impl SearchStats {
    pub(crate) fn merge(&mut self, o: &SearchStats) {
        self.shards += o.shards;
        self.shards_filtered += o.shards_filtered;
        self.docs += o.docs;
        self.candidates += o.candidates;
        self.matched_files += o.matched_files;
        self.dirty_files += o.dirty_files;
        self.full_scans += o.full_scans;
        self.unchanged_repos += o.unchanged_repos;
        self.list_time += o.list_time;
        self.map_time += o.map_time;
        self.postings_time += o.postings_time;
        self.verify_time += o.verify_time;
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

/// Verification is split into chunks of about this many candidate bytes (by file size),
/// across repos and within them. Splitting only by repo let one large repo bound the whole
/// search: in the bench corpus one repo prints 60% of a broad query's lines, and every core
/// past the third sat idle behind it (E16). 4 MiB splits the largest shards (~90 MiB) many
/// ways while the per-chunk costs (a printer buffer, a channel message, a reorder entry)
/// stay noise against verifying 4 MiB.
const CHUNK_BYTES: u64 = 4 << 20;

/// One unit of verification work: a run of one shard's candidates, or a repo's changed files.
struct Chunk {
    repo: usize,
    part: Part,
}

enum Part {
    Docs { shard: usize, range: std::ops::Range<usize> },
    Dirty,
}

/// Runs a search over `repos`, writing rg-formatted output to `out`. Borrowed handles, so
/// a server can search any subset of the repos it keeps open.
pub fn search(opts: &SearchOptions, repos: &[&RepoHandle], out: &mut dyn Write) -> Result<SearchReport> {
    search_chunked(opts, repos, out, CHUNK_BYTES)
}

/// Limits are the caller's to report as such; anything else names the repo it came from.
pub(crate) fn in_repo(repo: &RepoHandle, e: Error) -> Error {
    match e {
        Error::OutputLimit(_) | Error::Deadline => e,
        // Wrapped, not flattened to a string, so the cause (an I/O error, a corrupt shard)
        // stays reachable through `source()`.
        e => Error::backend(format!("repo {}", repo.name), e),
    }
}

/// [`search`] with the chunk size as a parameter, so a test can prove the output does not
/// depend on it.
fn search_chunked(
    opts: &SearchOptions,
    repos: &[&RepoHandle],
    out: &mut dyn Write,
    chunk_bytes: u64,
) -> Result<SearchReport> {
    let started = Instant::now();
    if opts.threads == 0 {
        return Err(Error::Invalid("threads must be > 0".into()));
    }
    if chunk_bytes == 0 {
        return Err(Error::Invalid("chunk size must be > 0".into()));
    }
    let pattern = if opts.fixed_strings { regex_syntax::escape(&opts.pattern) } else { opts.pattern.clone() };
    let plan_case = if opts.case == CaseMode::Sensitive { PlanCase::Sensitive } else { PlanCase::Insensitive };
    let query = plan(&pattern, plan_case)?;
    let matcher = build_matcher(&pattern, opts)?;
    let mut stats = SearchStats { repos: repos.len(), plan: format!("{query:?}"), ..SearchStats::default() };
    // One thread verifies chunks in order whatever their size, so finding boundaries (a
    // size lookup per candidate) would be pure cost; 1-CPU servers are the target.
    let chunk_bytes = if opts.threads == 1 { u64::MAX } else { chunk_bytes };

    // Phase 1, in parallel over repos: freshness overlay, candidates, and their chunks.
    // Cheap next to verifying (postings, plus the listing in strict mode), and the chunk
    // boundaries need every candidate's size, so it is done here rather than serially.
    let planned = parallel_indexed(opts.threads, repos.len(), |i| {
        let repo = repos[i];
        let mut s = SearchStats { shards: repo.shards.len(), ..SearchStats::default() };
        let plan = plan_repo(repo, &query, opts, &mut s).map_err(|e| in_repo(repo, e))?;
        let chunks = chunk_repo(repo, &plan, chunk_bytes).map_err(|e| in_repo(repo, e))?;
        Ok((plan, s, chunks))
    })?;
    let mut plans = Vec::with_capacity(repos.len());
    let mut chunks = Vec::new();
    let mut snapshots = Vec::new();
    for (i, (mut plan, s, parts)) in planned.into_iter().enumerate() {
        stats.merge(&s);
        snapshots.extend(plan.snapshot.take().map(|b| (i, b)));
        chunks.extend(parts.into_iter().map(|part| Chunk { repo: i, part }));
        plans.push(plan);
    }

    // Phase 2: verify chunks on every thread; a reorder buffer prints chunk k only after
    // 0..k, so output is the same as verifying everything in order on one thread.
    let phase2 = Verify {
        opts,
        repos,
        plans: &plans,
        chunks: &chunks,
        matcher: &matcher,
        // Shared across chunks: each chunk's output is buffered until its turn, so only a
        // corpus-wide count bounds what a search holds in memory.
        produced: AtomicUsize::new(0),
    };
    // A selective query has a few chunks: spawning a searcher per core would cost more
    // than it verifies.
    let workers = opts.threads.min(chunks.len());
    if workers <= 1 {
        phase2.inline(out, &mut stats)?;
    } else {
        phase2.parallel(workers, out, &mut stats)?;
    }
    stats.elapsed = started.elapsed();
    Ok(SearchReport { stats, snapshots })
}

/// Phase 2 of a search: the planned chunks and what verifying one needs.
struct Verify<'a> {
    opts: &'a SearchOptions,
    repos: &'a [&'a RepoHandle],
    plans: &'a [RepoPlan],
    chunks: &'a [Chunk],
    matcher: &'a RegexMatcher,
    produced: AtomicUsize,
}

impl Verify<'_> {
    fn verify(&self, chunk: &Chunk, matcher: &RegexMatcher, searcher: &mut Searcher) -> Result<(Vec<u8>, SearchStats)> {
        let repo = self.repos[chunk.repo];
        search_chunk(repo, &self.plans[chunk.repo], &chunk.part, matcher, searcher, self.opts, &self.produced)
            .map_err(|e| in_repo(repo, e))
    }

    /// No thread to spawn and nothing to reorder: verify in order on this thread. In a
    /// server a 5 ms search would otherwise pay a spawn per phase (E16).
    fn inline(&self, out: &mut dyn Write, stats: &mut SearchStats) -> Result<()> {
        let mut searcher = build_searcher(self.opts);
        for chunk in self.chunks {
            let (buf, s) = self.verify(chunk, self.matcher, &mut searcher)?;
            stats.merge(&s);
            out.write_all(&buf).map_err(|e| Error::io("<stdout>", e))?;
        }
        Ok(())
    }

    fn parallel(&self, workers: usize, out: &mut dyn Write, stats: &mut SearchStats) -> Result<()> {
        let next = AtomicUsize::new(0);
        let flow = Mutex::new(Flow { emit: 0, buffered: 0, done: false });
        let advanced = Condvar::new();
        // Set on the first error: no point claiming more chunks.
        let stop = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<(usize, Result<(Vec<u8>, SearchStats)>)>();
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let tx = tx.clone();
                let (next, matcher) = (&next, self.matcher.clone());
                let (flow, advanced, stop) = (&flow, &advanced, &stop);
                scope.spawn(move || {
                    // A worker that panics never sends its chunk, so the others would wait
                    // on the buffer forever, holding the channel open, and the scope would
                    // never join to re-raise the panic. Its guard releases them.
                    let _guard = ReleaseOnPanic { stop, flow, advanced };
                    let mut searcher = build_searcher(self.opts);
                    while !stop.load(Ordering::Relaxed) {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = self.chunks.get(k) else { break };
                        {
                            let mut f = flow.lock().unwrap_or_else(PoisonError::into_inner);
                            // The next chunk to print always runs, so the printer always
                            // progresses; so does every chunk before a failing one, which is
                            // what reports errors in chunk order.
                            while k != f.emit && f.buffered >= REORDER_BYTES && !f.done {
                                f = advanced.wait(f).unwrap_or_else(PoisonError::into_inner);
                            }
                            if f.done {
                                return;
                            }
                        }
                        let r = self.verify(chunk, &matcher, &mut searcher);
                        if let Ok((buf, _)) = &r {
                            flow.lock().unwrap_or_else(PoisonError::into_inner).buffered += buf.len();
                        }
                        if tx.send((k, r)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut pending: HashMap<usize, Result<(Vec<u8>, SearchStats)>> = HashMap::new();
            let mut emit = 0usize;
            // Results arrive in any order but are consumed in chunk order, errors included, so
            // the error a search reports does not depend on scheduling.
            let printed = (|| -> Result<()> {
                for (k, r) in rx {
                    if r.is_err() {
                        stop.store(true, Ordering::Relaxed);
                    }
                    pending.insert(k, r);
                    while let Some(r) = pending.remove(&emit) {
                        let (buf, s) = r?;
                        stats.merge(&s);
                        out.write_all(&buf).map_err(|e| Error::io("<stdout>", e))?;
                        emit += 1;
                        let mut f = flow.lock().unwrap_or_else(PoisonError::into_inner);
                        f.emit = emit;
                        f.buffered -= buf.len();
                        advanced.notify_all();
                    }
                }
                Ok(())
            })();
            // Whatever happened, nothing more will be printed: wake the workers waiting on
            // the buffer so they exit and the scope can join.
            release(&stop, &flow, &advanced);
            printed
        })
    }
}

/// Output verified but not yet printed, past which workers wait instead of starting a chunk
/// ahead of the printer. Without a bound one slow chunk lets the others buffer the rest of
/// the output: all of it for a CLI search without `--max-output`, on a box that may have
/// 2 GiB. Bytes, not chunks: a chunk-count window stalled broad queries whose output is
/// small (E18: `date_regex` +22–25% at 4–12 CPUs with 4 chunks per worker). Past the cap a
/// search holds at most one more chunk's output per worker.
const REORDER_BYTES: usize = 64 << 20;

/// The printer's progress, shared with the workers.
struct Flow {
    /// The next chunk to print.
    emit: usize,
    /// Bytes of verified chunks not printed yet.
    buffered: usize,
    /// The printer has returned: nothing more will be consumed.
    done: bool,
}

fn release(stop: &AtomicBool, flow: &Mutex<Flow>, advanced: &Condvar) {
    stop.store(true, Ordering::Relaxed);
    flow.lock().unwrap_or_else(PoisonError::into_inner).done = true;
    advanced.notify_all();
}

/// Wakes the workers waiting on the buffer when a worker unwinds. Only then: a worker that
/// ran out of chunks must not stop the others, whose chunks still need doing.
struct ReleaseOnPanic<'a> {
    stop: &'a AtomicBool,
    flow: &'a Mutex<Flow>,
    advanced: &'a Condvar,
}

impl Drop for ReleaseOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            release(self.stop, self.flow, self.advanced);
        }
    }
}

/// Runs `f(0..n)` on up to `threads` threads; results in index order. The first error by
/// index wins, so a failure reads the same whatever the scheduling.
fn parallel_indexed<T: Send>(threads: usize, n: usize, f: impl Fn(usize) -> Result<T> + Sync) -> Result<Vec<T>> {
    if threads.min(n) <= 1 {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut slots: Vec<Option<Result<T>>> = (0..n).map(|_| None).collect();
    let (tx, rx) = mpsc::channel::<(usize, Result<T>)>();
    std::thread::scope(|scope| {
        for _ in 0..threads.min(n) {
            let (tx, next, f) = (tx.clone(), &next, &f);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n || tx.send((i, f(i))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, r) in rx {
            slots[i] = Some(r);
        }
    });
    // Every index was claimed and sent: `scope` re-raises a worker's panic before this.
    slots.into_iter().map(|r| r.expect("every index has a result")).collect()
}

/// Cuts a repo's candidates into runs of about `chunk_bytes` (file sizes; a binary doc's
/// stored prefix is smaller, which only makes its chunk cheaper), shard by shard, then its
/// changed files as one more chunk.
/// `u64::MAX` means one chunk per shard, without looking at any size.
fn chunk_repo(repo: &RepoHandle, plan: &RepoPlan, chunk_bytes: u64) -> Result<Vec<Part>> {
    let mut parts = Vec::new();
    for (si, docs) in plan.candidates.iter().enumerate() {
        if chunk_bytes == u64::MAX {
            if !docs.is_empty() {
                parts.push(Part::Docs { shard: si, range: 0..docs.len() });
            }
            continue;
        }
        let shard = repo.shards[si].as_ref();
        let (mut start, mut bytes) = (0usize, 0u64);
        for (k, &doc) in docs.iter().enumerate() {
            // An empty file still costs a visit; count it as one byte.
            bytes += shard.meta(doc)?.size.max(1);
            if bytes >= chunk_bytes {
                parts.push(Part::Docs { shard: si, range: start..k + 1 });
                (start, bytes) = (k + 1, 0);
            }
        }
        if start < docs.len() {
            parts.push(Part::Docs { shard: si, range: start..docs.len() });
        }
    }
    if !plan.dirty.is_empty() {
        parts.push(Part::Dirty);
    }
    Ok(parts)
}

/// Mirrors rg's default (non-multiline) matcher configuration so results are identical.
pub(crate) fn build_matcher(pattern: &str, opts: &SearchOptions) -> Result<RegexMatcher> {
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

pub(crate) fn build_searcher(opts: &SearchOptions) -> Searcher {
    SearcherBuilder::new()
        .line_number(opts.line_number)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .max_matches(opts.max_count)
        .build()
}

pub(crate) enum Printer {
    Lines(Standard<NoColor<Vec<u8>>>),
    Summary(Summary<NoColor<Vec<u8>>>),
}

impl Printer {
    pub(crate) fn new(mode: OutputMode) -> Self {
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
    pub(crate) fn search(&mut self, searcher: &mut Searcher, matcher: &RegexMatcher, doc: Doc<'_>) -> Result<bool> {
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

    // grep_printer exposes the writer only mutably.
    pub(crate) fn len(&mut self) -> usize {
        match self {
            Self::Lines(p) => p.get_mut().get_ref().len(),
            Self::Summary(p) => p.get_mut().get_ref().len(),
        }
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Lines(p) => p.into_inner().into_inner(),
            Self::Summary(p) => p.into_inner().into_inner(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Mode {
    Slice,
    Reader,
}

#[derive(Clone, Copy)]
pub(crate) struct Doc<'a> {
    pub(crate) path: &'a str,
    pub(crate) content: &'a [u8],
    pub(crate) mode: Mode,
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

/// Where a visited document's bytes come from, so a second pass can read it again.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Loc<'a> {
    Indexed {
        shard: usize,
        doc: DocId,
        mode: Mode,
    },
    /// Changed since indexing: read from the source.
    Dirty(&'a str),
}

/// Every document one repo's search must verify, decided before any is read: each shard's
/// candidates (strict freshness's masked docs removed) and the files changed since indexing.
pub(crate) struct RepoPlan {
    /// Per shard, in doc order.
    candidates: Vec<Vec<DocId>>,
    /// Searched from the source, after every shard.
    dirty: Vec<String>,
    /// What the source wants stored for next time.
    snapshot: Option<Vec<u8>>,
}

pub(crate) fn plan_repo(
    repo: &RepoHandle,
    query: &Query,
    opts: &SearchOptions,
    stats: &mut SearchStats,
) -> Result<RepoPlan> {
    let overlay = match opts.freshness {
        Freshness::Strict if unchanged(repo, stats)? => None,
        Freshness::Strict => Some(overlay(repo, stats)?),
        Freshness::Off => None,
    };
    let mut plan = RepoPlan { candidates: Vec::new(), dirty: Vec::new(), snapshot: None };
    let t = Instant::now();
    for (si, shard) in repo.shards.iter().enumerate() {
        stats.docs += u64::from(shard.doc_count());
        let (units, filtered) = shard_units(shard.as_ref(), query)?;
        stats.shards_filtered += usize::from(filtered);
        // A filtered shard still verifies its always-searched docs: their matches are not
        // described by the shard's trigrams, so no filter can rule them out.
        let mut docs = match units {
            Some(units) => union(&to_docs(shard.as_ref(), &units)?, &shard.markers().always_search),
            None => (0..shard.doc_count()).collect(),
        };
        if let Some(o) = &overlay {
            docs.retain(|&d| !o.masked[si][d as usize]);
        }
        stats.candidates += docs.len() as u64;
        plan.candidates.push(docs);
    }
    stats.postings_time += t.elapsed();
    if let Some(o) = overlay {
        plan.dirty = o.dirty;
        plan.snapshot = o.snapshot;
    }
    Ok(plan)
}

/// Verifies `docs`, a run of shard `si`'s planned candidates, in order. `visit` returns
/// whether the document matched.
pub(crate) fn visit_docs<F>(
    repo: &RepoHandle,
    si: usize,
    docs: &[DocId],
    stats: &mut SearchStats,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(Loc<'_>, &str, Doc<'_>) -> Result<bool>,
{
    let root = repo.source.display_root();
    let shard = repo.shards[si].as_ref();
    for &doc in docs {
        let meta = shard.meta(doc)?;
        let content = shard.content(doc)?;
        // The shard's marker list, sorted: the docs rg reads through its reader (binary, BOM).
        let mode = if shard.markers().reader_mode.binary_search(&doc).is_ok() { Mode::Reader } else { Mode::Slice };
        let path = display_path(root, &meta.path);
        let loc = Loc::Indexed { shard: si, doc, mode };
        if visit(loc, &meta.path, Doc { path: &path, content: &content, mode })? {
            stats.matched_files += 1;
        }
    }
    Ok(())
}

/// Verifies the files strict freshness found changed, read from the source.
pub(crate) fn visit_dirty<F>(repo: &RepoHandle, plan: &RepoPlan, stats: &mut SearchStats, visit: &mut F) -> Result<()>
where
    F: FnMut(Loc<'_>, &str, Doc<'_>) -> Result<bool>,
{
    let root = repo.source.display_root();
    for rel in &plan.dirty {
        stats.dirty_files += 1;
        let content = repo.source.read(rel)?;
        let path = display_path(root, rel);
        if visit(Loc::Dirty(rel), rel, Doc { path: &path, content: &content, mode: Mode::Reader })? {
            stats.matched_files += 1;
        }
    }
    Ok(())
}

/// Visits every document of `repo` the search must verify, in the same order as the
/// chunked search prints them, on the calling thread. Returns the snapshot the source wants
/// stored. The ranked search uses it, so both verify the same documents.
pub(crate) fn walk_repo(
    repo: &RepoHandle,
    query: &Query,
    opts: &SearchOptions,
    stats: &mut SearchStats,
    mut visit: impl FnMut(Loc<'_>, &str, Doc<'_>) -> Result<bool>,
) -> Result<Option<Vec<u8>>> {
    let mut plan = plan_repo(repo, query, opts, stats)?;
    for si in 0..plan.candidates.len() {
        visit_docs(repo, si, &plan.candidates[si], stats, &mut visit)?;
    }
    visit_dirty(repo, &plan, stats, &mut visit)?;
    Ok(plan.snapshot.take())
}

fn search_chunk(
    repo: &RepoHandle,
    plan: &RepoPlan,
    part: &Part,
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    opts: &SearchOptions,
    produced: &AtomicUsize,
) -> Result<(Vec<u8>, SearchStats)> {
    let mut counted = 0usize;
    // Adds what this chunk printed since the last call to the shared count, then checks
    // both limits.
    let bounded = opts.limits.output_bytes.is_some() || opts.limits.deadline.is_some();
    let mut check = |printer: &mut Printer| -> Result<()> {
        if !bounded {
            return Ok(());
        }
        let len = printer.len();
        // The buffer only grows, so the difference never underflows.
        let grown = len - counted;
        let total = if grown == 0 {
            produced.load(Ordering::Relaxed)
        } else {
            produced.fetch_add(grown, Ordering::Relaxed) + grown
        };
        counted = len;
        if let Some(cap) = opts.limits.output_bytes
            && total > cap
        {
            return Err(Error::OutputLimit(cap));
        }
        check_deadline(opts)
    };
    let mut stats = SearchStats::default();
    let t = Instant::now();
    let mut printer = Printer::new(opts.output);
    {
        let mut visit = |_: Loc<'_>, _: &str, doc: Doc<'_>| -> Result<bool> {
            let hit = printer.search(searcher, matcher, doc)?;
            check(&mut printer)?;
            Ok(hit)
        };
        match part {
            Part::Docs { shard, range } => {
                visit_docs(repo, *shard, &plan.candidates[*shard][range.clone()], &mut stats, &mut visit)?;
            }
            Part::Dirty => visit_dirty(repo, plan, &mut stats, &mut visit)?,
        }
    }
    stats.verify_time = t.elapsed();
    Ok((printer.into_bytes(), stats))
}

pub(crate) fn check_deadline(opts: &SearchOptions) -> Result<()> {
    if opts.limits.deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(Error::Deadline);
    }
    Ok(())
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

/// Whether the shard's trigram filter lets `query` through (E24): `false` means no unit can
/// match, decided without reading the shard's dictionary or postings.
fn may_match(shard: &dyn Shard, query: &Query) -> bool {
    match query {
        Query::All => true,
        Query::Tri(t) => shard.may_contain(*t),
        Query::And(parts) => parts.iter().all(|p| may_match(shard, p)),
        Query::Or(parts) => parts.iter().any(|p| may_match(shard, p)),
    }
}

/// A shard's candidate units for `query` (`None`: every unit), and whether its filter
/// rejected the whole query, in which case none of its dictionary or postings was read.
/// The filter is asked about the whole tree before anything is counted or decoded (E24):
/// an AND's parts are read one at a time, so a part it rejects would otherwise be reached
/// only after the ones before it had read the shard.
fn shard_units(shard: &dyn Shard, query: &Query) -> Result<(Option<Vec<UnitId>>, bool)> {
    if !may_match(shard, query) {
        return Ok((Some(Vec::new()), true));
    }
    Ok((candidates(shard, query)?, false))
}

/// Evaluates `query` against a shard's units, once its filter has let `query` through
/// ([`shard_units`]). `None` means "every unit".
fn candidates(shard: &dyn Shard, query: &Query) -> Result<Option<Vec<UnitId>>> {
    match query {
        Query::All => Ok(None),
        Query::Tri(t) => shard.postings(*t).map(Some),
        // The filter let the whole AND through, so it let every part through too: nothing
        // left to ask it here.
        Query::And(parts) => {
            let mut lists = Vec::with_capacity(parts.len());
            let mut tris: Vec<Trigram> = Vec::new();
            for p in parts {
                if let Query::Tri(t) = p {
                    tris.push(*t);
                    continue;
                }
                if let Some(l) = candidates(shard, p)? {
                    if l.is_empty() {
                        return Ok(Some(l));
                    }
                    lists.push(l);
                }
            }
            let Some(counted) = count_trigrams(shard, &tris)? else {
                return Ok(Some(Vec::new()));
            };
            let mut rest = counted.iter();
            let mut spent = 0u64;
            if let Some(&(n, t)) = rest.next() {
                lists.push(shard.postings(t)?);
                spent = u64::from(n) + LIST_COST_POSTINGS;
            }
            lists.sort_unstable_by_key(Vec::len);
            let mut it = lists.into_iter();
            let Some(first) = it.next() else {
                return Ok(None);
            };
            let mut acc = it.fold(first, |acc, l| intersect(&acc, &l));
            // Rarest first, a further list is decoded only while all decoding so far, it
            // included, stays under the price of verifying every current candidate (E22):
            // once few candidates remain, reading a long list to drop some of them costs
            // more than checking them. Counts only grow along the walk and the candidates
            // only shrink, so the first list over budget ends it.
            for &(n, t) in rest {
                let cost = u64::from(n) + LIST_COST_POSTINGS;
                if acc.is_empty() || spent + cost > acc.len() as u64 * VERIFY_COST_POSTINGS {
                    break;
                }
                acc = intersect(&acc, &shard.postings(t)?);
                spent += cost;
            }
            Ok(Some(acc))
        }
        Query::Or(parts) => {
            let mut acc: Vec<UnitId> = Vec::new();
            for p in parts {
                // The OR passed when one branch did; a branch the filter rejects adds no
                // units, so it is not read.
                if !may_match(shard, p) {
                    continue;
                }
                match candidates(shard, p)? {
                    None => return Ok(None),
                    Some(l) => acc = union(&acc, &l),
                }
            }
            Ok(Some(acc))
        }
    }
}

/// Decode budget for an AND's trigram lists (E22), in postings decoded: one more candidate
/// unit to verify is priced as `VERIFY_COST_POSTINGS` decoded and intersected postings, and
/// every list costs `LIST_COST_POSTINGS` on top (its lookup, allocation and first page).
/// Leaving a list out keeps the candidates a superset of the true ones, so verification
/// keeps the result exact. The price is measured, not derived, and only two values were
/// tried: 4096 and 16384 measured the same overall, and 4096 kept more of the
/// selective-query win at 1 CPU (lower values are untested). A fixed two rarest lists
/// (zoekt's rule, without its positional check) lost 4-5%.
const VERIFY_COST_POSTINGS: u64 = 4096;
const LIST_COST_POSTINGS: u64 = 512;

/// `tris` with their unit counts, rarest first; `None` when one is absent from the shard,
/// which empties the AND without decoding anything.
fn count_trigrams(shard: &dyn Shard, tris: &[Trigram]) -> Result<Option<Vec<(u32, Trigram)>>> {
    let mut counted = Vec::with_capacity(tris.len());
    for &t in tris {
        let n = shard.count(t)?;
        if n == 0 {
            return Ok(None);
        }
        counted.push((n, t));
    }
    counted.sort_unstable();
    Ok(Some(counted))
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

    /// An in-memory shard where every unit is a candidate for every trigram: the regex
    /// alone decides what matches, which is all the chunking test needs.
    struct MemShard(Vec<(String, Vec<u8>)>);

    static NO_MARKERS: crate::ports::Markers =
        crate::ports::Markers { always_search: Vec::new(), reader_mode: Vec::new() };

    impl Shard for MemShard {
        fn doc_count(&self) -> u32 {
            u32::try_from(self.0.len()).unwrap()
        }
        fn meta(&self, doc: DocId) -> Result<crate::ports::DocMeta<'_>> {
            let (path, content) = &self.0[doc as usize];
            Ok(crate::ports::DocMeta { path: path.as_str().into(), size: content.len() as u64, mtime_ns: 0 })
        }
        fn postings(&self, _: crate::trigram::Trigram) -> Result<Vec<UnitId>> {
            Ok((0..self.doc_count()).collect())
        }
        fn count(&self, t: crate::trigram::Trigram) -> Result<u32> {
            Ok(u32::try_from(self.postings(t)?.len()).unwrap())
        }
        fn may_contain(&self, _: crate::trigram::Trigram) -> bool {
            true
        }
        fn markers(&self) -> &crate::ports::Markers {
            &NO_MARKERS
        }
        fn unit_doc(&self, unit: UnitId) -> Result<DocId> {
            Ok(unit)
        }
        /// A doc named `fail-*` cannot be read, to test which error a search reports.
        fn content(&self, doc: DocId) -> Result<std::borrow::Cow<'_, [u8]>> {
            let (path, content) = &self.0[doc as usize];
            if path.starts_with("fail-") {
                return Err(Error::Corrupt { shard: "mem".into(), reason: path.clone() });
            }
            Ok(content.as_slice().into())
        }
    }

    /// Freshness off never lists or reads the source; only its display root is used.
    struct MemSource(String);

    impl RepoSource for MemSource {
        fn display_root(&self) -> &str {
            &self.0
        }
        fn list(&self, _: Option<&[u8]>) -> Result<crate::ports::Listing> {
            Err(Error::Invalid("not listed with freshness off".into()))
        }
        fn unchanged(&self, _: &[u8], _: u128) -> Result<bool> {
            Err(Error::Invalid("not checked with freshness off".into()))
        }
        fn read(&self, _: &str) -> Result<Vec<u8>> {
            Err(Error::Invalid("not read with freshness off".into()))
        }
    }

    #[test]
    fn output_does_not_depend_on_chunking_or_threads() {
        // Three repos of two shards; docs of varied sizes so chunk boundaries land
        // everywhere, some without a match.
        let repos: Vec<RepoHandle> = (0..3)
            .map(|r| RepoHandle {
                name: format!("r{r}"),
                source: Box::new(MemSource(format!("/r{r}"))),
                shards: (0..2)
                    .map(|s| {
                        let docs = (0..7)
                            .map(|d| {
                                let body = format!("needle {r}.{s}.{d}\n").repeat(d * 3) + "hay\n";
                                (format!("s{s}/d{d}.txt"), body.into_bytes())
                            })
                            .collect();
                        Box::new(MemShard(docs)) as Box<dyn Shard>
                    })
                    .collect(),
                snapshot: None,
                listing_digest: None,
            })
            .collect();
        let handles: Vec<&RepoHandle> = repos.iter().collect();
        let run = |threads: usize, chunk: u64, output: OutputMode| {
            let opts = SearchOptions {
                pattern: "needle".into(),
                fixed_strings: true,
                case: CaseMode::Sensitive,
                word: false,
                line_number: true,
                output,
                max_count: None,
                threads,
                freshness: Freshness::Off,
                limits: Limits::default(),
            };
            let mut out = Vec::new();
            let report = search_chunked(&opts, &handles, &mut out, chunk).unwrap();
            (String::from_utf8(out).unwrap(), report.stats.candidates, report.stats.matched_files)
        };
        // An output cap is corpus-wide however the work is cut: every combination stops
        // with the same error.
        let capped = |threads: usize, chunk: u64| {
            let opts = SearchOptions {
                pattern: "needle".into(),
                fixed_strings: true,
                case: CaseMode::Sensitive,
                word: false,
                line_number: true,
                output: OutputMode::Lines,
                max_count: None,
                threads,
                freshness: Freshness::Off,
                limits: Limits { output_bytes: Some(200), deadline: None },
            };
            search_chunked(&opts, &handles, &mut Vec::new(), chunk).unwrap_err()
        };
        for (threads, chunk) in [(1, u64::MAX), (2, 1), (4, 40), (8, 200)] {
            assert!(
                matches!(capped(threads, chunk), Error::OutputLimit(200)),
                "{threads} threads, {chunk}-byte chunks"
            );
        }
        for output in [OutputMode::Lines, OutputMode::Count, OutputMode::FilesWithMatches] {
            let whole = run(1, u64::MAX, output);
            assert!(!whole.0.is_empty());
            assert_eq!(whole.1, 42, "every doc is a candidate");
            assert_eq!(whole.2, 36, "the six docs per shard repeating the needle");
            // One thread always verifies whole shards (chunk sizes are not even looked
            // at), so the small-chunk cases need two threads or more.
            for (threads, chunk) in [(2, 1), (2, 40), (4, 1), (4, 40), (8, 200), (3, u64::MAX)] {
                assert_eq!(run(threads, chunk, output), whole, "{threads} threads, {chunk}-byte chunks, {output:?}");
            }
        }
    }

    #[test]
    fn the_first_failing_chunk_is_the_error_reported() {
        // Two unreadable docs far apart: whichever thread fails first, the search reports
        // the earlier one, as a one-thread search would.
        let docs = (0..40)
            .map(|d| {
                let name = match d {
                    9 => "fail-early".to_string(),
                    33 => "fail-late".to_string(),
                    d => format!("d{d}.txt"),
                };
                (name, b"needle\n".repeat(20))
            })
            .collect();
        let repo = RepoHandle {
            name: "r".into(),
            source: Box::new(MemSource("/r".into())),
            shards: vec![Box::new(MemShard(docs))],
            snapshot: None,
            listing_digest: None,
        };
        for (threads, chunk) in [(1, u64::MAX), (2, 1), (4, 1), (8, 200)] {
            let opts = SearchOptions {
                pattern: "needle".into(),
                fixed_strings: true,
                case: CaseMode::Sensitive,
                word: false,
                line_number: true,
                output: OutputMode::Lines,
                max_count: None,
                threads,
                freshness: Freshness::Off,
                limits: Limits::default(),
            };
            let err = search_chunked(&opts, &[&repo], &mut Vec::new(), chunk).unwrap_err();
            assert!(err.to_string().contains("fail-early"), "{threads} threads, {chunk}-byte chunks: {err}");
        }
    }

    /// Lists by trigram, recording which ones were counted and which decoded. Its filter
    /// holds every listed trigram plus `false_positives`, as a real filter may.
    struct ListShard {
        lists: HashMap<Trigram, Vec<UnitId>>,
        false_positives: Vec<Trigram>,
        counted: Mutex<Vec<Trigram>>,
        decoded: Mutex<Vec<Trigram>>,
    }

    impl ListShard {
        fn new(lists: HashMap<Trigram, Vec<UnitId>>, false_positives: Vec<Trigram>) -> Self {
            Self { lists, false_positives, counted: Mutex::new(Vec::new()), decoded: Mutex::new(Vec::new()) }
        }
        /// What was counted and decoded since the last call.
        fn reads(&self) -> (Vec<Trigram>, Vec<Trigram>) {
            (std::mem::take(&mut *self.counted.lock().unwrap()), std::mem::take(&mut *self.decoded.lock().unwrap()))
        }
    }

    impl Shard for ListShard {
        fn doc_count(&self) -> u32 {
            100
        }
        fn meta(&self, _: DocId) -> Result<crate::ports::DocMeta<'_>> {
            unreachable!("candidates never reads metadata")
        }
        fn postings(&self, t: Trigram) -> Result<Vec<UnitId>> {
            self.decoded.lock().unwrap().push(t);
            Ok(self.lists.get(&t).cloned().unwrap_or_default())
        }
        fn count(&self, t: Trigram) -> Result<u32> {
            self.counted.lock().unwrap().push(t);
            Ok(self.lists.get(&t).map_or(0, |l| u32::try_from(l.len()).unwrap()))
        }
        fn may_contain(&self, t: Trigram) -> bool {
            self.lists.contains_key(&t) || self.false_positives.contains(&t)
        }
        fn markers(&self) -> &crate::ports::Markers {
            unreachable!("candidates never reads markers")
        }
        fn unit_doc(&self, unit: UnitId) -> Result<DocId> {
            Ok(unit)
        }
        fn content(&self, _: DocId) -> Result<std::borrow::Cow<'_, [u8]>> {
            unreachable!("candidates never reads content")
        }
    }

    #[test]
    fn an_and_stops_decoding_once_verifying_is_cheaper() {
        let t = |b: &[u8; 3]| Trigram::from_bytes(b[0], b[1], b[2]);
        let huge = 3 * VERIFY_COST_POSTINGS as u32;
        let lists = HashMap::from([
            (t(b"abc"), (0..huge).collect::<Vec<_>>()),
            (t(b"bcd"), vec![3, 5, 7, 9]),
            (t(b"cde"), (0..50).collect()),
            (t(b"def"), vec![5, 9, 11]),
        ]);
        // zzz is absent but passes the filter, so only its count can rule it out.
        let shard = ListShard::new(lists, vec![t(b"zzz")]);
        let q = Query::And([b"abc", b"bcd", b"cde", b"def"].iter().map(|b| Query::Tri(t(b))).collect());
        // Rarest first: def, bcd and cde fit the budget of 2-3 candidates; abc's list costs
        // more to decode than verifying 2 candidates and is never read. 5 and 9 are in abc
        // too, so the result equals the full intersection here.
        assert_eq!(candidates(&shard, &q).unwrap(), Some(vec![5, 9]));
        assert_eq!(shard.reads().1, vec![t(b"def"), t(b"bcd"), t(b"cde")]);
        // An absent trigram empties the AND before anything is decoded.
        let q = Query::And(vec![Query::Tri(t(b"abc")), Query::Tri(t(b"zzz"))]);
        assert_eq!(candidates(&shard, &q).unwrap(), Some(vec![]));
        assert_eq!(shard.reads(), (vec![t(b"abc"), t(b"zzz")], vec![]));
    }

    #[test]
    fn a_trigram_the_filter_rejects_empties_its_and_before_any_read() {
        let t = |b: &[u8; 3]| Trigram::from_bytes(b[0], b[1], b[2]);
        let and = |tris: &[&[u8; 3]]| Query::And(tris.iter().map(|b| Query::Tri(t(b))).collect());
        let lists = HashMap::from([(t(b"abc"), vec![1, 2]), (t(b"def"), vec![2, 3]), (t(b"ghi"), vec![7])]);
        let shard = ListShard::new(lists, vec![]);
        // xyz is rejected though it comes last: nothing before it is counted or decoded, and
        // the shard counts as filtered.
        assert_eq!(shard_units(&shard, &and(&[b"abc", b"def", b"xyz"])).unwrap(), (Some(vec![]), true));
        assert_eq!(shard.reads(), (vec![], vec![]));
        // An OR inside the AND is checked the same way: both branches rejected, no read.
        let q = Query::And(vec![Query::Tri(t(b"abc")), Query::Or(vec![and(&[b"xyz"]), Query::Tri(t(b"uvw"))])]);
        assert_eq!(shard_units(&shard, &q).unwrap(), (Some(vec![]), true));
        assert_eq!(shard.reads(), (vec![], vec![]));
        // In an OR only the rejected branches are skipped; the other is still read, and the
        // shard is not filtered.
        let q = Query::Or(vec![and(&[b"abc", b"xyz"]), Query::Tri(t(b"ghi")), Query::Tri(t(b"uvw"))]);
        assert_eq!(shard_units(&shard, &q).unwrap(), (Some(vec![7]), false));
        assert_eq!(shard.reads(), (vec![], vec![t(b"ghi")]));
        // The same OR under an AND the filter lets through: its branches are still asked.
        let q = Query::And(vec![Query::Tri(t(b"ghi")), q]);
        assert_eq!(shard_units(&shard, &q).unwrap(), (Some(vec![7]), false));
        assert_eq!(shard.reads(), (vec![t(b"ghi")], vec![t(b"ghi"), t(b"ghi")]));
        // What the filter lets through is planned as before.
        assert_eq!(shard_units(&shard, &and(&[b"abc", b"def"])).unwrap(), (Some(vec![2]), false));
        assert_eq!(shard.reads(), (vec![t(b"abc"), t(b"def")], vec![t(b"abc"), t(b"def")]));
        // "Every unit" needs no trigram, so the filter has nothing to reject.
        assert_eq!(shard_units(&shard, &Query::All).unwrap(), (None, false));
    }

    #[test]
    fn a_skipped_list_leaves_a_superset_and_the_rest_of_the_and_still_applies() {
        let t = |b: &[u8; 3]| Trigram::from_bytes(b[0], b[1], b[2]);
        let huge = 3 * VERIFY_COST_POSTINGS as u32;
        let decoded = |s: &ListShard| s.reads().1;
        let lists = HashMap::from([
            // Lacks 9, so the true intersection with def is [5, 11], but it is too long to read.
            (t(b"abc"), (0..huge).filter(|&u| u != 9).collect::<Vec<_>>()),
            (t(b"def"), vec![5, 9, 11]),
            (t(b"ghi"), vec![1, 2]),
            (t(b"jkl"), vec![1, 2, 3, 4]),
            (t(b"xyz"), vec![40]),
        ]);
        let shard = ListShard::new(lists, vec![]);
        let and = |tris: &[&[u8; 3]]| Query::And(tris.iter().map(|b| Query::Tri(t(b))).collect());
        // Superset: 9 survives because abc was never decoded; verification removes it.
        assert_eq!(candidates(&shard, &and(&[b"abc", b"def"])).unwrap(), Some(vec![5, 9, 11]));
        assert_eq!(decoded(&shard), vec![t(b"def")]);
        // A single trigram is decoded as is.
        assert_eq!(candidates(&shard, &Query::Tri(t(b"def"))).unwrap(), Some(vec![5, 9, 11]));
        decoded(&shard);
        // Once the candidates are empty nothing more is read: ghi, then def empties it, and
        // jkl, though cheap, is never decoded.
        assert_eq!(candidates(&shard, &and(&[b"jkl", b"def", b"ghi"])).unwrap(), Some(vec![]));
        assert_eq!(decoded(&shard), vec![t(b"ghi"), t(b"def")]);
        // A non-trigram part is intersected with the trigram lists: (def | xyz) & jkl share
        // nothing.
        let mixed =
            Query::And(vec![Query::Or(vec![Query::Tri(t(b"def")), Query::Tri(t(b"xyz"))]), Query::Tri(t(b"jkl"))]);
        assert_eq!(candidates(&shard, &mixed).unwrap(), Some(vec![]));
        let mixed =
            Query::And(vec![Query::Or(vec![Query::Tri(t(b"def")), Query::Tri(t(b"xyz"))]), Query::Tri(t(b"abc"))]);
        // abc is the AND's only trigram, so it is decoded however long: the rarest is always read.
        assert_eq!(candidates(&shard, &mixed).unwrap(), Some(vec![5, 11, 40]));
    }

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
            limits: Limits::default(),
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
