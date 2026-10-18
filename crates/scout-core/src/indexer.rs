//! Builds the shards for one repo.

use std::path::PathBuf;

use crate::catalog::RepoIndex;
use crate::ports::{IndexBackend, ListingDigest, RepoSource, ShardStore, ShardWriter};
use crate::trigram::{ALWAYS_SEARCH, READER_MODE, Trigram, TrigramExtractor};
use crate::{Error, Result};

/// Target size of a posting unit (see [`crate::ports::UnitId`]).
///
/// Measured on the bench corpus: 64 KiB grows posting entries 1.5x and cuts the bytes
/// verified for false-positive docs by up to ~70% (`useLayoutEffect`: 268 → 86 MiB).
/// 16 KiB filters a little better but doubles the postings; 256 KiB barely filters.
pub const UNIT_TARGET: usize = 64 << 10;

/// Splits `content` into units of about `target` bytes, each ending just after a newline
/// (or at the end). A line never spans two units, which is what makes per-unit trigram
/// filtering sound for line-oriented matching. A line longer than `target` makes one larger
/// unit.
#[must_use]
pub fn unit_spans(content: &[u8], target: usize) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::with_capacity(content.len() / target.max(1) + 1);
    let mut start = 0;
    while start < content.len() {
        let want = start + target;
        let end = if want >= content.len() {
            content.len()
        } else {
            memchr::memchr(b'\n', &content[want - 1..]).map_or(content.len(), |i| want + i)
        };
        spans.push(start..end);
        start = end;
    }
    if spans.is_empty() {
        spans.push(0..0);
    }
    spans
}

/// Distinct trigrams per unit of `content`, with `markers` added to the first unit.
fn units(extractor: &mut TrigramExtractor, content: &[u8], markers: &[Trigram]) -> Vec<Vec<Trigram>> {
    let mut out: Vec<Vec<Trigram>> =
        unit_spans(content, UNIT_TARGET).into_iter().map(|r| extractor.extract(&content[r]).to_vec()).collect();
    // Markers are outside the 24-bit trigram space, so appending keeps the list sorted.
    out[0].extend_from_slice(markers);
    out
}

#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    /// Content bytes per shard before a new shard is started. Building a shard holds its
    /// postings in memory, so this is what bounds indexing RAM on small machines.
    pub shard_content_cap: u64,
    /// Part of every shard name, so a reindex never reuses the name of a shard a reader
    /// may still be about to open. One per index run; see [`new_generation`].
    pub generation: u64,
}

/// A generation id no earlier run used: the clock in nanoseconds. Writers hold the store's
/// writer lock, so two runs never start in the same nanosecond on one store.
pub fn new_generation() -> Result<u64> {
    crate::catalog::since_epoch().map(|d| d.as_nanos() as u64)
}

/// A file that could not be indexed. Reported to the caller rather than aborting the
/// repo, because a file vanishing or being locked mid-walk is normal on a live checkout.
#[derive(Debug)]
pub struct SkippedFile {
    pub path: String,
    pub error: Error,
}

#[derive(Debug)]
pub struct IndexReport {
    pub index: RepoIndex,
    pub skipped: Vec<SkippedFile>,
    /// The source's snapshot of the listing that was indexed, for the caller to store in
    /// the [`crate::snapshots::SnapshotPack`]. Written by the caller, once for all repos,
    /// rather than here per repo, because the pack is one file.
    pub snapshot: Option<Vec<u8>>,
}

/// How rg reads a file found while walking a directory (BOM sniffing on, binary
/// detection = quit at the first NUL, searching everything before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Plain bytes: trigram-indexed and searched as a slice.
    Text,
    /// UTF-8 BOM: indexable as-is, but rg's reader strips the BOM, which `^` can observe.
    Utf8Bom,
    /// UTF-16 BOM: rg transcodes it to UTF-8 before matching, so raw-byte trigrams say
    /// nothing about it.
    Utf16,
    /// Contains a NUL at this offset. rg reads the file in fills of its line buffer and quits
    /// at the first fill holding the NUL, after printing the matches of earlier fills and a
    /// warning. scout reproduces rg with a fresh (64 KiB) buffer; rg's own output depends on
    /// how far earlier files grew its reused buffer (E14). Nothing after the NUL can ever
    /// affect output.
    Binary { nul: usize },
}

#[must_use]
pub fn classify(content: &[u8]) -> Kind {
    if content.starts_with(&[0xFF, 0xFE]) || content.starts_with(&[0xFE, 0xFF]) {
        return Kind::Utf16;
    }
    if let Some(nul) = memchr::memchr(0, content) {
        return Kind::Binary { nul };
    }
    if content.starts_with(&[0xEF, 0xBB, 0xBF]) { Kind::Utf8Bom } else { Kind::Text }
}

struct Building<'a> {
    backend: &'a dyn IndexBackend,
    store: &'a dyn ShardStore,
    repo: &'a str,
    generation: u64,
    staged: Vec<(String, PathBuf)>,
    current: Option<Box<dyn ShardWriter>>,
}

impl Building<'_> {
    fn add(&mut self, meta: &crate::ports::FileMeta, content: &[u8], units: &[Vec<Trigram>]) -> Result<u64> {
        if self.current.is_none() {
            let name = format!("{}.{:x}.{}.{}", self.repo, self.generation, self.staged.len(), self.backend.name());
            let path = self.store.staging_path(&name)?;
            self.current = Some(self.backend.create(&path)?);
            self.staged.push((name, path));
        }
        let w = self.current.as_mut().ok_or_else(|| Error::Invalid("shard writer missing".into()))?;
        w.add(meta, content, units)?;
        Ok(w.content_bytes())
    }

    fn seal(&mut self) -> Result<()> {
        if let Some(w) = self.current.take() {
            w.finish()?;
        }
        Ok(())
    }
}

/// Indexes every file `source` lists into one or more shards and publishes them.
///
/// Shards are only published after all of them were written successfully, so a failed
/// run never leaves a half-updated index behind.
pub fn index_repo(
    repo: &str,
    source: &dyn RepoSource,
    backend: &dyn IndexBackend,
    store: &dyn ShardStore,
    opts: IndexOptions,
) -> Result<IndexReport> {
    if opts.shard_content_cap == 0 {
        return Err(Error::Invalid("shard_content_cap must be > 0".into()));
    }
    let listing = source.list(None)?;
    let snapshot = listing.snapshot;
    let mut files = listing.files;
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));

    let mut extractor = TrigramExtractor::default();
    let mut b = Building { backend, store, repo, generation: opts.generation, staged: Vec::new(), current: None };
    let mut skipped = Vec::new();
    let (mut indexed, mut bytes, mut binary) = (0u64, 0u64, 0u64);

    for meta in &files {
        let content = match source.read(&meta.path) {
            Ok(c) => c,
            Err(error) => {
                skipped.push(SkippedFile { path: meta.path.clone(), error });
                continue;
            }
        };
        let (stored, shard_bytes) = match classify(&content) {
            Kind::Text => (content.len(), b.add(meta, &content, &units(&mut extractor, &content, &[]))?),
            Kind::Utf8Bom => (content.len(), b.add(meta, &content, &units(&mut extractor, &content, &[READER_MODE]))?),
            Kind::Utf16 => (content.len(), b.add(meta, &content, &[vec![READER_MODE, ALWAYS_SEARCH]])?),
            Kind::Binary { nul } => {
                // Keep the bytes up to and including the NUL: all that can affect rg's output,
                // and the NUL offset reproduces rg's warning text. Images, fonts and
                // archives usually have a NUL within a few bytes, so this costs almost
                // nothing to store. The bytes before the NUL are matched raw (no BOM), so
                // their trigrams filter these docs like any text file.
                binary += 1;
                let prefix = &content[..=nul];
                (prefix.len(), b.add(meta, prefix, &units(&mut extractor, &content[..nul], &[READER_MODE]))?)
            }
        };
        indexed += 1;
        bytes += stored as u64;
        if shard_bytes >= opts.shard_content_cap {
            b.seal()?;
        }
    }
    b.seal()?;

    let mut shards = Vec::with_capacity(b.staged.len());
    for (name, path) in &b.staged {
        store.publish(path, name)?;
        shards.push(name.clone());
    }
    // A skipped file is in the listing but not in the index, so the index does not hold
    // exactly this listing and no digest may claim it does.
    let listing_digest = skipped.is_empty().then(|| {
        let mut d = ListingDigest::default();
        for f in &files {
            d.add(&[&f.path], f.size, f.mtime_ns);
        }
        d.finish()
    });
    let indexed_at_unix = crate::catalog::now_unix()?;
    Ok(IndexReport {
        index: RepoIndex {
            backend: backend.name().to_string(),
            shards,
            listing_digest,
            // Only the caller knows whether the source is a synced working copy.
            commit: None,
            files: indexed,
            content_bytes: bytes,
            binary_files: binary,
            indexed_at_unix,
        },
        skipped,
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_spans_end_after_newlines_and_cover_everything() {
        let text = b"aa\nbbbb\ncc\nd";
        let spans = unit_spans(text, 2);
        // The second unit reaches its target inside "bbbb", so it runs to that line's end.
        assert_eq!(spans, vec![0..3, 3..8, 8..11, 11..12]);
        for w in spans.windows(2) {
            assert_eq!(text[w[0].end - 1], b'\n');
            assert_eq!(w[0].end, w[1].start);
        }
        assert_eq!(unit_spans(b"", 4), vec![0..0]);
        assert_eq!(unit_spans(b"no newline at all", 4), vec![0..17]);
        assert_eq!(unit_spans(b"abc\n", 4), vec![0..4]);
    }
}
