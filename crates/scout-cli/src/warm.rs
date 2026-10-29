//! Reads a served index into the page cache in the background after it opens (E45–E48), so
//! the queries that come after a restart find it in memory instead of each waiting on the
//! disk for its own pages. The shards are mmapped, and a plain `read` fills the same page
//! cache the map faults from, so this needs no `unsafe` and no knowledge of the format.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Bytes per read: ~2 ms of transfer at gp3's 125 MB/s. 4 MiB (~32 ms) slowed the queries
/// sent at ready by 14% (E45, geomean 1.141), the selective ones most (`llvm_regex` 1.86).
/// At 256 KiB the walk needs ~480 reads a second for 125 MB/s, well under gp3's 3000 IOPS.
/// A small query that lands behind a read still waited 15–19 ms in E49 (`llvm_regex`
/// 60 → 75 ms, median, with queries 300 ms apart), far more than one read's transfer; the
/// likely cause is the throttle dispatching by time slices
/// (docs/research/background-warmup.md), not measured.
pub const CHUNK: usize = 256 << 10;
/// How long nothing must have run before the walk reads. Pausing only while a search ran
/// still slowed searches sent back to back (E46: `literal_none` 3.4x at ready): the walk
/// read in the gaps between them, and the volume's throughput cap, which every read
/// shares, made the next search wait for it. A client that sends a burst sends its next
/// query within milliseconds, so the walk waits out the burst. The cost: a client that
/// sends a query at least every 100 ms keeps the walk from reading at all, and the cache
/// then holds what those queries read, not the whole index.
pub const IDLE: Duration = Duration::from_millis(100);
/// The longest the walk blocks in [`Activity::wait_idle`] before it looks at `stop` again,
/// so the walk of a generation no longer served ends even while searches never pause.
pub const RECHECK: Duration = Duration::from_secs(1);

/// What a server is doing that the walk must not compete with for the disk: searches and
/// sync cycles (a cycle fetches, indexes and writes shards). The walk sleeps on a condvar
/// until none runs, then exactly out the rest of `IDLE` (E48). E47 woke every 5 ms to
/// look instead; while that walk lived, the smallest warm query (`literal_none`, ~1.4 ms)
/// ran 1.06–1.4x slower even when the walk read next to nothing (E47b).
#[derive(Debug, Default)]
pub struct Activity {
    state: Mutex<Running>,
    ended: Condvar,
}

#[derive(Debug, Default)]
struct Running {
    count: usize,
    /// When the last activity ended; `None` before any did.
    last_end: Option<Instant>,
}

impl Activity {
    /// Counts an activity as running until the returned guard drops. A mutex, not atomics:
    /// the count must change under the lock the walk waits with, or the walk could miss
    /// the wake-up. Per search: two uncontended lock round trips and one `notify_all` (a
    /// futex wake, about a microsecond), against searches of milliseconds.
    pub fn start(&self) -> Active<'_> {
        self.lock().count += 1;
        Active(self)
    }

    /// Blocks until nothing has run for `IDLE`, or for `RECHECK` at most; true if idle.
    pub fn wait_idle(&self) -> bool {
        let deadline = Instant::now() + RECHECK;
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            let quiet_at = match (state.count, state.last_end) {
                (0, None) => return true,
                (0, Some(end)) if end + IDLE <= now => return true,
                (0, Some(end)) => end + IDLE,
                // Woken when it ends.
                _ => deadline,
            };
            if now >= deadline {
                return false;
            }
            // An activity that starts and ends meanwhile wakes this early and moves
            // `last_end`, so the loop recomputes when it is quiet.
            let wait = quiet_at.min(deadline) - now;
            state = self.ended.wait_timeout(state, wait).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Running> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A running activity; see [`Activity::start`].
pub struct Active<'a>(&'a Activity);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.count -= 1;
        state.last_end = Some(Instant::now());
        drop(state);
        self.0.ended.notify_all();
    }
}

/// How far a walk got, for `/status`.
#[derive(Debug, Default)]
pub struct Progress {
    /// Bytes this walk read.
    pub read: AtomicU64,
    /// Bytes of shards an earlier walk read, which this one does not read again.
    pub skipped: AtomicU64,
    /// The index's bytes. Set before the first read.
    pub total: AtomicU64,
    /// Set once it read all but `skipped`, or ended early.
    pub done: AtomicBool,
    /// Why it ended early, if it did.
    pub ended: Mutex<Option<String>>,
}

impl Progress {
    /// Marks the walk over, with why if it ended early. The caller records what a finished
    /// walk read before this, so `/status` never says done while the next generation's
    /// walk could still miss it.
    pub fn finish(&self, result: &Result<(), Ended>) {
        if let Err(e) = result {
            *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
        }
        self.done.store(true, Ordering::Release);
    }
}

/// Why a walk ended before reading every byte.
#[derive(Debug)]
pub enum Ended {
    /// `stop` said so: what it was warming is no longer served.
    Stopped,
    /// The index is larger than the budget, so none was read.
    OverBudget {
        size: u64,
    },
    Failed(PathBuf, std::io::Error),
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped: another generation is served"),
            Self::OverBudget { size } => write!(f, "not read: the index's {} MiB exceed the budget", size >> 20),
            Self::Failed(path, e) => write!(f, "{}: {e}", path.display()),
        }
    }
}

/// Reads the shards of `index` into the page cache, one `CHUNK` at a time, if the whole
/// index fits `budget`; none of them if not. An index that does not fit in memory cannot be
/// cached whole: reading part of it would evict what queries just read, and every new
/// generation would read it from disk again. Shards in `warmed` (read by the walk of an
/// earlier generation) are skipped: a sync that changes one repo keeps the other shards,
/// and rereading them, cache hits, would still copy the whole index through the 2 CPUs.
/// A warmed shard the kernel evicted since is not read again; queries fault it back in.
/// Before each chunk it calls `idle` until that returns true (see
/// [`Activity::wait_idle`]), and gives up when `stop`. The caller ends it with
/// [`Progress::finish`].
pub fn walk(
    index: &[PathBuf],
    warmed: &HashSet<PathBuf>,
    budget: u64,
    progress: &Progress,
    idle: impl Fn() -> bool,
    stop: impl Fn() -> bool,
) -> Result<(), Ended> {
    // A shard gone because another generation is served (its files collected) means
    // stopped, not failed.
    let gone = |path: &PathBuf, e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound && stop() { Ended::Stopped } else { Ended::Failed(path.clone(), e) }
    };
    let (mut size, mut skipped) = (0u64, 0u64);
    let mut unread = Vec::new();
    for path in index {
        let len = std::fs::metadata(path).map_err(|e| gone(path, e))?.len();
        size += len;
        if warmed.contains(path) {
            skipped += len;
        } else {
            unread.push((path, len));
        }
    }
    progress.total.store(size, Ordering::Release);
    if size > budget {
        return Err(Ended::OverBudget { size });
    }
    progress.skipped.store(skipped, Ordering::Release);
    let mut buf = vec![0u8; CHUNK];
    for (path, len) in unread {
        let failed = |e| Ended::Failed(path.clone(), e);
        // Opened only when its turn comes: a walk held off by searches can last long, and
        // holding every shard open meanwhile would spend descriptors the server's own opens
        // need.
        let mut file = File::open(path).map_err(|e| gone(path, e))?;
        // From the end backward: each chunk is then one read of CHUNK bytes on the volume.
        // Forward, the kernel's readahead split each into 128 KiB reads (`read_ahead_kb`),
        // 20,285 reads for the index against 11,526 backward (E48 against E47b): twice the
        // share of the volume's IOPS that queries also need.
        let mut end = len;
        while end > 0 {
            loop {
                if stop() {
                    return Err(Ended::Stopped);
                }
                if idle() {
                    break;
                }
            }
            let n = end.min(CHUNK as u64);
            let start = end - n;
            file.seek(SeekFrom::Start(start)).map_err(failed)?;
            // At most CHUNK, so it fits a usize.
            file.read_exact(&mut buf[..n as usize]).map_err(failed)?;
            end = start;
            progress.read.fetch_add(n, Ordering::Release);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn files(dir: &std::path::Path, sizes: &[usize]) -> Vec<PathBuf> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let p = dir.join(format!("{i}.native"));
                std::fs::write(&p, vec![i as u8; n]).unwrap();
                p
            })
            .collect()
    }

    /// `walk`, then `finish`, as serve does.
    fn run(
        index: &[PathBuf],
        warmed: &HashSet<PathBuf>,
        budget: u64,
        progress: &Progress,
        idle: impl Fn() -> bool,
        stop: impl Fn() -> bool,
    ) -> Result<(), Ended> {
        let result = walk(index, warmed, budget, progress, idle, stop);
        progress.finish(&result);
        result
    }

    fn ended(p: &Progress) -> Option<String> {
        p.ended.lock().unwrap().clone()
    }

    #[test]
    fn reads_every_byte_of_an_index_that_fits_and_none_of_one_that_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let paths = files(dir.path(), &[CHUNK + 10, 3, 0]);
        let size = CHUNK as u64 + 13;
        let all = Progress::default();
        run(&paths, &HashSet::new(), size, &all, || true, || false).unwrap();
        assert_eq!(all.read.load(Ordering::Acquire), size);
        assert_eq!(all.total.load(Ordering::Acquire), size);
        assert!(all.done.load(Ordering::Acquire));
        assert_eq!(ended(&all), None);
        let over = Progress::default();
        assert!(
            matches!(run(&paths, &HashSet::new(), size - 1, &over, || true, || false), Err(Ended::OverBudget { size: s }) if s == size)
        );
        assert_eq!(over.read.load(Ordering::Acquire), 0);
        assert!(over.done.load(Ordering::Acquire));
        assert!(ended(&over).unwrap().contains("exceed the budget"));
    }

    #[test]
    fn waits_until_idle_and_stops_when_told() {
        let dir = tempfile::tempdir().unwrap();
        let paths = files(dir.path(), &[3 * CHUNK]);
        // Not idle for the first few waits: nothing is read until it is.
        let waits = Cell::new(0);
        let progress = Progress::default();
        let idle = || {
            waits.set(waits.get() + 1);
            let busy = waits.get() <= 3;
            if busy {
                assert_eq!(progress.read.load(Ordering::Acquire), 0);
            }
            !busy
        };
        run(&paths, &HashSet::new(), u64::MAX, &progress, idle, || false).unwrap();
        assert_eq!(progress.read.load(Ordering::Acquire), 3 * CHUNK as u64);
        // Told to stop after one chunk: it reads no more, reports done and why.
        let stopped = Progress::default();
        let stop = || stopped.read.load(Ordering::Acquire) >= CHUNK as u64;
        assert!(matches!(run(&paths, &HashSet::new(), u64::MAX, &stopped, || true, stop), Err(Ended::Stopped)));
        assert_eq!(stopped.read.load(Ordering::Acquire), CHUNK as u64);
        assert!(stopped.done.load(Ordering::Acquire));
        assert!(ended(&stopped).unwrap().starts_with("stopped"));
    }

    #[test]
    fn wait_idle_returns_once_nothing_ran_for_idle() {
        let activity = Activity::default();
        assert!(activity.wait_idle(), "nothing ran yet");
        let t = Instant::now();
        drop(activity.start());
        assert!(activity.wait_idle());
        assert!(t.elapsed() >= IDLE, "it waits out IDLE after the last end");
        assert!(t.elapsed() < RECHECK);
    }

    #[test]
    fn an_activity_during_the_idle_window_starts_it_again() {
        let activity = Activity::default();
        drop(activity.start());
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(IDLE / 2);
                drop(activity.start());
            });
            let t = Instant::now();
            assert!(activity.wait_idle());
            // Not IDLE after the first end but after the second, half an IDLE later.
            assert!(t.elapsed() >= (IDLE + IDLE / 2).saturating_sub(Duration::from_millis(5)), "{:?}", t.elapsed());
        });
    }

    #[test]
    fn a_shard_gone_while_stopping_is_stopped_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = files(dir.path(), &[10]);
        paths.push(dir.path().join("collected.native"));
        let progress = Progress::default();
        assert!(matches!(run(&paths, &HashSet::new(), u64::MAX, &progress, || true, || true), Err(Ended::Stopped)));
    }

    #[test]
    fn wait_idle_sleeps_while_one_runs_and_wakes_when_it_ends() {
        let activity = Activity::default();
        let running = activity.start();
        // Running for all of RECHECK: not idle, so the walk can look at `stop`.
        let t = Instant::now();
        assert!(!activity.wait_idle());
        assert!(t.elapsed() >= RECHECK);
        std::thread::scope(|s| {
            let worker = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                drop(running);
                Instant::now()
            });
            assert!(activity.wait_idle());
            let end = worker.join().unwrap();
            // Woken by the end, then IDLE after it; not RECHECK's full second.
            assert!(end.elapsed() < IDLE + Duration::from_millis(500));
        });
    }

    #[test]
    fn skips_warmed_shards_but_counts_them_against_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let paths = files(dir.path(), &[CHUNK, 7]);
        let warmed = HashSet::from([paths[0].clone()]);
        let size = CHUNK as u64 + 7;
        let progress = Progress::default();
        run(&paths, &warmed, size, &progress, || true, || false).unwrap();
        assert_eq!(progress.read.load(Ordering::Acquire), 7);
        assert_eq!(progress.skipped.load(Ordering::Acquire), CHUNK as u64);
        assert_eq!(progress.total.load(Ordering::Acquire), size);
        let over = Progress::default();
        assert!(matches!(run(&paths, &warmed, size - 1, &over, || true, || false), Err(Ended::OverBudget { .. })));
        assert_eq!(over.read.load(Ordering::Acquire), 0);
        assert_eq!(over.skipped.load(Ordering::Acquire), 0, "nothing is skipped of an index not read");
    }

    #[test]
    fn a_missing_file_fails_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = files(dir.path(), &[10]);
        paths.push(dir.path().join("gone.native"));
        let progress = Progress::default();
        let err = run(&paths, &HashSet::new(), u64::MAX, &progress, || true, || false).unwrap_err();
        assert!(matches!(err, Ended::Failed(ref p, _) if p.ends_with("gone.native")));
        assert_eq!(progress.read.load(Ordering::Acquire), 0);
        assert!(ended(&progress).unwrap().contains("gone.native"));
    }
}
