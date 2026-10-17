//! Brings a snapshot up to date by re-listing the directories that may have changed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use scout_core::{Error, Result};

use crate::dirs::{self, Kind};
use crate::snapshot::{self, DirRec, FileRec, FileState, IGNORE_FILES, Snapshot, rejected_key};
use crate::{FsSource, join_rel, mtime_ns, now_ns};

pub(crate) enum Select {
    /// Every directory (a scan): directories without files may be skipped by mtime.
    All,
    /// Only these directories (indices into `Snapshot::dirs`), named by the change journal.
    /// Child directory ids are compared, because a directory replaced by another one of the
    /// same name produces journal records only under the *new* id.
    Only(HashSet<usize>),
}

pub(crate) enum Outcome {
    /// An ignore file inside the repo changed: any decision below it may differ.
    IgnoreChanged,
    Refreshed {
        snapshot: Snapshot,
        changed: bool,
    },
}

enum Listed {
    /// Directory without files whose mtime is unchanged: no name can have changed.
    Skipped,
    /// No longer exists; its subtree is gone with it.
    Gone(String),
    IgnoreChanged,
    Updated(Update),
}

struct Update {
    index: usize,
    rec: DirRec,
    changed: bool,
    /// Children the snapshot does not know, decided by a walker afterwards.
    new: Vec<NewEntry>,
    /// Known subdirectories that disappeared or were replaced by another directory.
    removed: Vec<String>,
    /// Current ids of the child directories that are still the same directory.
    child_ids: Vec<(String, u128)>,
}

struct NewEntry {
    rel: String,
    name: String,
    kind: Kind,
}

struct Index<'a> {
    /// Id of every known directory, by relative path.
    known: HashMap<&'a str, u128>,
    /// Known subdirectories of every directory.
    children: HashMap<&'a str, Vec<&'a str>>,
}

fn parent_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(p, _)| p)
}

fn child_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
}

fn not_found_below_root(e: &std::io::Error, rec: &DirRec) -> bool {
    // A missing repo root is an error; silently listing nothing would hide it.
    crate::is_gone(e) && !rec.rel.is_empty()
}

fn list_dir(root: &Path, rec: &DirRec, index: usize, ix: &Index<'_>, select: &Select) -> Result<Listed> {
    let abs = join_rel(root, &rec.rel);
    let fileless = rec.files.is_empty() && rec.ignore_files.is_empty();
    // Stat before listing: in a scan an unchanged mtime skips the listing, and the mtime
    // recorded is from before the listing, so a change made during it is seen next time.
    let pre_mtime = if fileless {
        match std::fs::metadata(&abs) {
            Ok(m) => Some(mtime_ns(&m)),
            Err(e) if not_found_below_root(&e, rec) => return Ok(Listed::Gone(rec.rel.clone())),
            Err(e) => return Err(Error::io(abs, e)),
        }
    } else {
        None
    };
    if matches!(select, Select::All) && rec.mtime_ns != 0 && pre_mtime == Some(rec.mtime_ns) {
        return Ok(Listed::Skipped);
    }
    let entries = match dirs::read_dir(&abs) {
        Ok(e) => e,
        Err(e) if not_found_below_root(&e, rec) => return Ok(Listed::Gone(rec.rel.clone())),
        Err(e) => return Err(Error::io(abs, e)),
    };
    let compare_ids = matches!(select, Select::Only(_));
    let mut out = Update {
        index,
        rec: rec.clone_header(),
        changed: false,
        new: Vec::new(),
        removed: Vec::new(),
        child_ids: Vec::new(),
    };
    let mut ignore_files = Vec::new();
    let mut present: HashSet<String> = HashSet::new();
    for e in entries {
        if e.kind == Kind::File && IGNORE_FILES.contains(&e.name.as_str()) {
            ignore_files.push(FileState { name: e.name.clone(), size: e.size, mtime_ns: e.mtime_ns });
        }
        let key = rejected_key(e.kind, &e.name);
        // Paths must be UTF-8, so a non-Unicode name can never be accepted.
        if e.lossy || rec.rejected.binary_search(&key).is_ok() {
            out.rec.rejected.push(key);
            continue;
        }
        let rel = child_rel(&rec.rel, &e.name);
        match e.kind {
            Kind::Dir => match ix.known.get(rel.as_str()) {
                Some(&id) if !compare_ids || id == e.id => {
                    present.insert(rel.clone());
                    out.child_ids.push((rel, e.id));
                }
                Some(_) => {
                    out.removed.push(rel.clone());
                    out.new.push(NewEntry { rel, name: e.name, kind: e.kind });
                }
                None => out.new.push(NewEntry { rel, name: e.name, kind: e.kind }),
            },
            Kind::File if rec.files.binary_search_by(|f| f.name.as_str().cmp(&e.name)).is_ok() => {
                out.rec.files.push(FileRec { name: e.name, size: e.size, mtime_ns: e.mtime_ns });
            }
            Kind::File | Kind::Other => out.new.push(NewEntry { rel, name: e.name, kind: e.kind }),
        }
    }
    for child in ix.children.get(rec.rel.as_str()).into_iter().flatten() {
        if !present.contains(*child) && !out.removed.iter().any(|r| r == child) {
            out.removed.push((*child).to_string());
        }
    }
    ignore_files.sort_by(|a, b| a.name.cmp(&b.name));
    if ignore_files != rec.ignore_files {
        return Ok(Listed::IgnoreChanged);
    }
    out.rec.ignore_files = ignore_files;
    out.rec.files.sort_by(|a, b| a.name.cmp(&b.name));
    out.rec.rejected.sort();
    out.changed = out.rec.files != rec.files
        || out.rec.rejected != rec.rejected
        || !out.removed.is_empty()
        || !out.new.is_empty();
    out.rec.mtime_ns = match pre_mtime {
        Some(m) if out.rec.files.is_empty() => m,
        _ => 0,
    };
    Ok(Listed::Updated(out))
}

impl DirRec {
    /// The fields that identify a directory record, with empty contents.
    fn clone_header(&self) -> DirRec {
        DirRec {
            rel: self.rel.clone(),
            id: self.id,
            files: Vec::new(),
            rejected: Vec::new(),
            ignore_files: Vec::new(),
            mtime_ns: 0,
        }
    }
}

fn group(files: &[scout_core::ports::FileMeta]) -> BTreeMap<String, Vec<String>> {
    let mut accepted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in files {
        let (parent, name) = f.path.rsplit_once('/').unwrap_or(("", &f.path));
        accepted.entry(parent.to_string()).or_default().push(name.to_string());
    }
    accepted
}

/// Decides `u.new` with the real walker, scoped to the directory, so the cost stays
/// proportional to what changed. Returns the records of newly accepted subtrees.
fn resolve_new(src: &FsSource, u: &mut Update) -> Result<Vec<DirRec>> {
    let abs = join_rel(&src.root, &u.rec.rel);
    let started = now_ns()?;
    let shallow = src.walk(&abs, Some(1))?;
    // Same rule as when recording: if the directory changed while the walker read it, a
    // name it did not yield may simply be newer than the walk. Record no rejection then;
    // the name stays unknown and is decided again by the next refresh.
    let unstable = std::fs::metadata(&abs).map_or(true, |m| mtime_ns(&m) >= started);
    let mut new_recs = Vec::new();
    for n in std::mem::take(&mut u.new) {
        match n.kind {
            Kind::Dir if shallow.dirs.contains(&n.rel) => {
                let started = now_ns()?;
                let walked = src.walk(&join_rel(&src.root, &n.rel), None)?;
                let mut sorted: Vec<&String> = walked.dirs.iter().collect();
                sorted.sort();
                let recs =
                    snapshot::record_dirs(&src.root, sorted.into_iter(), &walked.dirs, &group(&walked.files), started)
                        .map_err(|e| Error::io(join_rel(&src.root, &n.rel), e))?;
                new_recs.extend(recs);
            }
            Kind::File => match shallow.files.iter().find(|f| f.path == n.rel) {
                Some(f) => u.rec.files.push(FileRec { name: n.name, size: f.size, mtime_ns: f.mtime_ns }),
                None if !unstable => u.rec.rejected.push(rejected_key(n.kind, &n.name)),
                None => {}
            },
            _ if !unstable => u.rec.rejected.push(rejected_key(n.kind, &n.name)),
            _ => {}
        }
    }
    u.rec.files.sort_by(|a, b| a.name.cmp(&b.name));
    u.rec.rejected.sort();
    Ok(new_recs)
}

/// Re-lists the selected directories of `snap` and returns the updated snapshot. The
/// caller sets its journal cursor.
pub(crate) fn refresh(src: &FsSource, snap: Snapshot, select: &Select) -> Result<Outcome> {
    // Listing is syscall-bound, so many directories are split across threads. Few stay
    // on one thread: starting threads would cost more than it saves.
    const DIRS_PER_THREAD: usize = 64;
    let indices: Vec<usize> = match select {
        Select::All => (0..snap.dirs.len()).collect(),
        Select::Only(set) => {
            let mut v: Vec<usize> = set.iter().copied().collect();
            v.sort_unstable();
            v
        }
    };
    if indices.is_empty() {
        return Ok(Outcome::Refreshed { snapshot: snap, changed: false });
    }
    let ix = Index {
        known: snap.dirs.iter().map(|d| (d.rel.as_str(), d.id)).collect(),
        children: {
            let mut c: HashMap<&str, Vec<&str>> = HashMap::new();
            for d in snap.dirs.iter().filter(|d| !d.rel.is_empty()) {
                c.entry(parent_of(&d.rel)).or_default().push(&d.rel);
            }
            c
        },
    };
    let threads = src.threads.min(indices.len().div_ceil(DIRS_PER_THREAD)).max(1);
    let chunk = indices.len().div_ceil(threads).max(1);
    let parts: Vec<Result<Vec<Listed>>> = std::thread::scope(|s| {
        let handles: Vec<_> = indices
            .chunks(chunk)
            .map(|part| {
                let (ix, snap) = (&ix, &snap);
                s.spawn(move || part.iter().map(|&i| list_dir(&src.root, &snap.dirs[i], i, ix, select)).collect())
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(Error::Invalid("directory listing thread panicked".into()))))
            .collect()
    });
    let mut updates = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for part in parts {
        for listed in part? {
            match listed {
                Listed::Skipped => {}
                Listed::Gone(rel) => dropped.push(rel),
                Listed::IgnoreChanged => return Ok(Outcome::IgnoreChanged),
                Listed::Updated(u) => updates.push(u),
            }
        }
    }
    for u in &mut updates {
        dropped.extend(std::mem::take(&mut u.removed));
    }
    let is_dropped = |rel: &str| {
        dropped.iter().any(|p| {
            rel == p || (rel.len() > p.len() && rel.starts_with(p.as_str()) && rel.as_bytes()[p.len()] == b'/')
        })
    };
    // A dropped directory's own update describes whatever now sits at its path (in the
    // journal case, possibly a different directory that replaced it). Its parent already
    // decided that path as a new entry, so the update must not add anything itself.
    updates.retain(|u| !is_dropped(&u.rec.rel));
    let mut new_recs = Vec::new();
    for u in &mut updates {
        if !u.new.is_empty() {
            new_recs.extend(resolve_new(src, u)?);
        }
    }
    let mut changed = !dropped.is_empty() || !new_recs.is_empty() || updates.iter().any(|u| u.changed);
    let child_ids: HashMap<String, u128> = updates.iter_mut().flat_map(|u| std::mem::take(&mut u.child_ids)).collect();
    let mut replaced: HashMap<usize, DirRec> = updates.into_iter().map(|u| (u.index, u.rec)).collect();
    drop(ix);
    let Snapshot { dirs: old_dirs, watch, ancestors, journal } = snap;
    let mut dirs: Vec<DirRec> = Vec::with_capacity(old_dirs.len() + new_recs.len());
    for (i, old) in old_dirs.into_iter().enumerate() {
        if is_dropped(&old.rel) {
            continue;
        }
        let mut rec = replaced.remove(&i).unwrap_or(old);
        if let Some(&id) = child_ids.get(&rec.rel) {
            rec.id = id;
        }
        dirs.push(rec);
    }
    dirs.extend(new_recs);
    dirs.sort_by(|a, b| a.rel.cmp(&b.rel));
    if dirs.windows(2).any(|w| w[0].rel == w[1].rel) {
        return Err(Error::Invalid(format!("snapshot refresh of {} produced duplicate directories", src.display)));
    }
    let mut ancestors = ancestors;
    if matches!(select, Select::All) {
        // A scan may follow a rename of the root or an ancestor: record current ids so the
        // journal can be used again from here.
        let root_id = dirs::dir_id(&src.root).map_err(|e| Error::io(&src.root, e))?;
        let now_ancestors = snapshot::ancestor_ids(&src.root).map_err(|e| Error::io(&src.root, e))?;
        if let Some(r) = dirs.first_mut().filter(|d| d.rel.is_empty()) {
            changed |= r.id != root_id;
            r.id = root_id;
        }
        changed |= now_ancestors != ancestors;
        ancestors = now_ancestors;
    }
    Ok(Outcome::Refreshed { snapshot: Snapshot { dirs, watch, ancestors, journal }, changed })
}
