//! Builds the shards for one repo.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use crate::catalog::RepoIndex;
use crate::ports::{IndexBackend, ListingDigest, RepoSource, ShardStore, ShardWriter};
use crate::trigram::{ALWAYS_SEARCH, READER_MODE, Trigram, TrigramExtractor};
use crate::{Error, Result};

/// Target size of a posting unit (see [`crate::ports::UnitId`]).
///
/// Measured on the bench corpus: 64 KiB grows posting entries 1.5x and cuts the bytes
/// verified for false-positive docs by up to ~70% (`useLayoutEffect`: 268 → 86 MiB).
/// 16 KiB filters a little better but doubles the postings; 256 KiB barely filters.
pub const UNIT_TARGET: usize = 64 << 10;

/// Splits `content` into units of about `target` bytes, each ending just after a newline
/// (or at the end). A line never spans two units, which is what makes per-unit trigram
/// filtering sound for line-oriented matching. A line longer than `target` makes one larger
/// unit.
#[must_use]
pub fn unit_spans(content: &[u8], target: usize) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::with_capacity(content.len() / target.max(1) + 1);
    let mut start = 0;
    while start < content.len() {
        let want = start + target;
        let end = if want >= content.len() {
            content.len()
        } else {
            memchr::memchr(b'\n', &content[want - 1..]).map_or(content.len(), |i| want + i)
        };
        spans.push(start..end);
        start = end;
    }
    if spans.is_empty() {
        spans.push(0..0);
    }
    spans
}

/// Distinct trigrams per unit of `content`, with `markers` added to the first unit.
fn units(extractor: &mut TrigramExtractor, content: &[u8], markers: &[Trigram]) -> Vec<Vec<Trigram>> {
    let mut out: Vec<Vec<Trigram>> =
        unit_spans(content, UNIT_TARGET).into_iter().map(|r| extractor.extract(&content[r]).to_vec()).collect();
    // Markers are outside the 24-bit trigram space, so appending keeps the list sorted.
    out[0].extend_from_slice(markers);
    out
}

#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    /// Content bytes per shard before a new shard is started. Building a shard holds its
    /// postings in memory, so this is what bounds indexing RAM on small machines.
    pub shard_content_cap: u64,
    /// Part of every shard name, so a reindex never reuses the name of a shard a reader
    /// may still be about to open. One per index run; see [`new_generation`].
    pub generation: u64,
    /// Threads that read files and extract their trigrams ahead of the shard writer. 0: none,
    /// the writer reads each file itself. On one CPU that is cheaper, since the handoff to
    /// another thread costs CPU time with no core to overlap it on (E25: +4% CPU at 1 CPU).
    pub read_threads: usize,
}

/// A generation id no earlier run used: the clock in nanoseconds. Writers hold the store's
/// writer lock, so two runs never start in the same nanosecond on one store.
pub fn new_generation() -> Result<u64> {
    crate::catalog::since_epoch().map(|d| d.as_nanos() as u64)
}

/// A file that could not be indexed. Reported to the caller rather than aborting the
/// repo, because a file vanishing or being locked mid-walk is normal on a live checkout.
#[derive(Debug)]
pub struct SkippedFile {
    pub path: String,
    pub error: Error,
}

#[derive(Debug)]
pub struct IndexReport {
    pub index: RepoIndex,
    pub skipped: Vec<SkippedFile>,
    /// The source's snapshot of the listing that was indexed, for the caller to store in
    /// the [`crate::snapshots::SnapshotPack`]. Written by the caller, once for all repos,
    /// rather than here per repo, because the pack is one file.
    pub snapshot: Option<Vec<u8>>,
}

/// How rg reads a file found while walking a directory (BOM sniffing on, binary
/// detection = quit at the first NUL, searching everything before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Plain bytes: trigram-indexed and searched as a slice.
    Text,
    /// UTF-8 BOM: indexable as-is, but rg's reader strips the BOM, which `^` can observe.
    Utf8Bom,
    /// UTF-16 BOM: rg transcodes it to UTF-8 before matching, so raw-byte trigrams say
    /// nothing about it.
    Utf16,
    /// Contains a NUL at this offset. rg reads the file in fills of its line buffer and quits
    /// at the first fill holding the NUL, after printing the matches of earlier fills and a
    /// warning. scout reproduces rg with a fresh (64 KiB) buffer; rg's own output depends on
    /// how far earlier files grew its reused buffer (E14). Nothing after the NUL can ever
    /// affect output.
    Binary { nul: usize },
}

#[must_use]
pub fn classify(content: &[u8]) -> Kind {
    if content.starts_with(&[0xFF, 0xFE]) || content.starts_with(&[0xFE, 0xFF]) {
        return Kind::Utf16;
    }
    if let Some(nul) = memchr::memchr(0, content) {
        return Kind::Binary { nul };
    }
    if content.starts_with(&[0xEF, 0xBB, 0xBF]) { Kind::Utf8Bom } else { Kind::Text }
}

/// What every file costs the read-ahead window on top of its size: its allocations, and a
/// floor so a run of empty files cannot race ahead of the writer without bound.
const READ_AHEAD_PER_FILE: u64 = 4 << 10;

/// A file read, classified and split into units, ready for the shard writer.
struct Prepared {
    /// The bytes the shard keeps. For a binary file, the bytes up to and including the NUL:
    /// all that can affect rg's output, and the NUL offset reproduces rg's warning text.
    /// Images, fonts and archives usually have a NUL within a few bytes, so this costs
    /// almost nothing to store.
    stored: Vec<u8>,
    binary: bool,
    units: Vec<Vec<Trigram>>,
}

fn prepare(extractor: &mut TrigramExtractor, mut content: Vec<u8>) -> Prepared {
    let kind = classify(&content);
    let units = match kind {
        Kind::Text => units(extractor, &content, &[]),
        Kind::Utf8Bom => units(extractor, &content, &[READER_MODE]),
        Kind::Utf16 => vec![vec![READER_MODE, ALWAYS_SEARCH]],
        // The bytes before the NUL are matched raw (no BOM), so their trigrams filter
        // these docs like any text file.
        Kind::Binary { nul } => units(extractor, &content[..nul], &[READER_MODE]),
    };
    if let Kind::Binary { nul } = kind {
        // Free the rest now: a large binary (.pak, .png) would otherwise sit whole in the
        // read-ahead until the writer reaches it.
        content.truncate(nul + 1);
        content.shrink_to_fit();
    }
    Prepared { stored: content, binary: matches!(kind, Kind::Binary { .. }), units }
}

/// Runs `prepare` over `items` on `threads` threads and hands each result to `consume` on
/// the calling thread, in item order. Items are claimed in order and the claimed but not
/// yet consumed ones may cost at most `window` (by `cost`), except that one item is always
/// let through, however large, so a file bigger than the window cannot stall the run.
/// With 0 threads the calling thread prepares each item itself, just before consuming it.
///
/// The first error from `consume` stops the readers and is returned. A panic on either side
/// stops the run too: the other side would otherwise wait forever (the consumer for an item
/// a dead reader claimed, readers for window space a dead consumer never frees), and the
/// scope, which joins every reader before it re-raises, would hang instead of panicking.
fn read_ahead<T: Sync, S, R: Send>(
    items: &[T],
    threads: usize,
    window: u64,
    cost: impl Fn(&T) -> u64 + Sync,
    state: impl Fn() -> S + Sync,
    prepare: impl Fn(&mut S, &T) -> R + Sync,
    mut consume: impl FnMut(&T, R) -> Result<()>,
) -> Result<()> {
    struct Shared<R> {
        next: usize,
        in_flight: u64,
        ready: BTreeMap<usize, R>,
        stop: bool,
    }
    struct Pipe<R> {
        shared: Mutex<Shared<R>>,
        /// Readers to the consumer: an item is ready. One waiter, so `notify_one`.
        ready: Condvar,
        /// The consumer to readers: window space was freed, or the run stopped.
        space: Condvar,
    }
    /// Stops the run when dropped: always for the consumer (done, failed or unwinding), only
    /// when unwinding for a reader, whose normal exit means nothing to the others.
    struct Stop<'a, R> {
        pipe: &'a Pipe<R>,
        always: bool,
    }
    impl<R> Drop for Stop<'_, R> {
        fn drop(&mut self) {
            if self.always || std::thread::panicking() {
                lock(&self.pipe.shared).stop = true;
                self.pipe.ready.notify_all();
                self.pipe.space.notify_all();
            }
        }
    }
    fn lock<R>(m: &Mutex<Shared<R>>) -> MutexGuard<'_, Shared<R>> {
        // Neither side panics while holding the lock (`prepare` and `consume` run outside
        // it), so a poisoned lock still holds sound state.
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    if threads == 0 {
        let mut st = state();
        for item in items {
            consume(item, prepare(&mut st, item))?;
        }
        return Ok(());
    }
    let pipe = Pipe {
        shared: Mutex::new(Shared { next: 0, in_flight: 0, ready: BTreeMap::new(), stop: false }),
        ready: Condvar::new(),
        space: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..threads.min(items.len()) {
            scope.spawn(|| {
                let _stop = Stop { pipe: &pipe, always: false };
                let mut st = state();
                loop {
                    let i = {
                        let mut g = lock(&pipe.shared);
                        loop {
                            if g.stop || g.next == items.len() {
                                return;
                            }
                            let c = cost(&items[g.next]);
                            if g.in_flight == 0 || g.in_flight.saturating_add(c) <= window {
                                g.in_flight += c;
                                g.next += 1;
                                break g.next - 1;
                            }
                            g = pipe.space.wait(g).unwrap_or_else(PoisonError::into_inner);
                        }
                    };
                    let r = prepare(&mut st, &items[i]);
                    lock(&pipe.shared).ready.insert(i, r);
                    pipe.ready.notify_one();
                }
            });
        }
        let _stop = Stop { pipe: &pipe, always: true };
        for (i, item) in items.iter().enumerate() {
            let r = {
                let mut g = lock(&pipe.shared);
                loop {
                    if let Some(r) = g.ready.remove(&i) {
                        break r;
                    }
                    if g.stop {
                        // Only a panicking reader sets `stop` while the consumer runs. The
                        // scope re-raises that panic, so this error is never seen.
                        return Err(Error::Invalid("a read-ahead thread panicked".into()));
                    }
                    g = pipe.ready.wait(g).unwrap_or_else(PoisonError::into_inner);
                }
            };
            consume(item, r)?;
            lock(&pipe.shared).in_flight -= cost(item);
            pipe.space.notify_all();
        }
        Ok(())
    })
}

struct Building<'a> {
    backend: &'a dyn IndexBackend,
    store: &'a dyn ShardStore,
    repo: &'a str,
    generation: u64,
    staged: Vec<(String, PathBuf)>,
    current: Option<Box<dyn ShardWriter>>,
}

impl Building<'_> {
    fn add(&mut self, meta: &crate::ports::FileMeta, content: &[u8], units: &[Vec<Trigram>]) -> Result<u64> {
        if self.current.is_none() {
            let name = format!("{}.{:x}.{}.{}", self.repo, self.generation, self.staged.len(), self.backend.name());
            let path = self.store.staging_path(&name)?;
            self.current = Some(self.backend.create(&path)?);
            self.staged.push((name, path));
        }
        let w = self.current.as_mut().ok_or_else(|| Error::Invalid("shard writer missing".into()))?;
        w.add(meta, content, units)?;
        Ok(w.content_bytes())
    }

    fn seal(&mut self) -> Result<()> {
        if let Some(w) = self.current.take() {
            w.finish()?;
        }
        Ok(())
    }
}

/// Indexes every file `source` lists into one or more shards and publishes them.
///
/// Shards are only published after all of them were written successfully, so a failed
/// run never leaves a half-updated index behind.
pub fn index_repo(
    repo: &str,
    source: &dyn RepoSource,
    backend: &dyn IndexBackend,
    store: &dyn ShardStore,
    opts: IndexOptions,
) -> Result<IndexReport> {
    if opts.shard_content_cap == 0 {
        return Err(Error::Invalid("shard_content_cap must be > 0".into()));
    }
    let listing = source.list(None)?;
    let snapshot = listing.snapshot;
    let mut files = listing.files;
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));

    let mut b = Building { backend, store, repo, generation: opts.generation, staged: Vec::new(), current: None };
    let mut skipped = Vec::new();
    let (mut indexed, mut bytes, mut binary) = (0u64, 0u64, 0u64);

    // Reading and trigram extraction run ahead on `read_threads` threads while this thread
    // writes shards in path order, so the shards are the same as a serial run's. Serial
    // reads left one huge repo (chromium, 510k files) waiting on one file open at a time
    // (E25). The window bounds the content read ahead to an eighth of a shard. Units cost
    // 4 bytes per distinct trigram: about as much again as source code, up to ~4x for
    // minified text, so read-ahead takes 2/8 to 5/8 of a shard, 5–13% of the 5x-shard
    // memory a worker plans for. Each file also costs a flat [`READ_AHEAD_PER_FILE`]. The
    // cost is the listed size; a file that grew since the listing can overshoot it, as
    // rarely as it happens.
    let window = (opts.shard_content_cap / 8).max(READ_AHEAD_PER_FILE);
    read_ahead(
        &files,
        opts.read_threads,
        window,
        |meta| meta.size + READ_AHEAD_PER_FILE,
        TrigramExtractor::default,
        |extractor, meta| source.read(&meta.path).map(|content| prepare(extractor, content)),
        |meta, prepared| {
            let file = match prepared {
                Ok(file) => file,
                Err(error) => {
                    skipped.push(SkippedFile { path: meta.path.clone(), error });
                    return Ok(());
                }
            };
            let shard_bytes = b.add(meta, &file.stored, &file.units)?;
            if file.binary {
                binary += 1;
            }
            indexed += 1;
            bytes += file.stored.len() as u64;
            if shard_bytes >= opts.shard_content_cap {
                b.seal()?;
            }
            Ok(())
        },
    )?;
    b.seal()?;

    let mut shards = Vec::with_capacity(b.staged.len());
    for (name, path) in &b.staged {
        store.publish(path, name)?;
        shards.push(name.clone());
    }
    // A skipped file is in the listing but not in the index, so the index does not hold
    // exactly this listing and no digest may claim it does.
    let listing_digest = skipped.is_empty().then(|| {
        let mut d = ListingDigest::default();
        for f in &files {
            d.add(&[&f.path], f.size, f.mtime_ns);
        }
        d.finish()
    });
    let indexed_at_unix = crate::catalog::now_unix()?;
    Ok(IndexReport {
        index: RepoIndex {
            backend: backend.name().to_string(),
            shards,
            listing_digest,
            // Only the caller knows whether the source is a synced working copy.
            commit: None,
            files: indexed,
            content_bytes: bytes,
            binary_files: binary,
            indexed_at_unix,
        },
        skipped,
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_spans_end_after_newlines_and_cover_everything() {
        let text = b"aa\nbbbb\ncc\nd";
        let spans = unit_spans(text, 2);
        // The second unit reaches its target inside "bbbb", so it runs to that line's end.
        assert_eq!(spans, vec![0..3, 3..8, 8..11, 11..12]);
        for w in spans.windows(2) {
            assert_eq!(text[w[0].end - 1], b'\n');
            assert_eq!(w[0].end, w[1].start);
        }
        assert_eq!(unit_spans(b"", 4), vec![0..0]);
        assert_eq!(unit_spans(b"no newline at all", 4), vec![0..17]);
        assert_eq!(unit_spans(b"abc\n", 4), vec![0..4]);
    }

    /// Runs `f` on its own thread and fails if it takes longer than 10 s, so a regression
    /// that makes `read_ahead` hang fails the test instead of hanging the suite.
    fn within_timeout<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> std::thread::Result<R> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
        });
        rx.recv_timeout(std::time::Duration::from_secs(10)).expect("read_ahead hung")
    }

    fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
        p.downcast_ref::<&str>().map(ToString::to_string).or_else(|| p.downcast_ref::<String>().cloned()).unwrap()
    }

    #[test]
    fn read_ahead_consumes_in_order_within_the_window() {
        use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
        // Costs vary and some exceed the window alone; the order must still hold and the
        // run must finish. The consumer is slower than the readers, so the window is what
        // limits how far they get ahead (a fast consumer would keep the peak low anyway).
        let items: Vec<u64> = (0..500).map(|i| (i * 7919) % 13).collect();
        let (peak, live) = (AtomicU64::new(0), AtomicU64::new(0));
        let mut seen = Vec::new();
        read_ahead(
            &items,
            4,
            10,
            |&c| c,
            || (),
            |(), &c| {
                peak.fetch_max(live.fetch_add(c, SeqCst) + c, SeqCst);
                c
            },
            |&c, r| {
                std::thread::sleep(std::time::Duration::from_micros(50));
                live.fetch_sub(c, SeqCst);
                seen.push(r);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, items);
        // One item over the window is let through alone, so the peak is the window or the
        // largest single item.
        assert!(peak.into_inner() <= 12);
    }

    #[test]
    fn read_ahead_stops_on_the_first_consumer_error() {
        let items: Vec<u32> = (0..10_000).collect();
        let prepared = std::sync::atomic::AtomicUsize::new(0);
        let err = read_ahead(
            &items,
            4,
            8,
            |_| 1,
            || (),
            |(), &i| {
                prepared.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                i
            },
            |_, i| if i == 20 { Err(Error::Invalid("stop".into())) } else { Ok(()) },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Invalid(m) if m == "stop"));
        // When item 20 fails it still holds one of the 8 window slots, so at most items
        // 0..=27 were ever claimed, and only claimed items are prepared.
        assert!(prepared.into_inner() <= 28);
    }

    #[test]
    fn read_ahead_reader_panic_stops_the_run() {
        let outcome = within_timeout(|| {
            let items: Vec<u32> = (0..100).collect();
            read_ahead(&items, 2, 4, |_| 1, || (), |(), &i| assert!(i != 50, "reader failed"), |_, ()| Ok(()))
        });
        // The scope re-raises a spawned thread's panic under its own message.
        assert_eq!(panic_message(&*outcome.unwrap_err()), "a scoped thread panicked");
    }

    #[test]
    fn read_ahead_consumer_panic_stops_the_readers() {
        // Readers parked on a full window must be woken, or the scope's join hangs.
        let outcome = within_timeout(|| {
            let items: Vec<u32> = (0..1000).collect();
            read_ahead(
                &items,
                4,
                2,
                |_| 1,
                || (),
                |(), &i| i,
                |_, i| {
                    assert!(i != 10, "writer failed");
                    Ok(())
                },
            )
        });
        assert_eq!(panic_message(&*outcome.unwrap_err()), "writer failed");
    }

    #[test]
    fn read_ahead_edge_cases() {
        let none: [u32; 0] = [];
        read_ahead(&none, 4, 1, |_| 1, || (), |(), _| (), |_, ()| Ok(())).unwrap();
        // More threads than items: every item is still consumed once, in order.
        let mut seen = Vec::new();
        read_ahead(
            &[1, 2, 3],
            16,
            100,
            |_| 1,
            || (),
            |(), &i| i,
            |_, i| {
                seen.push(i);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, [1, 2, 3]);
        // No threads: prepared inline, still in order, and an error still stops the run.
        let mut seen = Vec::new();
        let err = read_ahead(
            &[1, 2, 3],
            0,
            1,
            |_| 1,
            || (),
            |(), &i| i,
            |_, i| {
                seen.push(i);
                if i == 2 { Err(Error::Invalid("stop".into())) } else { Ok(()) }
            },
        );
        assert!(matches!(err, Err(Error::Invalid(_))));
        assert_eq!(seen, [1, 2]);
    }

    /// Files of every [`Kind`], plus one listed that cannot be read.
    struct Files(Vec<(String, Vec<u8>)>);

    impl RepoSource for Files {
        fn display_root(&self) -> &'static str {
            "/mem"
        }
        fn list(&self, _: Option<&[u8]>) -> Result<crate::ports::Listing> {
            let gone = crate::ports::FileMeta { path: "gone".into(), size: 9, mtime_ns: 1 };
            let files = self
                .0
                .iter()
                .map(|(p, c)| crate::ports::FileMeta { path: p.clone(), size: c.len() as u64, mtime_ns: 1 })
                .chain(std::iter::once(gone))
                .collect();
            Ok(crate::ports::Listing { files, snapshot: None, full_scan: true })
        }
        fn unchanged(&self, _: &[u8], _: u128) -> Result<bool> {
            Ok(false)
        }
        fn read(&self, path: &str) -> Result<Vec<u8>> {
            self.0
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, c)| c.clone())
                .ok_or_else(|| Error::Invalid(format!("{path} vanished")))
        }
    }

    /// Every `add`, `finish` and `publish`, in order, as the shard writer and store saw them.
    type Log = std::sync::Arc<Mutex<Vec<String>>>;

    struct Recorder(Log);

    struct RecordingWriter(Log, u64);

    impl ShardWriter for RecordingWriter {
        fn add(
            &mut self,
            meta: &crate::ports::FileMeta,
            content: &[u8],
            units: &[Vec<Trigram>],
        ) -> Result<crate::ports::DocId> {
            self.1 += content.len() as u64;
            self.0.lock().unwrap().push(format!("add {} {content:?} {units:?}", meta.path));
            Ok(0)
        }
        fn content_bytes(&self) -> u64 {
            self.1
        }
        fn finish(self: Box<Self>) -> Result<()> {
            self.0.lock().unwrap().push("finish".into());
            Ok(())
        }
    }

    impl IndexBackend for Recorder {
        fn name(&self) -> &'static str {
            "rec"
        }
        fn create(&self, _: &std::path::Path) -> Result<Box<dyn ShardWriter>> {
            Ok(Box::new(RecordingWriter(std::sync::Arc::clone(&self.0), 0)))
        }
        fn open(&self, _: &std::path::Path) -> Result<Box<dyn crate::ports::Shard>> {
            Err(Error::Invalid("not opened while indexing".into()))
        }
    }

    impl ShardStore for Recorder {
        fn load_catalog(&self) -> Result<crate::catalog::Catalog> {
            Err(Error::Invalid("unused".into()))
        }
        fn save_catalog(&self, _: &crate::catalog::Catalog) -> Result<()> {
            Err(Error::Invalid("unused".into()))
        }
        fn staging_path(&self, shard: &str) -> Result<PathBuf> {
            Ok(PathBuf::from(shard))
        }
        fn publish(&self, staged: &std::path::Path, _: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("publish {}", staged.display()));
            Ok(())
        }
        fn replace(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Err(Error::Invalid("unused".into()))
        }
        fn local_path(&self, _: &str) -> Result<PathBuf> {
            Err(Error::Invalid("unused".into()))
        }
        fn remove(&self, _: &str) -> Result<()> {
            Err(Error::Invalid("unused".into()))
        }
        fn list(&self) -> Result<Vec<String>> {
            Err(Error::Invalid("unused".into()))
        }
        fn lock_writer(&self) -> Result<crate::ports::WriterLock> {
            Err(Error::Invalid("unused".into()))
        }
        fn try_lock_writer(&self) -> Result<Option<crate::ports::WriterLock>> {
            Err(Error::Invalid("unused".into()))
        }
    }

    #[test]
    fn shards_do_not_depend_on_read_threads() {
        let mut files: Vec<(String, Vec<u8>)> =
            (0..60).map(|i| (format!("t{i:02}.txt"), format!("line {i}\n").repeat(i * 37).into_bytes())).collect();
        files.push(("bom.txt".into(), b"\xEF\xBB\xBFhello bom\n".to_vec()));
        files.push(("u16.txt".into(), b"\xFF\xFEh\0i\0".to_vec()));
        files.push(("bin.dat".into(), [b"abcdef".as_slice(), &[0], &[7; 5000]].concat()));
        files.push(("empty.txt".into(), Vec::new()));
        let source = Files(files);
        let run = |threads| {
            let rec = Recorder(Log::default());
            // The same generation every run, so shard names match too.
            let opts = IndexOptions { shard_content_cap: 4096, generation: 1, read_threads: threads };
            let report = index_repo("r", &source, &rec, &rec, opts).unwrap();
            let skipped: Vec<_> = report.skipped.iter().map(|s| s.path.clone()).collect();
            let i = report.index;
            let log = rec.0.lock().unwrap().clone();
            (log, i.shards, i.files, i.content_bytes, i.binary_files, i.listing_digest.is_none(), skipped)
        };
        let serial = run(1);
        assert!(serial.1.len() > 3, "the cap should split the repo into several shards");
        assert_eq!(serial.4, 1);
        assert_eq!(serial.6, ["gone"]);
        // The binary file keeps its bytes through the NUL only.
        assert!(serial.0.iter().any(|l| l.starts_with("add bin.dat [97, 98, 99, 100, 101, 102, 0] ")));
        for threads in [0, 2, 4, 16] {
            assert_eq!(run(threads), serial, "{threads} read threads");
        }
    }
}
