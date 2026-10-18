//! Native shard format (v5).
//!
//! One file per shard, read through `mmap` so the OS decides what stays in RAM. That is
//! what keeps scout usable on a 2 GB machine with an index far larger than memory.
//!
//! ```text
//! magic    8  b"SCOUTNI5"
//! header  88  u32 version, u32 doc_count, u32 trigram_count, u32 unit_count,
//!             u64 content_off, u64 docs_off, u64 paths_off, u64 units_off, u64 keys_off,
//!             u64 vals_off, u64 fence_off, u64 filter_off, u64 postings_off
//! content     corpus bin: every document's bytes, concatenated
//! docs        doc_count × 40: u64 content_off, u64 size, u64 mtime_ns,
//!             u32 content_len, u32 path_off, u32 path_len, u32 pad
//! paths       UTF-8 relative paths, concatenated
//! units       unit_count × 4: u32 doc of each posting unit (see `UnitId`)
//! keys        trigram_count × 4, sorted: u32 trigram
//! vals        trigram_count × 12, same order: u32 unit_count, u64 postings offset
//! fence       ceil(trigram_count / FENCE) × 4: every FENCE-th key
//! filter      u64 words: a Bloom filter of every key (see `filter_slot`)
//! postings    per trigram: unit ids, delta + LEB128 varint encoded
//! ```
//!
//! The dictionary is split so a lookup touches few pages (E19). With v2's interleaved
//! 16-byte entries, a binary search over a shard's ~44k trigrams made ~16 probes over ~10
//! distinct pages, and dictionary lookups cost 3–4.5 ms of a selective query's ~10 ms at
//! 1 CPU: a fresh CLI process faults in every mmapped page it touches. Now the fence
//! (~1.4 KiB, copied to the heap at open since E23, shared by every lookup) picks a block,
//! the block's keys (512 bytes, one page or straddling two) find the trigram, and one
//! value read finishes it. Sections are not page-aligned, so those are typical counts, not bounds.
//!
//! The filter (E24) answers "does this shard lack the trigram?" without the dictionary:
//! the planner asks it for every trigram of a query first, so a shard that lacks one is
//! skipped without reading its dictionary or postings, which is what a cold query mostly
//! waits on. It sits right after the fence, which every open reads, so a cold open fetches
//! both in one read-around for a typical shard. Where an opened shard keeps it is the
//! backend's [`FilterHome`].
//!
//! Content comes first so the writer can stream it straight to disk; everything else is
//! small enough to buffer and append in `finish`, so every section starts where the one
//! before it ends. All integers are little-endian.
//!
//! v4 was E20's positional-mask format, rejected but built: its shards still exist (bench
//! indexes), so E24's layout took v5, and such a shard is refused as an old format with a
//! "rebuild" message instead of being misparsed as this one.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use scout_core::ports::{DocId, DocMeta, FileMeta, IndexBackend, Markers, Shard, ShardWriter, UnitId};
use scout_core::trigram::Trigram;
use scout_core::{Error, Result};

const MAGIC: &[u8; 8] = b"SCOUTNI5";
const VERSION: u32 = 5;
const HEADER_LEN: u64 = 8 + 88;
const DOC_LEN: usize = 40;
const VAL_LEN: usize = 12;
/// Keys per fence block: 128 × 4 bytes is one short cache-friendly run (usually within one
/// page), and the fence of a 44k-trigram shard is ~350 keys, ~1.4 KiB.
const FENCE: usize = 128;

/// Bits of filter per dictionary key, and how many of its word's bits each key sets (E24).
/// Simulated on the bench index's 6.06M keys over the literal bench queries, 6 and 4 let
/// 619 shard-queries through where an exact set lets 617 (4 bits: 632; 8 bits: 618), for
/// 0.75 bytes a key. The reader takes the word count from the section, so the bits per key
/// can change without a format change; the hash and `FILTER_K` cannot (a test pins them:
/// changed, every existing index would reject trigrams it holds and silently lose matches).
const FILTER_BITS_PER_KEY: usize = 6;
const FILTER_K: u64 = 4;
// A key's bits come from the hash's low 6·K bits and its word from the high 32: overlapping,
// the two choices would correlate and the false-positive rate would rise.
const _: () = assert!(6 * FILTER_K <= 32);

/// Where an opened shard keeps its trigram filter (E24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterHome {
    /// Copied to the heap at open. A long-running server probes every shard's filter on
    /// every query; on the heap the page cache cannot drop it, so after the index's pages
    /// are evicted (a restart, or memory pressure) a query still skips the shards it
    /// rejects without reading them. Costs 0.75 bytes per dictionary key.
    Heap,
    /// Probed in place through the map, so a probe reads a page of the filter (one a cold
    /// open has usually just read with the fence beside it). A process per query (the CLI)
    /// would otherwise copy every shard's filter on each start, about 4 MiB for the bench
    /// index, more than one query's probes cost.
    Map,
}

#[derive(Debug, Clone, Copy)]
pub struct NativeBackend {
    filter: FilterHome,
}

impl NativeBackend {
    /// Writing does not depend on `filter`; only shards this backend opens do.
    #[must_use]
    pub const fn new(filter: FilterHome) -> Self {
        Self { filter }
    }
}

impl IndexBackend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn create(&self, dest: &Path) -> Result<Box<dyn ShardWriter>> {
        Ok(Box::new(NativeWriter::create(dest)?))
    }

    fn open(&self, path: &Path) -> Result<Box<dyn Shard>> {
        Ok(Box::new(NativeShard::open(path, self.filter)?))
    }
}

struct DocRow {
    content_off: u64,
    content_len: u32,
    size: u64,
    mtime_ns: u64,
    path_off: u32,
    path_len: u32,
}

pub struct NativeWriter {
    path: PathBuf,
    out: BufWriter<File>,
    content_len: u64,
    docs: Vec<DocRow>,
    paths: Vec<u8>,
    /// Doc of each unit, indexed by unit id.
    units: Vec<u32>,
    /// `(trigram << 32) | unit`. One flat vector sorts faster and uses far less memory
    /// than a map of per-trigram vectors (no per-key allocation).
    pairs: Vec<u64>,
}

impl NativeWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|e| Error::io(path, e))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        // Header is rewritten in `finish`; reserve its space now.
        out.write_all(&[0u8; HEADER_LEN as usize]).map_err(|e| Error::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            out,
            content_len: 0,
            docs: Vec::new(),
            paths: Vec::new(),
            units: Vec::new(),
            pairs: Vec::new(),
        })
    }

    fn io(&self, e: std::io::Error) -> Error {
        Error::io(&self.path, e)
    }
}

fn to_u32(n: usize, what: &str) -> Result<u32> {
    u32::try_from(n).map_err(|_| Error::Invalid(format!("{what} exceeds u32 range")))
}

impl ShardWriter for NativeWriter {
    fn add(&mut self, meta: &FileMeta, content: &[u8], units: &[Vec<Trigram>]) -> Result<DocId> {
        if units.is_empty() {
            return Err(Error::Invalid(format!("{}: a document needs at least one unit", meta.path)));
        }
        let doc = to_u32(self.docs.len(), "doc count")?;
        self.out.write_all(content).map_err(|e| self.io(e))?;
        self.docs.push(DocRow {
            content_off: self.content_len,
            content_len: to_u32(content.len(), "file size")?,
            size: meta.size,
            mtime_ns: meta.mtime_ns,
            path_off: to_u32(self.paths.len(), "path table")?,
            path_len: to_u32(meta.path.len(), "path length")?,
        });
        self.paths.extend_from_slice(meta.path.as_bytes());
        self.content_len += content.len() as u64;
        for trigrams in units {
            let unit = to_u32(self.units.len(), "unit count")?;
            self.units.push(doc);
            self.pairs.extend(trigrams.iter().map(|t| (u64::from(t.0) << 32) | u64::from(unit)));
        }
        Ok(doc)
    }

    fn content_bytes(&self) -> u64 {
        self.content_len
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        let docs_off = HEADER_LEN + self.content_len;
        let mut docs = Vec::with_capacity(self.docs.len() * DOC_LEN);
        for d in &self.docs {
            docs.extend_from_slice(&d.content_off.to_le_bytes());
            docs.extend_from_slice(&d.size.to_le_bytes());
            docs.extend_from_slice(&d.mtime_ns.to_le_bytes());
            docs.extend_from_slice(&d.content_len.to_le_bytes());
            docs.extend_from_slice(&d.path_off.to_le_bytes());
            docs.extend_from_slice(&d.path_len.to_le_bytes());
            docs.extend_from_slice(&0u32.to_le_bytes());
        }
        let paths_off = docs_off + docs.len() as u64;
        let units_off = paths_off + self.paths.len() as u64;
        let units: Vec<u8> = self.units.iter().flat_map(|d| d.to_le_bytes()).collect();
        let keys_off = units_off + units.len() as u64;

        self.pairs.sort_unstable();
        let (keys, vals, postings) = encode_postings(&self.pairs);
        let fence: Vec<u8> = keys.chunks(FENCE * 4).flat_map(|block| block[..4].to_vec()).collect();
        let filter = build_filter(&keys);
        let vals_off = keys_off + keys.len() as u64;
        let fence_off = vals_off + vals.len() as u64;
        let filter_off = fence_off + fence.len() as u64;
        let postings_off = filter_off + filter.len() as u64;
        let trigram_count = to_u32(keys.len() / 4, "trigram count")?;

        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&to_u32(self.docs.len(), "doc count")?.to_le_bytes());
        header.extend_from_slice(&trigram_count.to_le_bytes());
        header.extend_from_slice(&to_u32(self.units.len(), "unit count")?.to_le_bytes());
        for off in [HEADER_LEN, docs_off, paths_off, units_off, keys_off, vals_off, fence_off, filter_off, postings_off]
        {
            header.extend_from_slice(&off.to_le_bytes());
        }

        let path = self.path.clone();
        let io = |e| Error::io(&path, e);
        self.out.write_all(&docs).map_err(io)?;
        self.out.write_all(&self.paths).map_err(io)?;
        self.out.write_all(&units).map_err(io)?;
        self.out.write_all(&keys).map_err(io)?;
        self.out.write_all(&vals).map_err(io)?;
        self.out.write_all(&fence).map_err(io)?;
        self.out.write_all(&filter).map_err(io)?;
        self.out.write_all(&postings).map_err(io)?;
        let mut file = self.out.into_inner().map_err(|e| Error::io(&path, e.into_error()))?;
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        file.write_all(&header).map_err(io)?;
        file.sync_all().map_err(io)
    }
}

/// Groups sorted `(trigram, unit)` pairs into the dictionary's keys and values and the
/// varint postings blob.
fn encode_postings(pairs: &[u64]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut keys = Vec::new();
    let mut vals = Vec::new();
    let mut postings = Vec::new();
    let mut i = 0;
    while i < pairs.len() {
        let tri = (pairs[i] >> 32) as u32;
        let start = postings.len() as u64;
        let mut prev = 0u32;
        let mut count = 0u32;
        while i < pairs.len() && (pairs[i] >> 32) as u32 == tri {
            let unit = pairs[i] as u32;
            write_varint(&mut postings, unit - prev);
            prev = unit;
            count += 1;
            i += 1;
        }
        keys.extend_from_slice(&tri.to_le_bytes());
        vals.extend_from_slice(&count.to_le_bytes());
        vals.extend_from_slice(&start.to_le_bytes());
    }
    (keys, vals, postings)
}

/// The filter section for the dictionary `keys` (little-endian u32s): at least one word,
/// so a shard without trigrams still has a filter, one that rejects every trigram.
fn build_filter(keys: &[u8]) -> Vec<u8> {
    let n = keys.len() / 4;
    let mut words = vec![0u64; (n * FILTER_BITS_PER_KEY).div_ceil(64).max(1)];
    for key in keys.chunks_exact(4) {
        let (word, bits) = filter_slot(le_u32(key, 0), words.len());
        words[word] |= bits;
    }
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// The word of a `words`-word filter that holds `key`, and the bits of it the key sets.
/// All of a key's bits share one word, so a probe is one memory access (one page when the
/// filter is read through the map) at a slightly higher false-positive rate than spreading
/// them; at 6 bits a key that rate is ~7%, and a shard lacking several of a query's
/// trigrams is let through only if all of them are false positives.
fn filter_slot(key: u32, words: usize) -> (usize, u64) {
    // splitmix64's finaliser: trigram keys are small, dense integers, so they need a full
    // mix before their bits can pick anything.
    let mut z = u64::from(key).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // The high 32 bits pick the word (multiply-shift: uniform without a division), the
    // low 24 pick the bits, so the two choices do not correlate.
    let word = (((z >> 32) * words as u64) >> 32) as usize;
    let bits = (0..FILTER_K).fold(0u64, |bits, i| bits | 1 << ((z >> (6 * i)) & 63));
    (word, bits)
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub struct NativeShard {
    name: String,
    map: memmap2::Mmap,
    doc_count: u32,
    trigram_count: u32,
    unit_count: u32,
    content_off: usize,
    docs_off: usize,
    paths_off: usize,
    units_off: usize,
    keys_off: usize,
    vals_off: usize,
    postings_off: usize,
    /// Every FENCE-th key, copied out of the map at open (E23): every lookup reads it, and
    /// on the heap the page cache cannot drop it (~1.4 KiB per shard).
    fence: Vec<u32>,
    filter: Filter,
    /// Words in the filter, at least one.
    filter_words: usize,
    markers: Markers,
}

/// A shard's trigram filter, where its [`FilterHome`] says.
enum Filter {
    Heap(Vec<u64>),
    /// The section's offset in the map.
    Map(usize),
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(a)
}

fn le_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

impl NativeShard {
    pub fn open(path: &Path, home: FilterHome) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        // SAFETY: shards are immutable once published (the store replaces them by
        // rename, never in place), so the mapping cannot change underneath us. A user
        // editing a shard file by hand is outside the supported contract.
        #[allow(unsafe_code)]
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| Error::io(path, e))?;
        let name = path.display().to_string();
        let corrupt = |reason: &str| Error::Corrupt { shard: name.clone(), reason: reason.to_string() };
        // Another format version is not damage, but it is just as unreadable, so it is
        // reported through the same error with a reason that says what fixes it.
        if map.len() >= 8 && map[..7] == MAGIC[..7] && map[..8] != MAGIC[..] {
            return Err(corrupt(&format!(
                "native format {} is not {}: rebuild the index with `scout index`",
                String::from_utf8_lossy(&map[..8]),
                String::from_utf8_lossy(MAGIC)
            )));
        }
        if map.len() < HEADER_LEN as usize || &map[..8] != MAGIC {
            return Err(corrupt("bad magic or truncated header"));
        }
        if le_u32(&map, 8) != VERSION {
            return Err(corrupt("unsupported version"));
        }
        let off = |i: usize| usize::try_from(le_u64(&map, 24 + i * 8)).map_err(|_| corrupt("offset overflow"));
        let (fence_off, filter_off) = (off(6)?, off(7)?);
        let mut shard = Self {
            doc_count: le_u32(&map, 12),
            trigram_count: le_u32(&map, 16),
            unit_count: le_u32(&map, 20),
            content_off: off(0)?,
            docs_off: off(1)?,
            paths_off: off(2)?,
            units_off: off(3)?,
            keys_off: off(4)?,
            vals_off: off(5)?,
            postings_off: off(8)?,
            fence: Vec::new(),
            filter: Filter::Map(filter_off),
            filter_words: 0,
            markers: Markers::default(),
            name: name.clone(),
            map,
        };
        // The writer appends every section where the one before ends, so the reader demands
        // exactly that of the fixed-size ones (only content, paths and postings vary): a bad
        // offset fails here instead of as a slice out of bounds mid-query. Checked, because
        // release builds wrap: an offset near u64::MAX would wrap past every comparison.
        let end = |start: usize, count: usize, len: usize| {
            count
                .checked_mul(len)
                .and_then(|n| start.checked_add(n))
                .ok_or_else(|| corrupt("section offsets out of bounds"))
        };
        let n = shard.trigram_count as usize;
        let docs_end = end(shard.docs_off, shard.doc_count as usize, DOC_LEN)?;
        let units_end = end(shard.units_off, shard.unit_count as usize, 4)?;
        let keys_end = end(shard.keys_off, n, 4)?;
        let vals_end = end(shard.vals_off, n, VAL_LEN)?;
        let fence_end = end(fence_off, n.div_ceil(FENCE), 4)?;
        if shard.content_off != HEADER_LEN as usize
            || shard.docs_off < shard.content_off
            || docs_end != shard.paths_off
            || shard.units_off < shard.paths_off
            || units_end != shard.keys_off
            || keys_end != shard.vals_off
            || vals_end != fence_off
            || fence_end != filter_off
            || shard.postings_off < filter_off
            || shard.postings_off > shard.map.len()
        {
            return Err(corrupt("section offsets out of bounds"));
        }
        let filter_len = shard.postings_off - filter_off;
        if filter_len == 0 {
            return Err(corrupt("empty filter"));
        }
        if !filter_len.is_multiple_of(8) {
            return Err(corrupt("filter is not a whole number of words"));
        }
        shard.filter_words = filter_len / 8;
        shard.fence = (fence_off..fence_end).step_by(4).map(|at| le_u32(&shard.map, at)).collect();
        if home == FilterHome::Heap {
            shard.filter =
                Filter::Heap((filter_off..shard.postings_off).step_by(8).map(|at| le_u64(&shard.map, at)).collect());
        }
        shard.markers = Markers::load(&shard)?;
        Ok(shard)
    }

    fn corrupt(&self, reason: &str) -> Error {
        Error::Corrupt { shard: self.name.clone(), reason: reason.to_string() }
    }

    fn row(&self, doc: DocId) -> Result<usize> {
        if doc >= self.doc_count {
            return Err(self.corrupt("doc id out of range"));
        }
        Ok(self.docs_off + doc as usize * DOC_LEN)
    }

    /// `(unit_count, postings offset)` of `tri`: the fence picks the one block that can
    /// hold it, a binary search of that block's keys finds it.
    fn dict_entry(&self, tri: Trigram) -> Option<(u32, usize)> {
        let n = self.trigram_count as usize;
        // The last block whose first key is <= tri.
        let block = self.fence.partition_point(|&k| k <= tri.0).checked_sub(1)?;
        let (mut lo, mut hi) = (block * FENCE, ((block + 1) * FENCE).min(n));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match le_u32(&self.map, self.keys_off + mid * 4).cmp(&tri.0) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let at = self.vals_off + mid * VAL_LEN;
                    return Some((le_u32(&self.map, at), le_u64(&self.map, at + 4) as usize));
                }
            }
        }
        None
    }
}

impl Shard for NativeShard {
    fn doc_count(&self) -> u32 {
        self.doc_count
    }

    fn meta(&self, doc: DocId) -> Result<DocMeta<'_>> {
        let r = self.row(doc)?;
        let start = self.paths_off + le_u32(&self.map, r + 28) as usize;
        let end = start + le_u32(&self.map, r + 32) as usize;
        let bytes = self.map.get(start..end).ok_or_else(|| self.corrupt("path out of bounds"))?;
        let path = std::str::from_utf8(bytes).map_err(|_| self.corrupt("path is not UTF-8"))?;
        Ok(DocMeta { path: Cow::Borrowed(path), size: le_u64(&self.map, r + 8), mtime_ns: le_u64(&self.map, r + 16) })
    }

    fn postings(&self, tri: Trigram) -> Result<Vec<UnitId>> {
        let Some((count, off)) = self.dict_entry(tri) else {
            return Ok(Vec::new());
        };
        let data = self
            .postings_off
            .checked_add(off)
            .and_then(|start| self.map.get(start..))
            .ok_or_else(|| self.corrupt("postings out of bounds"))?;
        // Every unit takes at least one byte, so a corrupt count cannot over-allocate.
        let mut out = Vec::with_capacity((count as usize).min(data.len()));
        let (mut pos, mut prev) = (0usize, 0u32);
        for _ in 0..count {
            let (mut v, mut shift) = (0u32, 0u32);
            loop {
                let b = *data.get(pos).ok_or_else(|| self.corrupt("truncated postings"))?;
                pos += 1;
                v |= u32::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            prev += v;
            out.push(prev);
        }
        Ok(out)
    }

    fn count(&self, tri: Trigram) -> Result<u32> {
        Ok(self.dict_entry(tri).map_or(0, |(count, _)| count))
    }

    fn may_contain(&self, tri: Trigram) -> bool {
        let (word, bits) = filter_slot(tri.0, self.filter_words);
        let w = match &self.filter {
            Filter::Heap(words) => words[word],
            Filter::Map(off) => le_u64(&self.map, off + word * 8),
        };
        w & bits == bits
    }

    fn markers(&self) -> &Markers {
        &self.markers
    }

    fn unit_doc(&self, unit: UnitId) -> Result<DocId> {
        if unit >= self.unit_count {
            return Err(self.corrupt("unit id out of range"));
        }
        let doc = le_u32(&self.map, self.units_off + unit as usize * 4);
        if doc >= self.doc_count {
            return Err(self.corrupt("unit points past the docs table"));
        }
        Ok(doc)
    }

    fn content(&self, doc: DocId) -> Result<Cow<'_, [u8]>> {
        let r = self.row(doc)?;
        // Checked and held to the content section: a corrupt row's u64 offset could wrap into
        // a valid range of the map and return some other bytes as this document.
        let start = usize::try_from(le_u64(&self.map, r)).ok().and_then(|o| self.content_off.checked_add(o));
        let end = start.and_then(|s| s.checked_add(le_u32(&self.map, r + 24) as usize));
        match (start, end) {
            (Some(start), Some(end)) if end <= self.docs_off => Ok(Cow::Borrowed(&self.map[start..end])),
            _ => Err(self.corrupt("content out of bounds")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::trigram::TrigramExtractor;

    /// `(path, content, unit blocks)`.
    type Doc<'a> = (&'a str, &'a [u8], &'a [&'a [u8]]);

    const MAPPED: NativeBackend = NativeBackend::new(FilterHome::Map);
    const HEAP: NativeBackend = NativeBackend::new(FilterHome::Heap);

    #[test]
    fn round_trips_docs_content_and_postings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        // Doc 1 is split into two units ("hello\n" and "world"), like a large file would be.
        let docs: [Doc<'_>; 3] = [
            ("a.rs", b"fn hello() {}", &[b"fn hello() {}"]),
            ("dir/b.rs", b"hello\nworld", &[b"hello\n", b"world"]),
            ("c.txt", b"nothing", &[b"nothing"]),
        ];
        let mut ex = TrigramExtractor::default();
        let mut w = MAPPED.create(&path).unwrap();
        for (i, (p, c, blocks)) in docs.iter().enumerate() {
            let meta = FileMeta { path: (*p).to_string(), size: c.len() as u64, mtime_ns: i as u64 * 10 };
            let units: Vec<Vec<Trigram>> = blocks.iter().map(|b| ex.extract(b).to_vec()).collect();
            w.add(&meta, c, &units).unwrap();
        }
        assert!(w.add(&FileMeta { path: "empty".into(), size: 0, mtime_ns: 0 }, b"", &[]).is_err());
        w.finish().unwrap();

        let s = MAPPED.open(&path).unwrap();
        assert_eq!(s.doc_count(), 3);
        assert_eq!(s.meta(1).unwrap().path, "dir/b.rs");
        assert_eq!(s.meta(2).unwrap().mtime_ns, 20);
        assert_eq!(&*s.content(1).unwrap(), b"hello\nworld");
        // Units: 0 = a.rs, 1 and 2 = dir/b.rs, 3 = c.txt.
        assert_eq!(s.postings(Trigram::from_bytes(b'h', b'e', b'l')).unwrap(), vec![0, 1]);
        assert_eq!(s.postings(Trigram::from_bytes(b'w', b'o', b'r')).unwrap(), vec![2]);
        assert_eq!(s.postings(Trigram::from_bytes(b'n', b'o', b't')).unwrap(), vec![3]);
        assert!(s.postings(Trigram::from_bytes(b'z', b'z', b'z')).unwrap().is_empty());
        assert_eq!([0, 1, 2, 3].map(|u| s.unit_doc(u).unwrap()), [0, 1, 1, 2]);
        assert!(s.unit_doc(4).is_err());
        // `count` is what the planner orders by, so it must agree with the decoded list; and
        // a filter that rejected a trigram the shard holds would lose matches.
        for (_, c, _) in &docs {
            for &t in ex.extract(c) {
                assert_eq!(s.count(t).unwrap() as usize, s.postings(t).unwrap().len(), "{t:?}");
                assert!(s.may_contain(t), "{t:?}");
            }
        }
        assert!(s.markers().always_search.is_empty() && s.markers().reader_mode.is_empty());
    }

    #[test]
    fn loads_marker_docs_at_open() {
        use scout_core::trigram::{ALWAYS_SEARCH, READER_MODE};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        let t = |b: &[u8; 3]| Trigram::from_bytes(b[0], b[1], b[2]);
        // Doc 1 has two units that both carry READER_MODE: it must be listed once. Doc 2 is
        // marked both ways, the way the indexer marks UTF-16.
        let docs: [(&str, Vec<Vec<Trigram>>); 3] = [
            ("plain", vec![vec![t(b"abc")]]),
            ("bom", vec![vec![t(b"abc"), READER_MODE], vec![t(b"xyz"), READER_MODE]]),
            ("utf16", vec![vec![READER_MODE, ALWAYS_SEARCH]]),
        ];
        let mut w = MAPPED.create(&path).unwrap();
        for (p, units) in &docs {
            w.add(&FileMeta { path: (*p).to_string(), size: 3, mtime_ns: 0 }, b"abc", units).unwrap();
        }
        w.finish().unwrap();
        let s = MAPPED.open(&path).unwrap();
        assert_eq!(s.markers().reader_mode, vec![1, 2]);
        assert_eq!(s.markers().always_search, vec![2]);
        assert!(s.may_contain(READER_MODE) && s.may_contain(ALWAYS_SEARCH));
    }

    #[test]
    fn finds_every_trigram_across_fence_blocks() {
        // Several full fence blocks plus a partial last one, an exact multiple of the block
        // (full last block), one key, and none at all (a shard of files under 3 bytes). One
        // doc per trigram, so trigram k's postings are exactly [k].
        for n in [FENCE * 3 + 17, FENCE * 3, 1, 0].map(|n| n as u32) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.native");
            let mut w = MAPPED.create(&path).unwrap();
            // Even keys only, so the odd ones in between must miss.
            let key = |k: u32| Trigram(0x10_0000 + k * 2);
            for k in 0..n {
                let meta = FileMeta { path: format!("f{k}"), size: 1, mtime_ns: 0 };
                w.add(&meta, b"x", &[vec![key(k)]]).unwrap();
            }
            if n == 0 {
                w.add(&FileMeta { path: "ab".into(), size: 2, mtime_ns: 0 }, b"ab", &[vec![]]).unwrap();
            }
            w.finish().unwrap();
            let s = MAPPED.open(&path).unwrap();
            // The filter holds every key and, at 6 bits a key (~7% false positives),
            // rejects most of the misses in between; with no keys it rejects everything.
            assert!((0..n).all(|k| s.may_contain(key(k))), "{n} keys: a key the filter rejects");
            let rejected = (0..n).filter(|&k| !s.may_contain(Trigram(key(k).0 + 1))).count();
            assert!(rejected * 10 >= n as usize * 8, "{n} keys: only {rejected} misses rejected");
            if n == 0 {
                assert!(
                    (0..1 << 16).all(|t| !s.may_contain(Trigram(t))),
                    "an empty shard's filter lets a trigram through"
                );
            }
            for k in 0..n {
                assert_eq!(s.postings(key(k)).unwrap(), vec![k], "{n} keys: trigram {k}");
                assert_eq!(s.count(key(k)).unwrap(), 1, "{n} keys: count of {k}");
                assert!(s.postings(Trigram(key(k).0 + 1)).unwrap().is_empty(), "{n} keys: after {k}");
                assert_eq!(s.count(Trigram(key(k).0 + 1)).unwrap(), 0, "{n} keys: count after {k}");
            }
            assert!(s.postings(Trigram(0x0f_ffff)).unwrap().is_empty(), "{n} keys: below the first");
            assert!(s.postings(Trigram(key(n).0 + 4)).unwrap().is_empty(), "{n} keys: past the last");
        }
    }

    /// A shard with a few hundred distinct trigrams from real text.
    fn text_shard(path: &Path) {
        let mut ex = TrigramExtractor::default();
        let mut w = MAPPED.create(path).unwrap();
        for (i, line) in [
            "fn parse_header(input: &[u8]) -> Result<Header>",
            "let fence = keys.chunks(FENCE * 4);",
            "Licensed under the Apache License, Version 2.0",
            "useLayoutEffect(() => { window.scrollTo(0, 0) })",
        ]
        .iter()
        .enumerate()
        {
            let meta = FileMeta { path: format!("f{i}"), size: line.len() as u64, mtime_ns: 0 };
            w.add(&meta, line.as_bytes(), &[ex.extract(line.as_bytes()).to_vec()]).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn heap_and_mapped_filters_answer_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        text_shard(&path);
        let (mapped, heap) = (MAPPED.open(&path).unwrap(), HEAP.open(&path).unwrap());
        // Every printable-ASCII trigram: the shard's own and a few hundred thousand misses.
        for t in (0x20_2020u32..0x7f_7f7f).filter(|t| t.to_le_bytes()[..3].iter().all(|b| (0x20..0x7f).contains(b))) {
            assert_eq!(mapped.may_contain(Trigram(t)), heap.may_contain(Trigram(t)), "{t:#x}");
        }
    }

    /// The filter's hash is part of the format: a reader hashing differently from the writer
    /// of an index would reject trigrams its shards hold and silently lose matches, and
    /// nothing in the header would say so. The values come from an independent copy of the
    /// hash in Python (`mix` in bench/harness/filtercheck.py), which E24 checked against
    /// `scout search --stats` on the bench index.
    #[test]
    fn the_filter_hash_is_pinned() {
        assert_eq!(filter_slot(0x0061_6263, 1000), (830, 0x0240_0204_0000_0000));
        assert_eq!(filter_slot(0x007a_6562, 54321), (44414, 0x0800_0020_0020_0080));
        assert_eq!(filter_slot(u32::MAX, 7), (3, 0x1000_0000_0100_0081));
        assert_eq!(filter_slot(u32::MAX - 1, 7), (0, 0x8800_0104_0000_0000));
        assert_eq!(filter_slot(0, 1), (0, 0x0040_8000_1000_0080));
    }

    /// `good` with header offset `i` (0 = content, 7 = filter, 8 = postings) set to `to`.
    fn with_offset(good: &[u8], i: usize, to: u64) -> Vec<u8> {
        let mut bad = good.to_vec();
        bad[24 + i * 8..32 + i * 8].copy_from_slice(&to.to_le_bytes());
        bad
    }

    #[test]
    fn rejects_sections_that_do_not_follow_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        text_shard(&path);
        let good = std::fs::read(&path).unwrap();
        let offset = |i: usize| u64::from_le_bytes(good[24 + i * 8..32 + i * 8].try_into().unwrap());
        let (filter_off, postings_off) = (offset(7), offset(8));
        let cases = [
            (with_offset(&good, 8, postings_off - 4), "filter is not a whole number of words"),
            (with_offset(&good, 8, filter_off), "empty filter"),
            // A gap between the fence and the filter: the writer never leaves one.
            (with_offset(&good, 7, filter_off + 8), "section offsets out of bounds"),
            (with_offset(&good, 0, 0), "section offsets out of bounds"),
            // keys_off + keys would wrap in a release build and pass every comparison.
            (with_offset(&good, 4, u64::MAX - 3), "section offsets out of bounds"),
        ];
        for (bad, want) in cases {
            std::fs::write(&path, &bad).unwrap();
            let err = MAPPED.open(&path).err().unwrap().to_string();
            assert!(err.contains(want), "want {want:?}: {err}");
        }
    }

    #[test]
    fn rejects_content_outside_the_content_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        text_shard(&path);
        let good = std::fs::read(&path).unwrap();
        let docs_off = u64::from_le_bytes(good[24 + 8..32 + 8].try_into().unwrap()) as usize;
        // Doc 0's row starts with its u64 content offset: one that wraps, and one that runs
        // into the docs table.
        for content_off in [u64::MAX - 1, docs_off as u64 - HEADER_LEN] {
            let mut bad = good.clone();
            bad[docs_off..docs_off + 8].copy_from_slice(&content_off.to_le_bytes());
            std::fs::write(&path, &bad).unwrap();
            let s = MAPPED.open(&path).unwrap();
            assert!(s.content(0).err().unwrap().to_string().contains("content out of bounds"), "{content_off}");
        }
    }

    #[test]
    fn rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad");
        std::fs::write(&path, b"not a shard at all, definitely not").unwrap();
        assert!(MAPPED.open(&path).is_err());
        // An older format names the fix instead of claiming corruption; v4 is E20's rejected
        // layout, of which bench indexes still exist.
        std::fs::write(&path, b"SCOUTNI4 and the rest of an old shard").unwrap();
        let err = MAPPED.open(&path).err().unwrap().to_string();
        assert!(err.contains("rebuild the index"), "{err}");
    }
}
