//! [`RepoSource`] for repos on the local filesystem, with rg's ignore rules.
//!
//! A listing is either a full walk (rg's walker, recorded as a [`snapshot`]) or a refresh
//! of an earlier snapshot. A refresh re-lists only directories that may have changed:
//! the ones the volume's change journal names (Windows), or all of them otherwise. Both
//! produce exactly what a full walk would.

mod context;
mod dirs;
mod journal;
mod refresh;
pub mod snapshot;

// The only platform switch in the crate: both modules expose the same functions and
// types (listing with ids, volume names, the change journal), so nothing else needs a cfg.
#[cfg(windows)]
#[allow(unsafe_code)]
#[path = "win.rs"]
mod os;
#[cfg(not(windows))]
#[path = "portable.rs"]
mod os;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use ignore::{WalkBuilder, WalkState};
use scout_core::ports::{FileMeta, Listing, RepoSource};
use scout_core::{Error, Result};

use crate::context::Plan;
pub use crate::context::{ChangeDetection, FsContext};
use crate::refresh::{Outcome, Select};

/// Peak working set and peak private commit of an exited child that has not been reaped
/// yet, in bytes. Here because all OS FFI of the workspace is in this crate's `win.rs`
/// (CLAUDE.md); `ErrorKind::Unsupported` where it is not measured.
pub fn process_peak_memory(child: &std::process::Child) -> std::io::Result<(u64, u64)> {
    os::peak_memory(child)
}

/// Memory this process may use, in bytes, for sizing the indexing budget: physical RAM, capped
/// by a cgroup limit on Linux (containers, systemd units). Here for the same reason
/// as [`process_peak_memory`]: a crate like sysinfo would do it without unsafe here, but
/// links shell32, oleaut32, pdh and more into every scout process, which made each search
/// 5–7 ms slower at process start (docs/experiments.md, E11 and E12).
pub fn usable_memory() -> std::io::Result<u64> {
    os::usable_memory()
}

pub struct FsSource {
    root: PathBuf,
    display: String,
    threads: usize,
    ctx: Arc<FsContext>,
}

pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    // A missing/pre-epoch mtime maps to 0. Freshness compares size *and* mtime, so the
    // worst case is that such a file is re-read from disk, never that a change is missed.
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

pub(crate) fn now_ns() -> Result<u64> {
    let d = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Invalid(format!("system clock before 1970: {e}")))?;
    Ok(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The directory no longer exists as a directory: deleted, or replaced by a file.
pub(crate) fn is_gone(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

pub(crate) fn join_rel(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() { root.to_path_buf() } else { root.join(rel) }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a walk found: accepted files and the directories it entered.
#[derive(Default)]
pub(crate) struct Walked {
    pub files: Vec<FileMeta>,
    pub dirs: HashSet<String>,
}

impl FsSource {
    /// `root` must be an absolute directory; it is also the prefix printed in results,
    /// matching what `rg PATTERN <root>` prints.
    pub fn new(root: &Path, threads: usize, ctx: Arc<FsContext>) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::Invalid(format!("repo root must be absolute: {}", root.display())));
        }
        if !root.is_dir() {
            return Err(Error::Invalid(format!("repo root is not a directory: {}", root.display())));
        }
        let display = root
            .to_str()
            .ok_or_else(|| Error::Invalid(format!("repo root is not UTF-8: {}", root.display())))?
            .to_string();
        Ok(Self { root: root.to_path_buf(), display, threads: threads.max(1), ctx })
    }

    /// For roots already validated by [`FsSource::new`] when the repo was added. Skips the
    /// `is_dir` stat: on Windows it costs ~40 µs per repo, paid on every search. A repo
    /// deleted since then still fails loudly, because listing the root errors.
    pub fn for_search(root: String, threads: usize, ctx: Arc<FsContext>) -> Result<Self> {
        if !Path::new(&root).is_absolute() {
            return Err(Error::Invalid(format!("repo root must be absolute: {root}")));
        }
        Ok(Self { root: PathBuf::from(&root), display: root, threads: threads.max(1), ctx })
    }

    /// Same configuration as `rg`'s `HiArgs::walk_builder` with default flags.
    fn walker(&self, start: &Path) -> WalkBuilder {
        let mut b = WalkBuilder::new(start);
        b.follow_links(false)
            .hidden(true)
            .parents(true)
            .ignore(true)
            .git_global(true)
            .git_ignore(true)
            .git_exclude(true)
            .require_git(true)
            .threads(self.threads)
            .add_custom_ignore_filename(".rgignore");
        b
    }

    fn relative(&self, path: &Path) -> Result<String> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| Error::Invalid(format!("{} escapes repo root", path.display())))?;
        let s = rel.to_str().ok_or_else(|| Error::Invalid(format!("path is not UTF-8: {}", path.display())))?;
        Ok(if std::path::MAIN_SEPARATOR == '/' { s.to_string() } else { s.replace(std::path::MAIN_SEPARATOR, "/") })
    }

    /// Runs the real walker from `start` (the repo root, or a directory inside it).
    pub(crate) fn walk(&self, start: &Path, max_depth: Option<usize>) -> Result<Walked> {
        let found = Mutex::new(Walked::default());
        let first_error: Mutex<Option<Error>> = Mutex::new(None);
        let fail = |e: Error| {
            lock(&first_error).get_or_insert(e);
            WalkState::Quit
        };
        self.walker(start).max_depth(max_depth).build_parallel().run(|| {
            Box::new(|entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        return fail(Error::backend(format!("walking {}", self.display), e));
                    }
                };
                let Some(ft) = entry.file_type() else {
                    return WalkState::Continue;
                };
                if ft.is_dir() {
                    match self.relative(entry.path()) {
                        Ok(rel) => lock(&found).dirs.insert(rel),
                        Err(e) => return fail(e),
                    };
                    return WalkState::Continue;
                }
                if !ft.is_file() {
                    return WalkState::Continue;
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(e) => {
                        return fail(Error::backend(format!("stat {}", entry.path().display()), e));
                    }
                };
                let path = match self.relative(entry.path()) {
                    Ok(p) => p,
                    Err(e) => return fail(e),
                };
                lock(&found).files.push(FileMeta { path, size: meta.len(), mtime_ns: mtime_ns(&meta) });
                WalkState::Continue
            })
        });
        if let Some(e) = first_error.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
            return Err(e);
        }
        Ok(found.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn full_listing(&self) -> Result<Listing> {
        // Everything that describes the state is captured *before* the walk (journal
        // position, watched files): a change made while walking then shows up as a change
        // since the snapshot instead of being baked into it unseen.
        let cursor = self.ctx.cursor(&self.root)?;
        let watch = snapshot::watch_list(&self.root, |p| self.ctx.stat(p));
        let walk_started_ns = now_ns()?;
        let walked = self.walk(&self.root, None)?;
        let mut accepted: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for f in &walked.files {
            let (parent, name) = f.path.rsplit_once('/').unwrap_or(("", &f.path));
            accepted.entry(parent.to_string()).or_default().push(name.to_string());
        }
        let snap = snapshot::build(&self.root, &walked.dirs, &accepted, walk_started_ns, cursor, watch)
            .map_err(|e| Error::io(&self.root, e))?;
        Ok(Listing { files: snap.files(), snapshot: Some(self.encode(&snap)?), full_scan: true })
    }

    fn encode(&self, snap: &snapshot::Snapshot) -> Result<Vec<u8>> {
        snapshot::encode(snap).map_err(|e| Error::backend(format!("encoding snapshot of {}", self.display), e))
    }
}

impl RepoSource for FsSource {
    fn display_root(&self) -> &str {
        &self.display
    }

    fn list(&self, snapshot: Option<&[u8]>) -> Result<Listing> {
        let Some(bytes) = snapshot else {
            return self.full_listing();
        };
        let decoded = snapshot::decode(bytes)
            .map_err(|e| Error::Corrupt { shard: format!("snapshot of {}", self.display), reason: e.to_string() })?;
        // A snapshot from another format version is not used (and replaced by the walk).
        let Some(snap) = decoded else {
            return self.full_listing();
        };
        if self.ctx.watch_changed(&snap.watch) {
            return self.full_listing();
        }
        let (select, cursor, persist_cursor) = match self.ctx.plan(&self.root, &snap)? {
            // A scan that establishes a journal position is worth keeping: it lets the
            // next search use the journal again.
            Plan::Scan { cursor } => (Select::All, cursor.clone(), cursor.is_some()),
            Plan::Journal { touched, cursor, persist_cursor } => (Select::Only(touched), Some(cursor), persist_cursor),
        };
        let full_scan = matches!(select, Select::All);
        match refresh::refresh(self, snap, &select)? {
            Outcome::IgnoreChanged => self.full_listing(),
            Outcome::Refreshed { mut snapshot, changed } => {
                snapshot.journal = cursor;
                let persist = changed || persist_cursor;
                let bytes = if persist { Some(self.encode(&snapshot)?) } else { None };
                Ok(Listing { files: snapshot.files(), snapshot: bytes, full_scan })
            }
        }
    }

    fn unchanged(&self, snapshot: &[u8], indexed: u128) -> Result<bool> {
        let head = snapshot::decode_head(snapshot)
            .map_err(|e| Error::Corrupt { shard: format!("snapshot of {}", self.display), reason: e.to_string() })?;
        // Another format: `list` replaces it.
        let Some(head) = head else { return Ok(false) };
        // A digest that differs from the index means the snapshot describes another
        // listing (it was refreshed after a change): the overlay has to be computed.
        if head.digest != indexed || self.ctx.watch_changed(&head.watch) {
            return Ok(false);
        }
        self.ctx.untouched(&self.root, &head)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let full = self.root.join(path);
        std::fs::read(&full).map_err(|e| Error::io(full, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(files: Vec<FileMeta>) -> Vec<(String, u64, u64)> {
        let mut v: Vec<_> = files.into_iter().map(|f| (f.path, f.size, f.mtime_ns)).collect();
        v.sort();
        v
    }

    fn paths(files: Vec<FileMeta>) -> Vec<String> {
        sorted(files).into_iter().map(|f| f.0).collect()
    }

    // Only where `usable_memory` is implemented: other Unixes need `--memory-mb`.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn usable_memory_is_plausible() {
        // Every machine that runs the tests has more than 256 MiB and less than 64 TiB.
        let total = usable_memory().unwrap();
        assert!((256 << 20..64 << 40).contains(&total), "{total}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n*.log\nbuild/\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/nested/lib.rs"), "pub fn x() {}").unwrap();
        std::fs::write(root.join("src/debug.log"), "ignored").unwrap();
        std::fs::write(root.join("target/out.rs"), "ignored").unwrap();
        std::fs::write(root.join(".hidden"), "hidden").unwrap();
        dir
    }

    fn source(root: &Path, detection: ChangeDetection) -> FsSource {
        FsSource::new(root, 2, Arc::new(FsContext::new(detection))).unwrap()
    }

    /// A fresh context per listing, like a new `scout search` process: journal positions
    /// and stat caches are per process.
    fn relist(root: &Path, detection: ChangeDetection, snap: &[u8]) -> Listing {
        source(root, detection).list(Some(snap)).unwrap()
    }

    fn full(root: &Path) -> Vec<(String, u64, u64)> {
        sorted(source(root, ChangeDetection::Scan).list(None).unwrap().files)
    }

    const MODES: [ChangeDetection; 2] = [ChangeDetection::Journal, ChangeDetection::Scan];

    fn digest_of(files: &[(String, u64, u64)]) -> u128 {
        let mut d = scout_core::ports::ListingDigest::default();
        for (path, size, mtime) in files {
            d.add(&[path], *size, *mtime);
        }
        d.finish()
    }

    /// Asks `unchanged` with the snapshot's own digest as "the indexed listing". When it
    /// says yes, the current full walk must have exactly that digest.
    fn check_unchanged(root: &Path, mode: ChangeDetection, snap: &[u8], context: &str) -> bool {
        let digest = snapshot::decode(snap).unwrap().unwrap().digest();
        let claimed = source(root, mode).unchanged(snap, digest).unwrap();
        if claimed {
            assert_eq!(digest_of(&full(root)), digest, "unchanged() was wrong: {context}");
        }
        claimed
    }

    #[test]
    fn lists_with_rg_ignore_rules() {
        let dir = repo();
        let src = source(dir.path(), ChangeDetection::Scan);
        assert_eq!(paths(src.list(None).unwrap().files), vec!["src/main.rs", "src/nested/lib.rs"]);
        assert_eq!(src.read("src/main.rs").unwrap(), b"fn main() {}");
    }

    #[test]
    fn refreshed_listing_matches_full_walk_after_changes() {
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            let snap = source(root, mode).list(None).unwrap().snapshot.unwrap();

            // New eligible file, new ignored file, new dir, deleted file, new hidden dir,
            // modified file.
            std::fs::write(root.join("src/new.rs"), "new").unwrap();
            std::fs::write(root.join("src/also.log"), "ignored").unwrap();
            std::fs::create_dir_all(root.join("docs/deep")).unwrap();
            std::fs::write(root.join("docs/deep/guide.md"), "# guide").unwrap();
            std::fs::remove_file(root.join("src/nested/lib.rs")).unwrap();
            std::fs::create_dir_all(root.join(".cache")).unwrap();
            std::fs::write(root.join(".cache/x.rs"), "hidden").unwrap();
            std::fs::write(root.join("src/main.rs"), "fn main() { changed(); }").unwrap();

            let fresh = relist(root, mode, &snap);
            assert_eq!(sorted(fresh.files), full(root), "{mode:?}");
            assert!(fresh.snapshot.is_some(), "{mode:?}: a changed state must be persisted");
        }
    }

    #[test]
    fn refreshes_chain_through_persisted_snapshots() {
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            let mut snap = source(root, mode).list(None).unwrap().snapshot.unwrap();
            for i in 0..3 {
                std::fs::write(root.join(format!("src/f{i}.rs")), "x").unwrap();
                std::fs::remove_dir_all(root.join("src/nested")).ok();
                let fresh = relist(root, mode, &snap);
                assert_eq!(sorted(fresh.files), full(root), "{mode:?} round {i}");
                snap = fresh.snapshot.unwrap();
            }
            // Nothing changed since the last persisted state: nothing to persist.
            let idle = relist(root, mode, &snap);
            assert_eq!(sorted(idle.files), full(root));
            if mode == ChangeDetection::Scan {
                assert!(idle.snapshot.is_none());
            }
        }
    }

    #[test]
    fn deleted_and_replaced_directories() {
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            std::fs::create_dir_all(root.join("pkg/a/b")).unwrap();
            std::fs::write(root.join("pkg/a/b/old.rs"), "old").unwrap();
            std::fs::create_dir_all(root.join("gone/sub")).unwrap();
            std::fs::write(root.join("gone/sub/x.rs"), "x").unwrap();
            let snap = source(root, mode).list(None).unwrap().snapshot.unwrap();

            // Same name, different directory: its old subtree must not survive.
            std::fs::remove_dir_all(root.join("pkg/a")).unwrap();
            std::fs::create_dir_all(root.join("pkg/a/c")).unwrap();
            std::fs::write(root.join("pkg/a/c/new.rs"), "new").unwrap();
            std::fs::remove_dir_all(root.join("gone")).unwrap();

            let fresh = relist(root, mode, &snap);
            assert_eq!(sorted(fresh.files), full(root), "{mode:?}");
        }
    }

    #[test]
    fn renamed_directory_moves_its_files() {
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            let snap = source(root, mode).list(None).unwrap().snapshot.unwrap();
            std::fs::rename(root.join("src/nested"), root.join("src/moved")).unwrap();
            let fresh = relist(root, mode, &snap);
            assert_eq!(sorted(fresh.files), full(root), "{mode:?}");
        }
    }

    #[test]
    fn kind_change_is_decided_again() {
        // `build/` ignores a directory called build, not a file.
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            std::fs::create_dir_all(root.join("src/build")).unwrap();
            std::fs::write(root.join("src/build/gen.rs"), "gen").unwrap();
            let snap = source(root, mode).list(None).unwrap().snapshot.unwrap();
            std::fs::remove_dir_all(root.join("src/build")).unwrap();
            std::fs::write(root.join("src/build"), "now a file").unwrap();
            let fresh = relist(root, mode, &snap);
            assert!(paths(fresh.files.clone()).contains(&"src/build".to_string()), "{mode:?}");
            assert_eq!(sorted(fresh.files), full(root), "{mode:?}");
        }
    }

    #[test]
    fn new_file_in_fileless_directory_is_found() {
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            std::fs::create_dir_all(root.join("pkg/inner")).unwrap();
            std::fs::write(root.join("pkg/inner/a.rs"), "a").unwrap();
            let snap = source(root, mode).list(None).unwrap().snapshot.unwrap();
            // Make sure the new entry gets a strictly later directory mtime.
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(root.join("pkg/b.rs"), "b").unwrap();
            let fresh = relist(root, mode, &snap);
            assert!(paths(fresh.files).contains(&"pkg/b.rs".to_string()), "{mode:?}");
        }
    }

    #[test]
    fn missing_root_is_an_error() {
        for mode in MODES {
            let dir = repo();
            let snap = source(dir.path(), mode).list(None).unwrap().snapshot.unwrap();
            let path = dir.path().to_path_buf();
            drop(dir);
            assert!(!path.exists());
            assert!(source_unchecked(&path, mode).list(Some(&snap)).is_err(), "{mode:?}");
        }
    }

    fn source_unchecked(root: &Path, detection: ChangeDetection) -> FsSource {
        FsSource::for_search(root.to_str().unwrap().to_string(), 1, Arc::new(FsContext::new(detection))).unwrap()
    }

    #[test]
    fn changed_ignore_file_falls_back_to_full_walk() {
        for mode in MODES {
            let dir = repo();
            let root = dir.path();
            let snap = source(root, mode).list(None).unwrap().snapshot.unwrap();
            std::fs::write(root.join(".gitignore"), "target/\n").unwrap(); // *.log now eligible
            let after = relist(root, mode, &snap);
            assert!(after.full_scan, "{mode:?}");
            assert!(paths(after.files).contains(&"src/debug.log".to_string()), "{mode:?}");
        }
    }

    #[test]
    fn rejects_relative_roots() {
        let ctx = Arc::new(FsContext::new(ChangeDetection::Scan));
        assert!(FsSource::new(Path::new("relative/dir"), 1, ctx).is_err());
    }

    /// Random edits, chained through persisted snapshots, must always list exactly what a
    /// full walk lists.
    #[test]
    fn random_changes_always_match_full_walk() {
        const NAMES: [&str; 8] = ["a.rs", "b.txt", "c.log", "build", "d", ".h", "e.md", "f"];
        const DIRS: [&str; 7] = ["", "src", "src/x", "src/x/y", "lib", "build", "lib/z"];
        for (seed, mode) in [1u64, 2, 3, 4].into_iter().flat_map(|s| MODES.map(|m| (s, m))) {
            let mut state = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(seed);
            let mut next = move |n: usize| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % n as u64) as usize
            };
            let dir = repo();
            let root = dir.path();
            let mut snap = source(root, mode).list(None).unwrap().snapshot.unwrap();
            for round in 0..60 {
                for _ in 0..=next(4) {
                    let parent = root.join(DIRS[next(DIRS.len())]);
                    let path = parent.join(NAMES[next(NAMES.len())]);
                    let _ = match next(7) {
                        0 | 1 => {
                            std::fs::create_dir_all(&parent).and_then(|()| std::fs::write(&path, format!("r{round}")))
                        }
                        2 => std::fs::remove_file(&path),
                        3 => std::fs::create_dir_all(&path),
                        4 => std::fs::remove_dir_all(&path),
                        5 => std::fs::rename(&path, parent.join(NAMES[next(NAMES.len())])),
                        _ => std::fs::remove_dir_all(&parent).and_then(|()| std::fs::create_dir_all(&parent)),
                    };
                }
                check_unchanged(root, mode, &snap, &format!("{mode:?} seed {seed} round {round}"));
                let fresh = relist(root, mode, &snap);
                assert_eq!(sorted(fresh.files), full(root), "{mode:?} seed {seed} round {round}");
                if let Some(s) = fresh.snapshot {
                    snap = s;
                }
            }
        }
    }

    /// The point of the journal: only the directories it names are listed, and a change
    /// that keeps size and mtime is still noticed.
    #[cfg(windows)]
    #[test]
    fn journal_refresh_lists_only_changed_directories() {
        let dir = repo();
        let root = dir.path();
        let snap = source(root, ChangeDetection::Journal).list(None).unwrap().snapshot.unwrap();
        let decoded = snapshot::decode(&snap).unwrap().unwrap();
        assert!(decoded.journal.is_some(), "temp dir volume should have a change journal");

        let idle = relist(root, ChangeDetection::Journal, &snap);
        assert!(!idle.full_scan);
        assert!(idle.snapshot.is_none(), "nothing changed, nothing to persist");
        assert!(check_unchanged(root, ChangeDetection::Journal, &snap, "idle"), "an idle repo is proven unchanged");
        assert!(!check_unchanged(root, ChangeDetection::Scan, &snap, "scan mode"), "scan mode proves nothing");

        std::fs::write(root.join("src/nested/added.rs"), "added").unwrap();
        assert!(!check_unchanged(root, ChangeDetection::Journal, &snap, "added file"));
        let fresh = relist(root, ChangeDetection::Journal, &snap);
        assert!(!fresh.full_scan);
        assert_eq!(sorted(fresh.files), full(root));
    }
}
