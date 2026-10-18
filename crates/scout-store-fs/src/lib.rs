//! Local-directory [`ShardStore`].
//!
//! ```text
//! <dir>/catalog.json
//! <dir>/write.lock         (held by the one process allowed to write the catalog)
//! <dir>/shards/<name>
//! <dir>/staging/<name>     (written by the indexer, renamed into shards/ on publish)
//! ```
//! Staging lives on the same volume as `shards/`, so publishing is an atomic rename and
//! a concurrent search never sees a half-written shard.

use std::path::{Path, PathBuf};

use scout_core::catalog::Catalog;
use scout_core::ports::{ShardStore, WriterLock};
use scout_core::{Error, Result};

pub struct FsStore {
    dir: PathBuf,
}

impl FsStore {
    pub fn open(dir: &Path) -> Result<Self> {
        for sub in ["shards", "staging"] {
            let p = dir.join(sub);
            std::fs::create_dir_all(&p).map_err(|e| Error::io(p, e))?;
        }
        Ok(Self { dir: dir.to_path_buf() })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn checked(name: &str) -> Result<&str> {
        let ok = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !name.starts_with('.');
        if ok { Ok(name) } else { Err(Error::Invalid(format!("invalid shard name: {name:?}"))) }
    }
}

impl ShardStore for FsStore {
    fn load_catalog(&self) -> Result<Catalog> {
        let path = self.dir.join("catalog.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| Error::backend(format!("parsing {}", path.display()), e))
            }
            // No catalog yet is the normal state of a fresh index directory.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Catalog::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    fn save_catalog(&self, catalog: &Catalog) -> Result<()> {
        let path = self.dir.join("catalog.json");
        // Per process, so a writer that crashed mid-write never collides with the next.
        let tmp = self.dir.join(format!("catalog.json.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(catalog).map_err(|e| Error::backend("serializing catalog", e))?;
        std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(path, e))
    }

    fn staging_path(&self, shard: &str) -> Result<PathBuf> {
        Ok(self.dir.join("staging").join(Self::checked(shard)?))
    }

    fn publish(&self, staged: &Path, shard: &str) -> Result<()> {
        let dest = self.dir.join("shards").join(Self::checked(shard)?);
        // rename replaces silently; a name collision means two runs shared a generation.
        // Callers hold the writer lock, so nothing can create `dest` between check and rename.
        if dest.exists() {
            return Err(Error::Invalid(format!("shard {shard:?} is already published")));
        }
        std::fs::rename(staged, &dest).map_err(|e| Error::io(dest, e))
    }

    fn replace(&self, staged: &Path, name: &str) -> Result<()> {
        let dest = self.dir.join("shards").join(Self::checked(name)?);
        std::fs::rename(staged, &dest).map_err(|e| Error::io(dest, e))
    }

    fn local_path(&self, shard: &str) -> Result<PathBuf> {
        Ok(self.dir.join("shards").join(Self::checked(shard)?))
    }

    fn remove(&self, shard: &str) -> Result<()> {
        let p = self.dir.join("shards").join(Self::checked(shard)?);
        match std::fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(p, e)),
        }
    }

    fn list(&self) -> Result<Vec<String>> {
        let dir = self.dir.join("shards");
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))? {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| Error::Invalid(format!("non-UTF-8 store entry {}", n.display())))?;
            names.push(name);
        }
        Ok(names)
    }

    fn lock_writer(&self) -> Result<WriterLock> {
        let (path, file) = self.lock_file()?;
        // An OS lock, not a lock file: it is released when the process dies, so a crash
        // never leaves the store locked.
        file.lock().map_err(|e| Error::io(&path, e))?;
        Ok(Box::new(file))
    }

    fn try_lock_writer(&self) -> Result<Option<WriterLock>> {
        let (path, file) = self.lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Box::new(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(Error::io(&path, e)),
        }
    }
}

impl FsStore {
    fn lock_file(&self) -> Result<(PathBuf, std::fs::File)> {
        let path = self.dir.join("write.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        Ok((path, file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::catalog::RepoEntry;

    #[test]
    fn catalog_round_trip_and_publish() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path()).unwrap();
        assert!(store.load_catalog().unwrap().repos.is_empty());
        let mut cat = Catalog::default();
        cat.repos.push(RepoEntry { name: "r".into(), root: "/x".into(), remote: None, index: None });
        store.save_catalog(&cat).unwrap();
        assert_eq!(store.load_catalog().unwrap().repos[0].name, "r");

        let staged = store.staging_path("r.0.native").unwrap();
        std::fs::write(&staged, b"x").unwrap();
        store.publish(&staged, "r.0.native").unwrap();
        assert!(store.local_path("r.0.native").unwrap().exists());
        assert_eq!(store.list().unwrap(), ["r.0.native"]);
        std::fs::write(&staged, b"y").unwrap();
        assert!(store.publish(&staged, "r.0.native").is_err(), "a published name is never replaced");
        // Exclusion between two handles, which is what two processes hold.
        let held = store.lock_writer().unwrap();
        assert!(store.try_lock_writer().unwrap().is_none(), "a held lock excludes other writers");
        drop(held);
        let again = store.try_lock_writer().unwrap().expect("free once dropped");
        drop(again);
        drop(store.lock_writer().unwrap());
        assert!(store.staging_path("../evil").is_err());
    }
}
