//! Directory snapshot: the complete listing state of a repo, so a repeat listing only has
//! to look at what changed.
//!
//! Recorded during a full walk:
//! - every directory the walker entered, with its file id, the files it accepted (with
//!   size and mtime) and the child names it rejected (ignored, hidden, links);
//! - the state of every file that can change ignore decisions from outside the
//!   directories: ancestors' ignore files, `.git/info/exclude`, the global gitignore and
//!   the git config files that locate it;
//! - the file ids of the repo's ancestors, and (Windows) the change-journal position the
//!   state is valid at.
//!
//! The listing *is* the snapshot's files: a refresh updates the records of directories
//! that changed and the listing follows. Unknown names (new files or directories) are
//! decided by a real walker scoped to their directory; a changed watched file makes the
//! caller do a full walk. A refreshed listing therefore always equals a full walk; only
//! the cost differs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use scout_core::ports::{FileMeta, ListingDigest};
use serde::{Deserialize, Serialize};

use crate::dirs::{self, Kind};

pub const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".rgignore"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Sorted by `rel`, so `""` (the root) comes first and a directory's subtree follows it.
    pub dirs: Vec<DirRec>,
    pub watch: Vec<Watched>,
    /// File ids of the root's ancestors. A journal record renaming or deleting one of them
    /// changes every path in the repo.
    pub ancestors: Vec<u128>,
    /// Where in the volume's change journal this state is valid; `None` without a journal.
    pub journal: Option<Cursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// `\\?\Volume{GUID}\` name of the volume.
    pub volume: String,
    pub journal_id: u64,
    /// Every change before this USN is reflected in the snapshot.
    pub usn: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirRec {
    /// Relative to the repo root, `/`-separated; `""` is the root.
    pub rel: String,
    /// Change-journal file id (0 where unsupported).
    pub id: u128,
    /// Accepted files, sorted by name.
    pub files: Vec<FileRec>,
    /// Child entries the walker did not yield, as [`rejected_key`]s, sorted.
    pub rejected: Vec<String>,
    /// Ignore files present in this directory with their state when recorded.
    pub ignore_files: Vec<FileState>,
    /// Directory mtime, recorded only for directories without files or ignore files (the
    /// only ones a directory scan may skip by mtime); 0 means "never trust".
    /// Creating, deleting or renaming an entry updates the directory's mtime, so an
    /// unchanged mtime proves the set of names is unchanged.
    pub mtime_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRec {
    pub name: String,
    pub size: u64,
    pub mtime_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub name: String,
    pub size: u64,
    pub mtime_ns: u64,
}

/// A file outside the enumerated directories whose change invalidates the snapshot.
/// `state == None` records that it did not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    pub path: PathBuf,
    pub state: Option<(u64, u64)>,
}

impl Snapshot {
    /// The listing this snapshot describes.
    #[must_use]
    pub fn files(&self) -> Vec<FileMeta> {
        let mut out = Vec::with_capacity(self.dirs.iter().map(|d| d.files.len()).sum());
        for d in &self.dirs {
            for f in &d.files {
                let path = if d.rel.is_empty() { f.name.clone() } else { format!("{}/{}", d.rel, f.name) };
                out.push(FileMeta { path, size: f.size, mtime_ns: f.mtime_ns });
            }
        }
        out
    }
}

/// Key of a rejected entry: its kind and name. The kind matters because ignore rules can
/// depend on it (`build/` rejects a directory but not a file called `build`), so a name
/// that comes back as a different kind must be decided again.
pub(crate) fn rejected_key(kind: Kind, name: &str) -> String {
    let tag = match kind {
        Kind::File => "f:",
        Kind::Dir => "d:",
        Kind::Other => "l:",
    };
    format!("{tag}{name}")
}

#[must_use]
pub fn stat(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), crate::mtime_ns(&m)))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Files outside the repo's own directories that influence the walker's decisions.
pub fn watch_list(root: &Path, stat: impl Fn(&Path) -> Option<(u64, u64)>) -> Vec<Watched> {
    let mut paths: Vec<PathBuf> = vec![root.join(".git").join("info").join("exclude")];
    paths.extend(ignore::gitignore::gitconfig_excludes_path());
    if let Some(home) = home_dir() {
        paths.push(home.join(".gitconfig"));
        paths.push(home.join(".config").join("git").join("config"));
    }
    if let Some(p) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        paths.push(PathBuf::from(p));
    }
    for ancestor in root.ancestors().skip(1) {
        for name in IGNORE_FILES {
            paths.push(ancestor.join(name));
        }
    }
    paths.into_iter().map(|path| Watched { state: stat(&path), path }).collect()
}

pub(crate) fn ancestor_ids(root: &Path) -> std::io::Result<Vec<u128>> {
    root.ancestors().skip(1).map(dirs::dir_id).collect()
}

/// Records `dirs` (relative paths) from the outcome of a walk.
///
/// `walked_dirs` are all directories the walk yielded (it decides which child names are
/// subdirectories rather than rejected names), `accepted` the accepted file names
/// grouped by parent directory. A directory whose mtime is at or after `walk_started_ns`
/// changed while the walk ran: a name the walk did not yield there may be new rather than
/// rejected, so none are recorded as rejected (each is re-decided later, by a real
/// walker) and its mtime is never trusted.
pub(crate) fn record_dirs<'a>(
    root: &Path,
    dirs: impl Iterator<Item = &'a String>,
    walked_dirs: &HashSet<String>,
    accepted: &BTreeMap<String, Vec<String>>,
    walk_started_ns: u64,
) -> std::io::Result<Vec<DirRec>> {
    let dirs: Vec<&String> = dirs.collect();
    let mut child_ids: HashMap<String, u128> = HashMap::new();
    let mut recs = Vec::with_capacity(dirs.len());
    for rel in &dirs {
        let abs = crate::join_rel(root, rel);
        // A directory that vanished after the walk entered it is simply not recorded: its
        // files are gone, and its parent (changed after the walk started) is re-decided.
        let dir_mtime = match std::fs::metadata(&abs) {
            Ok(m) if m.is_dir() => crate::mtime_ns(&m),
            Ok(_) if !rel.is_empty() => continue,
            Err(e) if crate::is_gone(&e) && !rel.is_empty() => continue,
            Ok(_) => {
                return Err(std::io::Error::new(std::io::ErrorKind::NotADirectory, "repo root is not a directory"));
            }
            Err(e) => return Err(e),
        };
        let unstable = dir_mtime >= walk_started_ns;
        let listing = match dirs::read_dir(&abs) {
            Ok(l) => l,
            Err(e) if crate::is_gone(&e) && !rel.is_empty() => continue,
            Err(e) => return Err(e),
        };
        let names: HashSet<&str> =
            accepted.get(*rel).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default();
        let mut files = Vec::new();
        let mut rejected = Vec::new();
        let mut ignore_files = Vec::new();
        for e in listing {
            let child = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
            if e.kind == Kind::Dir {
                child_ids.insert(child.clone(), e.id);
            }
            if e.kind == Kind::File && IGNORE_FILES.contains(&e.name.as_str()) {
                ignore_files.push(FileState { name: e.name.clone(), size: e.size, mtime_ns: e.mtime_ns });
            }
            if !e.lossy && e.kind == Kind::File && names.contains(e.name.as_str()) {
                files.push(FileRec { name: e.name, size: e.size, mtime_ns: e.mtime_ns });
            } else if !unstable && (e.kind != Kind::Dir || !walked_dirs.contains(&child)) {
                rejected.push(rejected_key(e.kind, &e.name));
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        rejected.sort();
        ignore_files.sort_by(|a, b| a.name.cmp(&b.name));
        let fileless = files.is_empty() && ignore_files.is_empty();
        let mtime_ns = if unstable || !fileless { 0 } else { dir_mtime };
        recs.push(DirRec { rel: (*rel).clone(), id: 0, files, rejected, ignore_files, mtime_ns });
    }
    // A directory's id comes from its parent's listing; only directories whose parent is
    // not part of this batch (the root, or the top of a new subtree) need their own open.
    for r in &mut recs {
        r.id = match child_ids.get(&r.rel) {
            Some(&id) => id,
            None => dirs::dir_id(&crate::join_rel(root, &r.rel))?,
        };
    }
    Ok(recs)
}

/// Builds a snapshot from the outcome of a full walk.
pub(crate) fn build(
    root: &Path,
    walked_dirs: &HashSet<String>,
    accepted: &BTreeMap<String, Vec<String>>,
    walk_started_ns: u64,
    journal: Option<Cursor>,
    watch: Vec<Watched>,
) -> std::io::Result<Snapshot> {
    let mut sorted: Vec<&String> = walked_dirs.iter().collect();
    sorted.sort();
    let dirs = record_dirs(root, sorted.into_iter(), walked_dirs, accepted, walk_started_ns)?;
    Ok(Snapshot { dirs, watch, ancestors: ancestor_ids(root)?, journal })
}

/// Leads every encoded snapshot; the last byte is the format version. Anything else
/// (an older snapshot, a future version) is "another format": not used, replaced by a full
/// walk.
const MAGIC: [u8; 8] = *b"SCOUTSN";

/// Encoded ahead of the directory records and decodable alone ([`decode_head`]): all a
/// search needs to prove that nothing changed since indexing. Most repos are untouched
/// between searches, and for them the directory records (90% of the bytes) are never
/// decoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
    /// [`scout_core::ports::ListingDigest`] of [`Snapshot::files`].
    pub digest: u128,
    /// File id of the repo root, if it was recorded.
    pub root_id: Option<u128>,
    pub ancestors: Vec<u128>,
    pub journal: Option<Cursor>,
    pub watch: Vec<Watched>,
    /// File ids of every recorded directory.
    pub dir_ids: Vec<u128>,
}

impl Snapshot {
    #[must_use]
    pub fn root_id(&self) -> Option<u128> {
        self.dirs.first().filter(|d| d.rel.is_empty()).map(|d| d.id)
    }

    /// The [`scout_core::ports::ListingDigest`] of [`Self::files`], without building paths.
    #[must_use]
    pub fn digest(&self) -> u128 {
        let mut d = ListingDigest::default();
        for dir in &self.dirs {
            for f in &dir.files {
                if dir.rel.is_empty() {
                    d.add(&[&f.name], f.size, f.mtime_ns);
                } else {
                    d.add(&[&dir.rel, "/", &f.name], f.size, f.mtime_ns);
                }
            }
        }
        d.finish()
    }

    fn head(&self) -> Head {
        Head {
            digest: self.digest(),
            root_id: self.root_id(),
            ancestors: self.ancestors.clone(),
            journal: self.journal.clone(),
            watch: self.watch.clone(),
            dir_ids: self.dirs.iter().map(|d| d.id).collect(),
        }
    }
}

// bincode rather than JSON: every strict search decodes snapshots, and JSON decoding was
// ~40 ms of thread time per search across the bench corpus (E8a). The cost is a format
// nobody can read by eye; `Snapshot` derives Debug for that.
fn config() -> impl bincode::config::Config {
    bincode::config::standard()
}

pub fn encode(s: &Snapshot) -> Result<Vec<u8>, bincode::error::EncodeError> {
    let mut out = MAGIC.to_vec();
    bincode::serde::encode_into_std_write((&s.head(), &s.dirs), &mut out, config())?;
    Ok(out)
}

/// `Ok(None)` for a snapshot in another format (it is simply not used); an error for
/// bytes that claim this format but do not decode, which means the store is corrupt.
pub fn decode(bytes: &[u8]) -> Result<Option<Snapshot>, bincode::error::DecodeError> {
    let Some(payload) = bytes.strip_prefix(&MAGIC) else {
        return Ok(None);
    };
    let ((head, dirs), used): ((Head, Vec<DirRec>), usize) = bincode::serde::decode_from_slice(payload, config())?;
    if used != payload.len() {
        return Err(bincode::error::DecodeError::OtherString(format!("{} trailing bytes", payload.len() - used)));
    }
    Ok(Some(Snapshot { dirs, watch: head.watch, ancestors: head.ancestors, journal: head.journal }))
}

/// Decodes only the [`Head`]; same `Ok(None)` rule as [`decode`].
pub fn decode_head(bytes: &[u8]) -> Result<Option<Head>, bincode::error::DecodeError> {
    let Some(payload) = bytes.strip_prefix(&MAGIC) else {
        return Ok(None);
    };
    let (head, _): (Head, usize) = bincode::serde::decode_from_slice(payload, config())?;
    Ok(Some(head))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            dirs: vec![
                DirRec {
                    rel: String::new(),
                    id: u128::MAX,
                    files: vec![FileRec { name: "a.rs".into(), size: 3, mtime_ns: 7 }],
                    rejected: vec!["d:target".into()],
                    ignore_files: vec![],
                    mtime_ns: 0,
                },
                DirRec {
                    rel: "src".into(),
                    id: 5,
                    files: vec![FileRec { name: "b.rs".into(), size: 1, mtime_ns: 2 }],
                    rejected: vec![],
                    ignore_files: vec![],
                    mtime_ns: 0,
                },
            ],
            watch: vec![Watched { path: PathBuf::from("x/.gitignore"), state: None }],
            ancestors: vec![1, 2],
            journal: Some(Cursor { volume: "v".into(), journal_id: 9, usn: -1 }),
        }
    }

    #[test]
    fn round_trips() {
        let bytes = encode(&sample()).unwrap();
        let decoded = decode(&bytes).unwrap().unwrap();
        assert_eq!(format!("{decoded:?}"), format!("{:?}", sample()));
        let head = decode_head(&bytes).unwrap().unwrap();
        assert_eq!(head.digest, sample().digest());
        assert_eq!(head.root_id, Some(u128::MAX));
        assert_eq!(head.dir_ids, vec![u128::MAX, 5]);
    }

    /// The head digest must equal the digest the indexer computes from the listing, or
    /// the unchanged fast path never fires (or, worse, fires wrongly).
    #[test]
    fn digest_matches_the_listing() {
        let mut d = ListingDigest::default();
        for f in sample().files() {
            d.add(&[&f.path], f.size, f.mtime_ns);
        }
        assert_eq!(sample().digest(), d.finish());
        let mut changed = sample();
        changed.dirs[1].files[0].mtime_ns += 1;
        assert_ne!(changed.digest(), sample().digest());
    }

    /// Another format (an older JSON snapshot) is replaced, not an error; bytes in this
    /// format that do not decode are corruption, and must surface.
    #[test]
    fn other_formats_are_ignored_and_damage_is_an_error() {
        assert!(decode(br#"{"version":3,"dirs":[]}"#).unwrap().is_none());
        let bytes = encode(&sample()).unwrap();
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
    }
}
