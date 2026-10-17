//! A volume change journal, read lazily and shared by every repo on the volume.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use scout_core::{Error, Result};

use crate::os::Journal;
use crate::snapshot::Cursor;

/// What the journal says happened since `from`, as "latest USN per id": a directory
/// changed since a cursor iff its latest record is at or after that cursor.
pub(crate) struct Seen {
    from: i64,
    /// Latest record of any child, by parent directory id.
    by_parent: HashMap<u128, i64>,
    /// Latest rename/delete record, by the id of the renamed or deleted entry.
    by_id: HashMap<u128, i64>,
}

impl Seen {
    /// A child of directory `dir` was created, changed, renamed or deleted since `from`.
    pub(crate) fn changed_under(&self, dir: u128, from: i64) -> bool {
        self.by_parent.get(&dir).is_some_and(|&u| u >= from)
    }

    /// The repo root or one of its ancestors was renamed or deleted since `from`.
    pub(crate) fn moved(&self, root_id: Option<u128>, ancestors: &[u128], from: i64) -> bool {
        root_id.iter().chain(ancestors).any(|id| self.by_id.get(id).is_some_and(|&u| u >= from))
    }
}

pub(crate) struct Feed {
    journal: Journal,
    seen: Mutex<Seen>,
}

impl Feed {
    pub(crate) fn new(journal: Journal) -> Self {
        let from = journal.next();
        Self { journal, seen: Mutex::new(Seen { from, by_parent: HashMap::new(), by_id: HashMap::new() }) }
    }

    pub(crate) fn cursor(&self, volume: String) -> Cursor {
        Cursor { volume, journal_id: self.journal.id(), usn: self.journal.next() }
    }

    /// Whether every record since `c` is still in the journal.
    pub(crate) fn covers(&self, c: &Cursor) -> bool {
        c.journal_id == self.journal.id() && c.usn >= self.journal.lowest_valid() && c.usn <= self.journal.next()
    }

    /// Records since `from`, reading the journal only for the part not read yet.
    pub(crate) fn since(&self, from: i64) -> Result<MutexGuard<'_, Seen>> {
        let mut seen = self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if from < seen.from {
            let to = seen.from;
            let Seen { by_parent, by_id, .. } = &mut *seen;
            self.journal
                .read(from, to, |usn, id, parent, structural| {
                    let p = by_parent.entry(parent).or_insert(usn);
                    *p = (*p).max(usn);
                    if structural {
                        let e = by_id.entry(id).or_insert(usn);
                        *e = (*e).max(usn);
                    }
                })
                .map_err(|e| Error::io("change journal", e))?;
            seen.from = from;
        }
        Ok(seen)
    }
}
