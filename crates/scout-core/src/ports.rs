//! Ports: the traits adapters implement to make scout's storage swappable.
//!
//! Three independent axes, so any combination works:
//! - [`RepoSource`]: where repo *contents* come from (local FS today, git/S3 later).
//! - [`IndexBackend`] / [`Shard`]: how an index shard is encoded and queried.
//! - [`ShardStore`]: where shard files and the catalog live (local dir today, an object
//!   store with a local cache later).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::catalog::Catalog;
use crate::trigram::{ALWAYS_SEARCH, READER_MODE, Trigram};

/// Metadata of one file in a repo. `path` is relative to the repo root and always uses
/// `/` separators, so shards are portable across operating systems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub path: String,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch. Freshness checks compare
    /// it together with `size`, the same signal `git status` uses.
    pub mtime_ns: u64,
}

/// Metadata of an indexed document, borrowed from the shard where the backend can.
#[derive(Debug, Clone)]
pub struct DocMeta<'a> {
    pub path: Cow<'a, str>,
    pub size: u64,
    pub mtime_ns: u64,
}

/// Result of listing a repo.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub files: Vec<FileMeta>,
    /// Opaque, source-specific state that lets a later `list` answer faster (e.g. the FS
    /// source's directory snapshot). Core stores it next to the shards and hands it back
    /// unchanged; it never interprets it.
    pub snapshot: Option<Vec<u8>>,
    /// The source had to examine everything (no usable snapshot, or no change feed that
    /// could narrow it down). Reported in search stats: it is the slow path.
    pub full_scan: bool,
}

/// Order-independent digest of a listing: two listings with the same (path, size, mtime)
/// entries have the same digest whatever order they are produced in.
///
/// Lets a source prove "the listing is exactly the one that was indexed" from a few bytes,
/// without producing the listing. 128 bits, because a collision would silently hide a
/// change (and results must never be stale).
#[derive(Debug, Default)]
pub struct ListingDigest {
    sum: u128,
    buf: Vec<u8>,
}

impl ListingDigest {
    /// Adds one file; its path is the concatenation of `path_parts` (so a source can hash
    /// `dir`, `"/"`, `name` without building the path).
    pub fn add(&mut self, path_parts: &[&str], size: u64, mtime_ns: u64) {
        self.buf.clear();
        for p in path_parts {
            self.buf.extend_from_slice(p.as_bytes());
        }
        // NUL cannot occur in a path, so the path/metadata boundary is unambiguous.
        self.buf.push(0);
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.buf.extend_from_slice(&mtime_ns.to_le_bytes());
        // Summing per-entry hashes is what makes the digest order-independent; paths are
        // unique within a listing, so no entry can cancel another.
        self.sum = self.sum.wrapping_add(xxhash_rust::xxh3::xxh3_128(&self.buf));
    }

    #[must_use]
    pub fn finish(&self) -> u128 {
        self.sum
    }
}

/// Where a repo's files come from.
pub trait RepoSource: Send + Sync {
    /// Prefix used when printing paths (e.g. the absolute repo root).
    fn display_root(&self) -> &str;

    /// Lists the files a plain `rg` run over this repo would search, honouring the same
    /// ignore rules (`.gitignore`, `.ignore`, hidden files, ...).
    ///
    /// `snapshot` is what an earlier listing returned, if any. Using it is an
    /// optimisation only: the result must be exactly what a listing without it returns.
    /// A returned `Listing::snapshot` replaces the stored one; sources return one only
    /// when it is worth the write.
    fn list(&self, snapshot: Option<&[u8]>) -> Result<Listing>;

    /// Whether listing now would return exactly the files whose [`ListingDigest`] is
    /// `indexed`, decided from `snapshot` without producing the listing. `false` means
    /// "unknown": the caller lists and compares. A source must answer `true` only when
    /// it can prove it, and `false` whenever the snapshot itself should be replaced (so
    /// that `list` gets to return the replacement).
    fn unchanged(&self, snapshot: &[u8], indexed: u128) -> Result<bool>;

    fn read(&self, path: &str) -> Result<Vec<u8>>;
}

/// Opaque document number within one shard.
pub type DocId = u32;

/// What postings point at: a whole document, or one line-aligned block of a large one.
///
/// Indexing blocks instead of whole documents is what keeps big files (CSV dumps, bundled
/// JS) from being candidates for every query: a line-oriented match lies on one line, so
/// all its trigrams fall in a single block, and a doc whose trigrams are merely spread
/// over different blocks is filtered out. A doc's units are contiguous and ascending,
/// so mapping a sorted unit list to docs keeps it sorted.
pub type UnitId = u32;

/// What a search reads of a doc: all of it, or only the units that can match (E32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extent<'a> {
    Whole,
    /// Sorted, not empty, all the doc's own.
    Units(&'a [UnitId]),
}

/// A doc a search is about to read, and how much of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want<'a> {
    pub doc: DocId,
    pub extent: Extent<'a>,
}

/// Visits line-aligned content in file order. `false` stops before the next block is
/// decoded; the line number is relative to the whole file, including skipped units.
pub type BlockVisitor<'a> = dyn FnMut(&[u8], u64) -> Result<bool> + 'a;

/// Encodes and decodes shards. Trigram extraction happens in core, so every backend
/// indexes exactly the same keys and differs only in storage.
pub trait IndexBackend: Send + Sync {
    /// Short stable identifier recorded in the catalog (e.g. `native`, `sqlite`).
    fn name(&self) -> &'static str;

    fn create(&self, dest: &Path) -> Result<Box<dyn ShardWriter>>;

    fn open(&self, path: &Path) -> Result<Box<dyn Shard>>;
}

pub trait ShardWriter {
    /// Adds one document as one or more units. Each entry of `units` holds that unit's
    /// distinct trigrams, sorted ascending; `units` must not be empty.
    fn add(&mut self, meta: &FileMeta, content: &[u8], units: &[Vec<Trigram>]) -> Result<DocId>;

    /// Bytes of content added so far; the indexer uses it to cap shard size.
    fn content_bytes(&self) -> u64;

    fn finish(self: Box<Self>) -> Result<()>;
}

/// Read side of one shard.
pub trait Shard: Send + Sync {
    fn doc_count(&self) -> u32;

    fn meta(&self, doc: DocId) -> Result<DocMeta<'_>>;

    /// Conservative character-shape summary, read without decoding document content.
    fn shape_mask(&self, doc: DocId) -> Result<crate::shape::ShapeMask>;

    /// Sorted ids of units containing `tri`; empty when the trigram is absent.
    fn postings(&self, tri: Trigram) -> Result<Vec<UnitId>>;

    /// How many units contain `tri`, without decoding them: the planner reads it to decode
    /// only the rarest lists of an AND (E22).
    fn count(&self, tri: Trigram) -> Result<u32>;

    /// Whether the shard may hold `tri`, answered from a filter without reading the
    /// shard's dictionary or postings (E24; a filter read through a map costs a page of its
    /// own). `false` is certain, so the planner asks it for every trigram of a query before
    /// counting or decoding any; `true` can be wrong (a false positive), but is never
    /// `false` for a trigram the shard holds.
    fn may_contain(&self, tri: Trigram) -> bool;

    /// The marker doc lists, decoded once when the shard opens (E23): planning reads both
    /// for every shard on every query, and once the page cache has dropped them each one
    /// costs a random read at the end of the dictionary and of the postings, per shard.
    fn markers(&self) -> &Markers;

    /// The document a unit belongs to.
    fn unit_doc(&self, unit: UnitId) -> Result<DocId>;

    /// `doc`'s content: a slice of the shard where it is stored as is, else decoded into
    /// `buf` (E29). One buffer per verifying thread serves every doc it reads, so a
    /// compressed index costs a decode per read, not an allocation too.
    fn content<'a>(&'a self, doc: DocId, buf: &'a mut Vec<u8>) -> Result<&'a [u8]>;

    /// Visits `want`'s text a block at a time, borrowing raw storage or reusing `buf` to
    /// decode it. A visitor can stop decoding after its first match or a file-wide limit.
    /// A backend's storage granularity may require reading the whole doc as one block.
    fn visit_content(&self, want: Want<'_>, buf: &mut Vec<u8>, visit: &mut BlockVisitor<'_>) -> Result<()>;

    /// Starts reading what `wants` will read from disk in the background, ahead of the
    /// [`Shard::content`] and [`Shard::visit_content`] calls that will need it (E28).
    /// Without it a search whose index is not in the page cache waits on one read at a time
    /// per verifying thread, each a page fault; on a network volume (EBS) every one of those
    /// pays the volume's latency. A hint: it reads nothing a later read would not, and a
    /// backend with nothing to read ahead does nothing.
    ///
    /// `query` is [`Shard::read_cost`] summed over every shard the search reads: a disk's
    /// caps bind the whole query, so how a backend reads one block depends on whether the
    /// query's reads or its bytes run out first (E52).
    fn prefetch(&self, wants: &[Want<'_>], query: ReadCost) -> Result<()>;

    /// What [`Shard::prefetch`] of `wants` would read from a cold disk, before any of it
    /// is read: a search sums it over every shard while it plans (E52). A backend with
    /// nothing to read ahead returns the default.
    fn read_cost(&self, wants: &[Want<'_>]) -> Result<ReadCost>;
}

/// Reads from disk and their bytes, as a backend would issue them (E52).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadCost {
    pub reads: u64,
    pub bytes: u64,
}

impl std::ops::AddAssign for ReadCost {
    fn add_assign(&mut self, other: Self) {
        self.reads += other.reads;
        self.bytes += other.bytes;
    }
}

/// Docs carrying the marker pseudo-trigrams [`ALWAYS_SEARCH`] and [`READER_MODE`].
///
/// `Default` (both empty) exists only so a backend can build its shard first and then
/// [`Markers::load`] through it; an opened shard always holds the loaded lists.
#[derive(Debug, Default, Clone)]
pub struct Markers {
    /// Candidates for every query, whatever its trigrams.
    pub always_search: Vec<DocId>,
    /// Searched through rg's reader path instead of as a raw slice.
    pub reader_mode: Vec<DocId>,
}

impl Markers {
    /// Decodes both lists through the shard's own postings and unit table.
    pub fn load(shard: &dyn Shard) -> Result<Self> {
        Ok(Self { always_search: marker_docs(shard, ALWAYS_SEARCH)?, reader_mode: marker_docs(shard, READER_MODE)? })
    }
}

fn marker_docs(shard: &dyn Shard, marker: Trigram) -> Result<Vec<DocId>> {
    let mut docs: Vec<DocId> = Vec::new();
    for unit in shard.postings(marker)? {
        let doc = shard.unit_doc(unit)?;
        // A doc's units are contiguous and ascending, so its repeats are adjacent.
        if docs.last() != Some(&doc) {
            docs.push(doc);
        }
    }
    Ok(docs)
}

/// Where shard files and the catalog live.
pub trait ShardStore: Send + Sync {
    fn load_catalog(&self) -> Result<Catalog>;

    fn save_catalog(&self, catalog: &Catalog) -> Result<()>;

    /// A local path a new shard can be written to before it is published.
    fn staging_path(&self, shard: &str) -> Result<PathBuf>;

    /// Makes a staged shard visible under `shard`. Shard names are unique per index run
    /// (see [`crate::indexer::IndexOptions::generation`]), so an existing shard of that
    /// name is an error, never something to replace: a live reader may be using it.
    fn publish(&self, staged: &Path, shard: &str) -> Result<()>;

    /// Atomically replaces the store file `name` (not a shard) with `staged`. For files
    /// that are rewritten in place, like [`crate::snapshots::PACK_NAME`], whose readers
    /// accept either the old or the new version.
    fn replace(&self, staged: &Path, name: &str) -> Result<()>;

    /// A local path the backend can open (a remote store would download/cache here).
    fn local_path(&self, shard: &str) -> Result<PathBuf>;

    fn remove(&self, shard: &str) -> Result<()>;

    /// Every published object's name: shards, plus store files kept beside them such as
    /// [`crate::snapshots::PACK_NAME`]. Used to find shards a crashed run published but
    /// never recorded in a catalog.
    fn list(&self) -> Result<Vec<String>>;

    /// Blocks until this process is the only writer of the catalog, until the guard drops.
    /// Catalog updates are read-modify-write; without this two writers lose each other's
    /// changes. Readers never take it.
    fn lock_writer(&self) -> Result<WriterLock>;

    /// [`Self::lock_writer`] without waiting: `None` while another writer holds it. For
    /// writes that are worth doing only when they cost the caller nothing.
    fn try_lock_writer(&self) -> Result<Option<WriterLock>>;
}

/// Held while writing; releases the store's writer lock when dropped.
pub type WriterLock = Box<dyn std::any::Any + Send>;
