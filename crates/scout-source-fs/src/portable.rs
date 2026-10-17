//! The `os` surface for platforms without a change journal (everything but Windows, so
//! far). Same functions and types as `win.rs`, so no other module needs a `cfg`.
//!
//! [`Journal`] is uninhabited: `open` always reports [`io::ErrorKind::Unsupported`], the
//! context treats that like a FAT volume, and every refresh scans. The type system then
//! proves no journal method is ever reached here.

use std::io;
use std::path::Path;

use crate::dirs::{Entry, Kind};

pub(crate) fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let kind = if ft.is_symlink() {
            Kind::Other
        } else if ft.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        };
        // Only files need a stat; directories are described by their own listing.
        let (size, mtime_ns) = if kind == Kind::File {
            let m = entry.metadata()?;
            (m.len(), crate::mtime_ns(&m))
        } else {
            (0, 0)
        };
        let os = entry.file_name();
        let lossy = os.to_str().is_none();
        out.push(Entry { name: os.to_string_lossy().into_owned(), lossy, kind, id: 0, size, mtime_ns });
    }
    Ok(out)
}

/// Ids only matter to the journal, and there is none here.
#[allow(clippy::unnecessary_wraps, reason = "same signature as win.rs, which can fail")]
pub(crate) fn file_id(_: &Path) -> io::Result<u128> {
    Ok(0)
}

/// Feeds are cached by volume. With no journal every volume's feed is the same absent
/// one, so a single key is exact rather than an approximation.
#[allow(clippy::unnecessary_wraps, reason = "same signature as win.rs, which can fail")]
pub(crate) fn volume_of(_: &Path) -> io::Result<String> {
    Ok(String::new())
}

pub(crate) fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
}

pub(crate) enum Journal {}

impl Journal {
    pub(crate) fn open(_: &str) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no change journal on this platform"))
    }

    pub(crate) fn id(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn next(&self) -> i64 {
        match *self {}
    }

    pub(crate) fn lowest_valid(&self) -> i64 {
        match *self {}
    }

    pub(crate) fn read(&self, _: i64, _: i64, _: impl FnMut(i64, u128, u128, bool)) -> io::Result<()> {
        match *self {}
    }
}
