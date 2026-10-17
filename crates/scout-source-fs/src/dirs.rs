//! One directory listing API for every platform: names, kinds, sizes, mtimes and (on
//! Windows) the file ids the change journal refers to.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    /// Symlinks and junctions: the walker never follows them, so they are never listed.
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub name: String,
    /// The OS name was not valid Unicode and `name` is its lossy form. Such a path can
    /// never be accepted (paths are UTF-8), but its name still has to be remembered.
    pub lossy: bool,
    pub kind: Kind,
    /// Change-journal file id; 0 where the platform has no journal support.
    pub id: u128,
    /// Size and mtime are only meaningful for files.
    pub size: u64,
    pub mtime_ns: u64,
}

pub(crate) use crate::os::{file_id as dir_id, read_dir};

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay compares listed (size, mtime) against what the indexer recorded via
    /// std metadata; any difference would make every file look changed.
    #[test]
    fn sizes_and_mtimes_match_std_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let entries = read_dir(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        let file = entries.iter().find(|e| e.name == "a.txt").unwrap();
        let meta = std::fs::metadata(dir.path().join("a.txt")).unwrap();
        assert_eq!(file.kind, Kind::File);
        assert_eq!(file.size, meta.len());
        assert_eq!(file.mtime_ns, crate::mtime_ns(&meta));
        assert_eq!(entries.iter().find(|e| e.name == "sub").unwrap().kind, Kind::Dir);
    }

    #[cfg(windows)]
    #[test]
    fn listing_ids_match_handle_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let listed = read_dir(dir.path()).unwrap().into_iter().find(|e| e.name == "sub").unwrap();
        assert_ne!(listed.id, 0);
        assert_eq!(listed.id, dir_id(&dir.path().join("sub")).unwrap());
    }
}
