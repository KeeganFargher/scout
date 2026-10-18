//! The catalog: which repos are registered, which shards hold their index, and which
//! replaced shards are waiting to be deleted.

use serde::{Deserialize, Serialize};

/// How long a replaced shard outlives the catalog that stopped naming it. A search reads
/// the catalog and opens its shards a few milliseconds later; deleting a shard inside that
/// window would fail the search. 60 s is far beyond any such gap and still reclaims disk
/// within a sync interval.
pub const RETIRE_GRACE_SECS: u64 = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub repos: Vec<RepoEntry>,
    /// Shards no repo references any more, deleted once [`RETIRE_GRACE_SECS`] old.
    pub retired: Vec<RetiredShard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    /// Unique, filesystem-safe name; also the shard-name prefix.
    pub name: String,
    /// Source locator (for the FS source: the absolute repo root).
    pub root: String,
    /// The remote a sync cycle mirrors into `root`; `None` for repos added by path, which
    /// sync never touches.
    pub remote: Option<Remote>,
    /// `None` until the repo has been indexed at least once.
    pub index: Option<RepoIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub url: String,
    /// `None` follows the remote's default branch (its `HEAD`).
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub backend: String,
    pub shards: Vec<String>,
    /// [`crate::ports::ListingDigest`] of the listing that was indexed, when the index
    /// holds exactly that listing (no file failed to read). Lets strict freshness skip
    /// repos whose source proves nothing changed.
    pub listing_digest: Option<u128>,
    /// The commit the working copy was at when indexed. Only synced repos have one: their
    /// working copy changes only through sync, so "same commit" means "same files".
    pub commit: Option<String>,
    pub files: u64,
    pub content_bytes: u64,
    pub binary_files: u64,
    pub indexed_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredShard {
    pub shard: String,
    pub at_unix: u64,
}

impl Catalog {
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&RepoEntry> {
        self.repos.iter().find(|r| r.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut RepoEntry> {
        self.repos.iter_mut().find(|r| r.name == name)
    }

    /// Installs `index` for repo `name` and retires the shards it replaces.
    pub fn set_index(&mut self, name: &str, index: RepoIndex, now_unix: u64) -> crate::Result<()> {
        let entry = self.find_mut(name).ok_or_else(|| crate::Error::Invalid(format!("no repo named {name:?}")))?;
        let old = entry.index.replace(index);
        self.retire(old, now_unix);
        Ok(())
    }

    /// Unregisters repo `name` and retires its shards.
    pub fn remove_repo(&mut self, name: &str, now_unix: u64) -> crate::Result<RepoEntry> {
        let pos = self
            .repos
            .iter()
            .position(|r| r.name == name)
            .ok_or_else(|| crate::Error::Invalid(format!("no repo named {name:?}")))?;
        let mut entry = self.repos.remove(pos);
        self.retire(entry.index.take(), now_unix);
        Ok(entry)
    }

    fn retire(&mut self, index: Option<RepoIndex>, now_unix: u64) {
        let shards = index.into_iter().flat_map(|i| i.shards);
        self.retired.extend(shards.map(|shard| RetiredShard { shard, at_unix: now_unix }));
    }

    /// Every shard a repo's current index uses.
    pub fn live_shards(&self) -> impl Iterator<Item = &str> {
        self.repos.iter().flat_map(|r| r.index.iter().flat_map(|i| i.shards.iter().map(String::as_str)))
    }

    /// Removes and returns the retired shards past their grace period.
    pub fn take_expired(&mut self, now_unix: u64) -> Vec<String> {
        let (expired, kept) =
            std::mem::take(&mut self.retired).into_iter().partition(|r| r.at_unix + RETIRE_GRACE_SECS <= now_unix);
        self.retired = kept;
        expired.into_iter().map(|r: RetiredShard| r.shard).collect()
    }
}

/// A repo name usable as a catalog key and shard-name prefix: lowercase ASCII
/// alphanumerics, `-` and `_`, never empty.
#[must_use]
pub fn sanitize_name(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let out = out.trim_matches('-');
    if out.is_empty() { "repo".to_string() } else { out.to_string() }
}

/// Seconds since the epoch. A clock before 1970 is an error, not 0: a zero time would
/// expire retired shards inside their grace period, under readers still opening them.
pub fn now_unix() -> crate::Result<u64> {
    since_epoch().map(|d| d.as_secs())
}

pub(crate) fn since_epoch() -> crate::Result<std::time::Duration> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| crate::Error::Invalid(format!("system clock is before 1970: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(shards: &[&str]) -> RepoIndex {
        RepoIndex {
            backend: "native".into(),
            shards: shards.iter().map(|s| (*s).to_string()).collect(),
            listing_digest: None,
            commit: None,
            files: 0,
            content_bytes: 0,
            binary_files: 0,
            indexed_at_unix: 0,
        }
    }

    #[test]
    fn replaced_and_removed_shards_expire_after_the_grace_period() {
        let mut c = Catalog::default();
        c.repos.push(RepoEntry { name: "a".into(), root: "/a".into(), remote: None, index: None });
        c.set_index("a", index(&["a.g1.0.native"]), 100).unwrap();
        assert!(c.retired.is_empty());
        c.set_index("a", index(&["a.g2.0.native"]), 200).unwrap();
        assert_eq!(c.live_shards().collect::<Vec<_>>(), ["a.g2.0.native"]);
        assert!(c.take_expired(200 + RETIRE_GRACE_SECS - 1).is_empty());
        c.remove_repo("a", 300).unwrap();
        assert_eq!(c.take_expired(200 + RETIRE_GRACE_SECS), ["a.g1.0.native"]);
        assert_eq!(c.take_expired(300 + RETIRE_GRACE_SECS), ["a.g2.0.native"]);
        assert!(c.retired.is_empty() && c.repos.is_empty());
        assert!(c.set_index("missing", index(&[]), 0).is_err());
    }
}
