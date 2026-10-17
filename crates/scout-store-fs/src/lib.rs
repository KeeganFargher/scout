//! Local-directory [`ShardStore`].
//!
//! ```text
//! <dir>/catalog.json
//! <dir>/shards/<name>
//! <dir>/staging/<name>     (written by the indexer, renamed into shards/ on publish)
//! ```
//! Staging lives on the same volume as `shards/`, so publishing is an atomic rename and
//! a concurrent search never sees a half-written shard.

use std::path::{Path, PathBuf};

use scout_core::catalog::Catalog;
use scout_core::ports::ShardStore;
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
        let tmp = self.dir.join("catalog.json.tmp");
        let bytes = serde_json::to_vec_pretty(catalog).map_err(|e| Error::backend("serializing catalog", e))?;
        std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(path, e))
    }

    fn staging_path(&self, shard: &str) -> Result<PathBuf> {
        Ok(self.dir.join("staging").join(Self::checked(shard)?))
    }

    fn publish(&self, staged: &Path, shard: &str) -> Result<()> {
        let dest = self.dir.join("shards").join(Self::checked(shard)?);
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
        cat.repos.push(RepoEntry { name: "r".into(), root: "/x".into(), index: None });
        store.save_catalog(&cat).unwrap();
        assert_eq!(store.load_catalog().unwrap().repos[0].name, "r");

        let staged = store.staging_path("r.0.native").unwrap();
        std::fs::write(&staged, b"x").unwrap();
        store.publish(&staged, "r.0.native").unwrap();
        assert!(store.local_path("r.0.native").unwrap().exists());
        assert!(store.staging_path("../evil").is_err());
    }
}
