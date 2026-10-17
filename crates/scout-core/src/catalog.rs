//! The catalog: which repos are registered, and which shards hold their index.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    /// Unique, filesystem-safe name; also the shard-name prefix.
    pub name: String,
    /// Source locator (for the FS source: the absolute repo root).
    pub root: String,
    /// `None` until the repo has been indexed at least once.
    pub index: Option<RepoIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub backend: String,
    pub shards: Vec<String>,
    /// [`crate::ports::ListingDigest`] of the listing that was indexed, when the index
    /// holds exactly that listing (no file failed to read). Lets strict freshness skip
    /// repos whose source proves nothing changed.
    pub listing_digest: Option<u128>,
    pub files: u64,
    pub content_bytes: u64,
    pub binary_files: u64,
    pub indexed_at_unix: u64,
}

impl Catalog {
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&RepoEntry> {
        self.repos.iter().find(|r| r.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut RepoEntry> {
        self.repos.iter_mut().find(|r| r.name == name)
    }
}
