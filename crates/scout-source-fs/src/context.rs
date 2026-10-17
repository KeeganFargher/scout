//! State shared by every [`crate::FsSource`] of one process.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use scout_core::{Error, Result};

use crate::journal::Feed;
use crate::snapshot::{Cursor, Head, Snapshot, Watched};

/// How a refresh finds the directories that changed since the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeDetection {
    /// Ask the volume's change journal (Windows NTFS/ReFS) and re-list only the
    /// directories it names. Volumes without a journal, and snapshots whose journal
    /// position has been overwritten, are scanned instead.
    Journal,
    /// Re-list every known directory.
    Scan,
}

/// What a refresh has to look at.
pub(crate) enum Plan {
    /// Every directory. `cursor` is the journal position the result will be valid at.
    Scan { cursor: Option<Cursor> },
    /// Only these directories (indices into `Snapshot::dirs`) changed since the snapshot.
    Journal { touched: HashSet<usize>, cursor: Cursor, persist_cursor: bool },
}

/// Shared across repos: one journal read per volume serves every repo on it, and the
/// watch list (mostly the same ancestor ignore files and git config for every repo) is
/// stat'ed once per path instead of once per repo.
pub struct FsContext {
    detection: ChangeDetection,
    stats: Mutex<HashMap<PathBuf, Option<(u64, u64)>>>,
    feeds: Mutex<HashMap<String, Option<Arc<Feed>>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FsContext {
    #[must_use]
    pub fn new(detection: ChangeDetection) -> Self {
        Self { detection, stats: Mutex::new(HashMap::new()), feeds: Mutex::new(HashMap::new()) }
    }

    pub(crate) fn stat(&self, path: &Path) -> Option<(u64, u64)> {
        if let Some(s) = lock(&self.stats).get(path) {
            return *s;
        }
        let s = crate::snapshot::stat(path);
        lock(&self.stats).insert(path.to_path_buf(), s);
        s
    }

    pub(crate) fn watch_changed(&self, watch: &[Watched]) -> bool {
        watch.iter().any(|w| self.stat(&w.path) != w.state)
    }
}

/// Where a snapshot stands relative to its volume's journal.
enum Window {
    /// The journal cannot narrow anything down: look at everything. `cursor` is the
    /// position the result will be valid at.
    Scan { cursor: Option<Cursor> },
    /// Every change since `from` is readable from `feed`.
    Journal { feed: Arc<Feed>, from: i64, cursor: Cursor, persist_cursor: bool },
}

impl FsContext {
    fn feed(&self, volume: &str) -> Result<Option<Arc<Feed>>> {
        let mut feeds = lock(&self.feeds);
        if let Some(f) = feeds.get(volume) {
            return Ok(f.clone());
        }
        let f = match crate::os::Journal::open(volume) {
            Ok(j) => Some(Arc::new(Feed::new(j))),
            // FAT, network shares, or a disabled journal: those volumes are scanned.
            Err(e) if crate::os::is_unsupported(&e) => None,
            Err(e) => return Err(Error::io(volume, e)),
        };
        feeds.insert(volume.to_string(), f.clone());
        Ok(f)
    }

    /// The journal position a state recorded from now on is valid at. Taken before the
    /// caller looks at the directories, so changes made while it looks are replayed by the
    /// next refresh rather than lost.
    pub(crate) fn cursor(&self, root: &Path) -> Result<Option<Cursor>> {
        if self.detection == ChangeDetection::Scan {
            return Ok(None);
        }
        let volume = crate::os::volume_of(root).map_err(|e| Error::io(root, e))?;
        Ok(self.feed(&volume)?.map(|f| f.cursor(volume)))
    }

    fn window(&self, root: &Path, journal: Option<&Cursor>) -> Result<Window> {
        // Every search reads the journal from the oldest cursor, at ~5.6 ms per MiB of
        // records (measured), and that read is on every search's critical path. Advancing a
        // cursor costs re-encoding the snapshot plus one write of the shared snapshot pack
        // (all repos advance together, so it is one write). At 256 KiB the read stays
        // under ~1.5 ms and the write happens about once per 256 KiB of volume activity.
        const PERSIST_LAG: i64 = 256 << 10;
        let Some(old) = journal else {
            return Ok(Window::Scan { cursor: self.cursor(root)? });
        };
        if self.detection == ChangeDetection::Scan {
            return Ok(Window::Scan { cursor: None });
        }
        let Some(feed) = self.feed(&old.volume)? else {
            return Ok(Window::Scan { cursor: None });
        };
        let cursor = feed.cursor(old.volume.clone());
        if !feed.covers(old) {
            return Ok(Window::Scan { cursor: Some(cursor) });
        }
        let persist_cursor = cursor.usn - old.usn > PERSIST_LAG;
        Ok(Window::Journal { feed, from: old.usn, cursor, persist_cursor })
    }

    pub(crate) fn plan(&self, root: &Path, snap: &Snapshot) -> Result<Plan> {
        match self.window(root, snap.journal.as_ref())? {
            Window::Scan { cursor } => Ok(Plan::Scan { cursor }),
            Window::Journal { feed, from, cursor, persist_cursor } => {
                let seen = feed.since(from)?;
                if seen.moved(snap.root_id(), &snap.ancestors, from) {
                    // The repo root or one of its ancestors was renamed or deleted: every
                    // path may have changed, so look at everything.
                    return Ok(Plan::Scan { cursor: Some(cursor) });
                }
                let touched = snap
                    .dirs
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| seen.changed_under(d.id, from))
                    .map(|(i, _)| i)
                    .collect();
                Ok(Plan::Journal { touched, cursor, persist_cursor })
            }
        }
    }

    /// Whether the journal proves that none of the snapshot's directories changed, so a
    /// refresh would return the snapshot unchanged. `false` also when the snapshot's
    /// cursor should be advanced: only `list` can persist that.
    pub(crate) fn untouched(&self, root: &Path, head: &Head) -> Result<bool> {
        // Same answer as the Scan arm below, but without `window` asking for a fresh
        // cursor, which can open the volume journal for nothing.
        if head.journal.is_none() {
            return Ok(false);
        }
        match self.window(root, head.journal.as_ref())? {
            Window::Scan { .. } | Window::Journal { persist_cursor: true, .. } => Ok(false),
            Window::Journal { feed, from, .. } => {
                let seen = feed.since(from)?;
                Ok(!seen.moved(head.root_id, &head.ancestors, from)
                    && !head.dir_ids.iter().any(|&id| seen.changed_under(id, from)))
            }
        }
    }
}
