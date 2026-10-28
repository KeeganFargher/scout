//! Native shard format (v9).
//!
//! One file per shard, read through `mmap` so the OS decides what stays in RAM. That is
//! what keeps scout usable on a 2 GB machine with an index far larger than memory.
//!
//! ```text
//! magic    8  b"SCOUTNI9"
//! header 104  u32 version, u32 doc_count, u32 trigram_count, u32 unit_count,
//!             u32 block_count, u32 pad, u64 content_off, u64 docs_off, u64 blocks_off,
//!             u64 paths_off, u64 units_off, u64 keys_off, u64 vals_off, u64 fence_off,
//!             u64 filter_off, u64 postings_off
//! content     corpus bin: every document's blocks, back to back in doc order
//! docs        doc_count × 72: u64 stored offset of its first block, u64 size,
//!             u64 mtime_ns, u32 content_len, u32 path_off, u32 path_len, u32 first block,
//!             32-byte conservative character-shape mask
//! blocks      block_count × 12: u32 stored length, u32 content length, u32 lines of the
//!             doc before it; a block whose two lengths are equal is stored as is, any
//!             other is an lz4 block
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
//! both in one read-around for a typical shard. Where an opened shard keeps it, and its
//! dictionary, is the backend's [`LookupHome`].
//!
//! Content is cut into blocks (E29), the line-aligned ~64 KiB spans posting units use, so
//! a large file is stored as several. With [`ContentCodec::Lz4`] a block is stored lz4
//! compressed when that makes it smaller: source code shrinks about 3x (chromium +
//! llvm-project: 5.0 GiB to 1.6), so an index that would not fit a small server's page
//! cache may, and every read that misses it moves
//! that much less over a network volume. Reading a compressed doc costs decoding it
//! (~3.6 GB/s per core), which an index that fits RAM uncompressed only loses to, so the
//! codec is the writer's choice per index. A doc whose blocks are all stored as is is
//! still one contiguous slice of the map, read without a copy.
//!
//! A text doc's blocks are its posting units (both are `unit_spans` of its content), and
//! each block records how many lines of the doc come before it (v7, E32): a search that
//! knows which units can match reads and decodes those blocks alone and still numbers
//! their lines as rg would. A binary or BOM doc's units cover only what rg reads of it,
//! so its blocks are not its units; the search reads those whole.
//!
//! Content comes first so the writer can stream it straight to disk; everything else is
//! small enough to buffer and append in `finish`, so every section starts where the one
//! before it ends. All integers are little-endian.
//!
//! v4 was E20's positional-mask format, rejected but built: its shards still exist (bench
//! indexes), so E24's layout took v5; v6 added blocks (E29), v7 their line counts (E32),
//! v8 prototyped character-shape masks, and v9 adds tagged pairs and numeric shape
//! filtering (E34). A shard of an older version is refused with a "rebuild" message instead of being misparsed
//! as this one.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use scout_core::indexer::{UNIT_TARGET, unit_spans};
use scout_core::ports::{
    BlockVisitor, DocId, DocMeta, Extent, FileMeta, IndexBackend, Markers, Shard, ShardWriter, UnitId, Want,
};
use scout_core::shape::ShapeMask;
use scout_core::trigram::Trigram;
use scout_core::{Error, Result};

const MAGIC: &[u8; 8] = b"SCOUTNI9";
const VERSION: u32 = 9;
const HEADER_LEN: u64 = 8 + 104;
/// Where the header's u64 section offsets start, after the magic and six u32s.
const OFFSETS_AT: usize = 32;
const DOC_LEN: usize = 72;
const BLOCK_LEN: usize = 12;
/// The most an lz4 block can decode to per byte: every byte of a match length past the
/// token adds at most 255 bytes of output.
const LZ4_MAX_EXPANSION: usize = 255;
/// The page size the read advice is sized against: 4 KiB on every target scout serves.
const PAGE: usize = 4096;
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

/// Where an opened shard keeps the data every query looks up before it reaches content:
/// its trigram filter (E24) and its dictionary, keys and values (E36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupHome {
    /// Copied to the heap at open. A long-running server probes every shard's filter and
    /// dictionary on every query; on the heap the page cache cannot drop them, so after the
    /// index's pages are evicted (memory pressure on an index bigger than memory) a query
    /// still skips the shards it rejects, and finds its lists, without a read. A dictionary
    /// lookup touches 16 bytes, but a cold one reads the 128 KiB around them: on
    /// chromium + llvm-project keys and values were half of a selective query's cold reads
    /// (E36). Costs 16.75 bytes per key, ~105 MiB there (4% of the index), read once at
    /// open.
    Heap,
    /// Probed in place through the map, so a probe reads a page of the filter (one a cold
    /// open has usually just read with the fence beside it). A process per query (the CLI)
    /// would otherwise copy every shard's filter and dictionary on each start, more than
    /// one query's lookups read.
    Map,
}

/// How a writer stores content blocks (E29). Reading handles both, block by block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentCodec {
    /// Every block as is: no decoding on any read. For an index that fits in memory.
    Raw,
    /// lz4, for each block that it shrinks: about 3x less disk and page cache for source
    /// code, at a decode per read of a compressed doc.
    Lz4,
}

#[derive(Debug, Clone, Copy)]
pub struct NativeBackend {
    lookup: LookupHome,
    codec: ContentCodec,
}

impl NativeBackend {
    /// Writing does not depend on `lookup`, and reading not on `codec`: `lookup` applies to
    /// the shards this backend opens, `codec` to the ones it writes.
    #[must_use]
    pub const fn new(lookup: LookupHome, codec: ContentCodec) -> Self {
        Self { lookup, codec }
    }
}

impl IndexBackend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn create(&self, dest: &Path) -> Result<Box<dyn ShardWriter>> {
        Ok(Box::new(NativeWriter::create(dest, self.codec)?))
    }

    fn open(&self, path: &Path) -> Result<Box<dyn Shard>> {
        Ok(Box::new(NativeShard::open(path, self.lookup)?))
    }
}

struct DocRow {
    /// Where its first block starts in the content section.
    content_off: u64,
    content_len: u32,
    size: u64,
    mtime_ns: u64,
    path_off: u32,
    path_len: u32,
    first_block: u32,
    shape: ShapeMask,
}

pub struct NativeWriter {
    path: PathBuf,
    out: BufWriter<File>,
    codec: ContentCodec,
    /// Content bytes added, before compression: what the shard cap bounds, since building
    /// a shard's postings costs memory in proportion to them.
    content_len: u64,
    /// Bytes written to the content section.
    stored_len: u64,
    docs: Vec<DocRow>,
    /// `(stored length, content length, lines of its doc before it)` of each block.
    blocks: Vec<(u32, u32, u32)>,
    paths: Vec<u8>,
    /// Doc of each unit, indexed by unit id.
    units: Vec<u32>,
    /// `(trigram << 32) | unit`. One flat vector sorts faster and uses far less memory
    /// than a map of per-trigram vectors (no per-key allocation).
    pairs: Vec<u64>,
}

impl NativeWriter {
    fn create(path: &Path, codec: ContentCodec) -> Result<Self> {
        let file = File::create(path).map_err(|e| Error::io(path, e))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        // Header is rewritten in `finish`; reserve its space now.
        out.write_all(&[0u8; HEADER_LEN as usize]).map_err(|e| Error::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            out,
            codec,
            content_len: 0,
            stored_len: 0,
            docs: Vec::new(),
            blocks: Vec::new(),
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
        let row = DocRow {
            content_off: self.stored_len,
            content_len: to_u32(content.len(), "file size")?,
            size: meta.size,
            mtime_ns: meta.mtime_ns,
            path_off: to_u32(self.paths.len(), "path table")?,
            path_len: to_u32(meta.path.len(), "path length")?,
            first_block: to_u32(self.blocks.len(), "block count")?,
            shape: ShapeMask::from_content(content),
        };
        // Blocks are the posting units' spans (line-aligned ~64 KiB), so a later reader can
        // fetch and decode only the units a query needs. An empty doc is one empty block.
        let mut lines = 0u32;
        for span in unit_spans(content, UNIT_TARGET) {
            let raw = &content[span];
            if self.codec == ContentCodec::Lz4 && raw.len() > i32::MAX as usize {
                return Err(Error::Invalid("a compressed content block exceeds the decoder's i32 length".into()));
            }
            // Small blocks cost little cache space but hundreds of thousands of decoder
            // calls on broad queries. Borrow them raw; compress larger blocks only when
            // smaller. Equal lengths are how a reader recognizes raw storage.
            let packed = match self.codec {
                ContentCodec::Lz4 if raw.len() >= 4096 => {
                    Some(lz4_flex::block::compress(raw)).filter(|c| c.len() < raw.len())
                }
                _ => None,
            };
            let stored = packed.as_deref().unwrap_or(raw);
            self.out.write_all(stored).map_err(|e| self.io(e))?;
            self.blocks.push((to_u32(stored.len(), "block")?, to_u32(raw.len(), "block")?, lines));
            self.stored_len += stored.len() as u64;
            // Fewer newlines than bytes, and the doc's length fits a u32 (its row's check).
            lines += memchr::memchr_iter(b'\n', raw).count() as u32;
        }
        self.docs.push(row);
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
        let docs_off = HEADER_LEN + self.stored_len;
        let mut docs = Vec::with_capacity(self.docs.len() * DOC_LEN);
        for d in &self.docs {
            docs.extend_from_slice(&d.content_off.to_le_bytes());
            docs.extend_from_slice(&d.size.to_le_bytes());
            docs.extend_from_slice(&d.mtime_ns.to_le_bytes());
            docs.extend_from_slice(&d.content_len.to_le_bytes());
            docs.extend_from_slice(&d.path_off.to_le_bytes());
            docs.extend_from_slice(&d.path_len.to_le_bytes());
            docs.extend_from_slice(&d.first_block.to_le_bytes());
            docs.extend_from_slice(&d.shape.to_bytes());
        }
        let blocks_off = docs_off + docs.len() as u64;
        let mut blocks = Vec::with_capacity(self.blocks.len() * BLOCK_LEN);
        for &(stored, len, lines) in &self.blocks {
            blocks.extend_from_slice(&stored.to_le_bytes());
            blocks.extend_from_slice(&len.to_le_bytes());
            blocks.extend_from_slice(&lines.to_le_bytes());
        }
        let paths_off = blocks_off + blocks.len() as u64;
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
        header.extend_from_slice(&to_u32(self.blocks.len(), "block count")?.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        for off in [
            HEADER_LEN,
            docs_off,
            blocks_off,
            paths_off,
            units_off,
            keys_off,
            vals_off,
            fence_off,
            filter_off,
            postings_off,
        ] {
            header.extend_from_slice(&off.to_le_bytes());
        }

        let path = self.path.clone();
        let io = |e| Error::io(&path, e);
        self.out.write_all(&docs).map_err(io)?;
        self.out.write_all(&blocks).map_err(io)?;
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
    block_count: u32,
    content_off: usize,
    docs_off: usize,
    blocks_off: usize,
    paths_off: usize,
    units_off: usize,
    keys_off: usize,
    vals_off: usize,
    postings_off: usize,
    /// Every FENCE-th key, copied out of the map at open (E23): every lookup reads it, and
    /// on the heap the page cache cannot drop it (~1.4 KiB per shard).
    fence: Vec<u32>,
    /// The keys and values sections, where the [`LookupHome`] says.
    dict: Dict,
    filter: Filter,
    /// Words in the filter, at least one.
    filter_words: usize,
    markers: Markers,
}

/// A shard's dictionary: its keys then its values, as in the map.
enum Dict {
    Heap(Box<[u8]>),
    Map,
}

/// A shard's trigram filter, where its [`LookupHome`] says.
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
    pub fn open(path: &Path, home: LookupHome) -> Result<Self> {
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
        let off = |i: usize| usize::try_from(le_u64(&map, OFFSETS_AT + i * 8)).map_err(|_| corrupt("offset overflow"));
        let (fence_off, filter_off) = (off(7)?, off(8)?);
        let mut shard = Self {
            doc_count: le_u32(&map, 12),
            trigram_count: le_u32(&map, 16),
            unit_count: le_u32(&map, 20),
            block_count: le_u32(&map, 24),
            content_off: off(0)?,
            docs_off: off(1)?,
            blocks_off: off(2)?,
            paths_off: off(3)?,
            units_off: off(4)?,
            keys_off: off(5)?,
            vals_off: off(6)?,
            postings_off: off(9)?,
            fence: Vec::new(),
            dict: Dict::Map,
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
        let blocks_end = end(shard.blocks_off, shard.block_count as usize, BLOCK_LEN)?;
        let units_end = end(shard.units_off, shard.unit_count as usize, 4)?;
        let keys_end = end(shard.keys_off, n, 4)?;
        let vals_end = end(shard.vals_off, n, VAL_LEN)?;
        let fence_end = end(fence_off, n.div_ceil(FENCE), 4)?;
        if shard.content_off != HEADER_LEN as usize
            || shard.docs_off < shard.content_off
            || docs_end != shard.blocks_off
            || blocks_end != shard.paths_off
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
        if home == LookupHome::Heap {
            shard.filter =
                Filter::Heap((filter_off..shard.postings_off).step_by(8).map(|at| le_u64(&shard.map, at)).collect());
            shard.dict = Dict::Heap(shard.map[shard.keys_off..fence_off].into());
        }
        // A posting list is read once per query that needs it, and its neighbours belong to
        // other trigrams, so a cold fault's 128 KiB read-around on a cloud disk mostly reads
        // lists no one asked for. Without it, a list asks for exactly its range
        // (`postings`): E30, cold geomean 0.91 and warm 0.89 when the index does not fit
        // (1 GiB). The dictionary keeps read-around: turning it off there too read 14% fewer
        // bytes cold (cold time 0.80), but when the index does not fit each lookup became
        // its own synchronous 4 KiB read where one read-around had fetched pages later
        // lookups reuse, and the steady state got slower (1.09 at 1 GiB; 1.46 at 2 GiB on
        // v6, measured with this WILLNEED against neither). `advise_range` rounds the start
        // down to a page, so the filter's last page goes with it.
        shard.advise_random(shard.postings_off..shard.map.len())?;
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

    /// The keys section then the values section, from the heap or the map.
    fn dict(&self) -> &[u8] {
        match &self.dict {
            Dict::Heap(bytes) => bytes,
            Dict::Map => &self.map[self.keys_off..self.vals_off + self.trigram_count as usize * VAL_LEN],
        }
    }

    /// `(unit_count, postings offset, end of its list)` of `tri`: the fence picks the one
    /// block that can hold it, a binary search of that block's keys finds it. Lists are
    /// written in key order, so a list ends where the next key's starts (the last one at
    /// the end of the section).
    fn dict_entry(&self, tri: Trigram) -> Option<(u32, usize, usize)> {
        let n = self.trigram_count as usize;
        let dict = self.dict();
        let vals = n * 4;
        // The last block whose first key is <= tri.
        let block = self.fence.partition_point(|&k| k <= tri.0).checked_sub(1)?;
        let (mut lo, mut hi) = (block * FENCE, ((block + 1) * FENCE).min(n));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match le_u32(dict, mid * 4).cmp(&tri.0) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let at = vals + mid * VAL_LEN;
                    let end = if mid + 1 < n {
                        le_u64(dict, at + VAL_LEN + 4) as usize
                    } else {
                        self.map.len() - self.postings_off
                    };
                    return Some((le_u32(dict, at), le_u64(dict, at + 4) as usize, end));
                }
            }
        }
        None
    }
}

impl Shard for NativeShard {
    fn shape_mask(&self, doc: DocId) -> Result<ShapeMask> {
        let row = self.row(doc)?;
        ShapeMask::from_bytes(&self.map[row + 40..row + DOC_LEN])
    }
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
        let Some((count, off, end)) = self.dict_entry(tri) else {
            return Ok(Vec::new());
        };
        let range = self
            .postings_off
            .checked_add(off)
            .zip(self.postings_off.checked_add(end))
            .filter(|(start, end)| start <= end && *end <= self.map.len())
            .ok_or_else(|| self.corrupt("postings out of bounds"))?;
        let range = range.0..range.1;
        // The postings section reads without read-around (see `open`), so a list longer
        // than a page is asked for whole: one request for its pages instead of a fault, and
        // a wait, per page (E30).
        if range.len() > PAGE {
            self.will_need(range.clone())?;
        }
        let data = &self.map[range];
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
        Ok(self.dict_entry(tri).map_or(0, |(count, _, _)| count))
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

    fn content<'a>(&'a self, doc: DocId, buf: &'a mut Vec<u8>) -> Result<&'a [u8]> {
        let stored = self.stored(doc)?;
        if stored.raw {
            return Ok(&self.map[stored.bytes]);
        }
        // Keep initialized bytes between files: decoding overwrites the entire returned
        // slice, so clearing first would zero the same memory on every read.
        if buf.len() < stored.len {
            buf.resize(stored.len, 0);
        }
        let (mut from, mut to) = (stored.bytes.start, 0usize);
        for b in stored.blocks {
            let (packed, len) = self.block(b);
            self.decode(&self.map[from..from + packed], &mut buf[to..to + len])?;
            from += packed;
            to += len;
        }
        Ok(&buf[..stored.len])
    }

    fn visit_content(&self, want: Want<'_>, buf: &mut Vec<u8>, visit: &mut BlockVisitor<'_>) -> Result<()> {
        let stored = self.stored(want.doc)?;
        match want.extent {
            Extent::Whole if stored.raw => {
                visit(&self.map[stored.bytes], 1)?;
            }
            Extent::Whole => {
                self.visit_blocks(stored.blocks, stored.bytes.start, buf, visit)?;
            }
            Extent::Units(units) => {
                let blocks = self.unit_blocks(want.doc, &stored, units)?;
                let offs = self.offsets(&stored);
                for r in blocks {
                    let start = stored.bytes.start + offs[r.start].0;
                    let keep_going = if stored.raw {
                        let end = stored.bytes.start + offs[r.end].0;
                        visit(&self.map[start..end], self.block_lines(stored.blocks.start + r.start) + 1)?
                    } else {
                        self.visit_blocks(
                            stored.blocks.start + r.start..stored.blocks.start + r.end,
                            start,
                            buf,
                            visit,
                        )?
                    };
                    if !keep_going {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn prefetch(&self, wants: &[Want<'_>]) -> Result<()> {
        // A filtered scan skips small files between candidates. Queue their nearby
        // ranges together instead of issuing repeated syscalls for the same pages.
        // At most 4 KiB per gap is read additionally, within the caller's bounded window.
        let mut run: Option<std::ops::Range<usize>> = None;
        let mut add = |next: std::ops::Range<usize>| -> Result<()> {
            match &mut run {
                Some(r) if next.start >= r.end && next.start - r.end <= 4096 => r.end = next.end,
                _ => {
                    if let Some(done) = run.replace(next) {
                        self.will_need(done)?;
                    }
                }
            }
            Ok(())
        };
        for want in wants {
            let stored = self.stored(want.doc)?;
            match want.extent {
                Extent::Whole => add(stored.bytes)?,
                Extent::Units(units) => {
                    let offs = self.offsets(&stored);
                    let base = stored.bytes.start;
                    for r in self.unit_blocks(want.doc, &stored, units)? {
                        add(base + offs[r.start].0..base + offs[r.end].0)?;
                    }
                }
            }
        }
        match run {
            Some(r) => self.will_need(r),
            None => Ok(()),
        }
    }
}

/// Where a doc's content is stored: its bytes in the map and its blocks.
struct Stored {
    bytes: std::ops::Range<usize>,
    blocks: std::ops::Range<usize>,
    /// Its content length, which its blocks' content lengths add up to.
    len: usize,
    /// Every block stored as is, so `bytes` is the content itself.
    raw: bool,
}

impl NativeShard {
    /// One initialized scratch buffer serves every compressed block. The visitor sees
    /// only the decoded slice, and can stop before any later payload is read or decoded.
    fn visit_blocks(
        &self,
        blocks: std::ops::Range<usize>,
        mut from: usize,
        buf: &mut Vec<u8>,
        visit: &mut BlockVisitor<'_>,
    ) -> Result<bool> {
        for b in blocks {
            let (packed, len) = self.block(b);
            let src = &self.map[from..from + packed];
            let bytes = if packed == len {
                src
            } else {
                if buf.len() < len {
                    buf.resize(len, 0);
                }
                self.decode(src, &mut buf[..len])?;
                &buf[..len]
            };
            if !visit(bytes, self.block_lines(b) + 1)? {
                return Ok(false);
            }
            from += packed;
        }
        Ok(true)
    }

    /// Where `doc` is stored. Checked and held to the content section: a corrupt row's u64
    /// offset could wrap into a valid range of the map and return some other bytes as this
    /// document, and corrupt block lengths could decode past its buffer.
    fn stored(&self, doc: DocId) -> Result<Stored> {
        let r = self.row(doc)?;
        let first = le_u32(&self.map, r + 36) as usize;
        let last = if doc + 1 < self.doc_count {
            le_u32(&self.map, r + DOC_LEN + 36) as usize
        } else {
            self.block_count as usize
        };
        if first >= last || last > self.block_count as usize {
            return Err(self.corrupt("doc blocks out of range"));
        }
        if self.block_lines(first) != 0 {
            return Err(self.corrupt("a doc's first block has lines before it"));
        }
        let (mut packed, mut len, mut raw) = (0usize, 0usize, true);
        for b in first..last {
            let (p, l) = self.block(b);
            // The writer compresses a block only when that shrinks it, and an lz4 block can
            // expand at most ~255x. Held here, before `content` sizes its buffer from these
            // lengths: a corrupt 10-byte block claiming 4 GiB would otherwise allocate and
            // zero 4 GiB per verifying thread before its decode failed.
            if p > l || (p < l && l > p.saturating_mul(LZ4_MAX_EXPANSION)) {
                return Err(self.corrupt("block lengths are not a stored or lz4 block"));
            }
            if p < l && l > i32::MAX as usize {
                return Err(self.corrupt("lz4 block exceeds the decoder's i32 length"));
            }
            packed += p;
            len += l;
            raw &= p == l;
        }
        if len != le_u32(&self.map, r + 24) as usize {
            return Err(self.corrupt("doc blocks do not add up to its length"));
        }
        let start = usize::try_from(le_u64(&self.map, r)).ok().and_then(|o| self.content_off.checked_add(o));
        match start.and_then(|s| Some(s..s.checked_add(packed)?)) {
            Some(bytes) if bytes.end <= self.docs_off => Ok(Stored { bytes, blocks: first..last, len, raw }),
            _ => Err(self.corrupt("content out of bounds")),
        }
    }

    /// `(stored length, content length)` of block `b`, which the caller has bounded by
    /// `block_count`.
    fn block(&self, b: usize) -> (usize, usize) {
        let at = self.blocks_off + b * BLOCK_LEN;
        (le_u32(&self.map, at) as usize, le_u32(&self.map, at + 4) as usize)
    }

    /// One block's content into `dst`, sized to its content length: copied when stored as
    /// is, else lz4-decoded, which must fill `dst` exactly.
    fn decode(&self, src: &[u8], dst: &mut [u8]) -> Result<()> {
        if src.len() == dst.len() {
            dst.copy_from_slice(src);
        } else {
            // The reference decoder validates both input and output bounds. Keep the
            // exact-length check too: an undersized result must never expose old bytes.
            let len =
                i32::try_from(dst.len()).map_err(|_| self.corrupt("lz4 block exceeds the decoder's i32 length"))?;
            if src.len() > i32::MAX as usize
                || lz4::block::decompress_to_buffer(src, Some(len), dst).ok() != Some(dst.len())
            {
                return Err(self.corrupt("lz4 block does not decode to its length"));
            }
        }
        Ok(())
    }

    /// Lines of its doc before block `b`, bounded like [`Self::block`].
    fn block_lines(&self, b: usize) -> u64 {
        u64::from(le_u32(&self.map, self.blocks_off + b * BLOCK_LEN + 8))
    }

    /// `(stored, content)` offset of each of the doc's blocks from the doc's start, plus
    /// the doc's end: `stored.blocks.len() + 1` entries.
    fn offsets(&self, stored: &Stored) -> Vec<(usize, usize)> {
        let mut at = (0usize, 0usize);
        let mut out = Vec::with_capacity(stored.blocks.len() + 1);
        out.push(at);
        for b in stored.blocks.clone() {
            let (p, l) = self.block(b);
            at = (at.0 + p, at.1 + l);
            out.push(at);
        }
        out
    }

    /// The doc's first unit: the units table lists each unit's doc, docs ascending.
    fn first_unit(&self, doc: DocId) -> Result<UnitId> {
        let doc_of = |u: usize| le_u32(&self.map, self.units_off + u * 4);
        let (mut lo, mut hi) = (0usize, self.unit_count as usize);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if doc_of(mid) < doc {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == self.unit_count as usize || doc_of(lo) != doc {
            return Err(self.corrupt("doc has no units"));
        }
        // Below `unit_count`, a u32.
        Ok(lo as UnitId)
    }

    /// `units` of `doc` as runs of adjacent block indexes, relative to its first block. A
    /// text doc's units are its blocks, in order (see the module docs).
    fn unit_blocks(&self, doc: DocId, stored: &Stored, units: &[UnitId]) -> Result<Vec<std::ops::Range<usize>>> {
        if units.is_empty() {
            return Err(Error::Invalid(format!("{}: no units asked of doc {doc}", self.name)));
        }
        let first = self.first_unit(doc)?;
        let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
        for &u in units {
            let k = u
                .checked_sub(first)
                .map(|k| k as usize)
                .filter(|&k| k < stored.blocks.len())
                .ok_or_else(|| self.corrupt("unit outside its doc's blocks"))?;
            match runs.last_mut() {
                Some(r) if r.end == k => r.end += 1,
                Some(r) if r.end > k => return Err(Error::Invalid(format!("{}: units not sorted", self.name))),
                _ => runs.push(k..k + 1),
            }
        }
        Ok(runs)
    }

    /// `madvise(MADV_WILLNEED)`: the kernel queues reads for the range's pages that are not
    /// cached and returns without waiting for them, so one thread puts many reads in flight.
    #[cfg(unix)]
    fn will_need(&self, range: std::ops::Range<usize>) -> Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        self.map.advise_range(memmap2::Advice::WillNeed, range.start, range.len()).map_err(|e| Error::io(&self.name, e))
    }

    /// `madvise(MADV_RANDOM)`: a fault in the range reads its own page, no read-around.
    #[cfg(unix)]
    fn advise_random(&self, range: std::ops::Range<usize>) -> Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        self.map.advise_range(memmap2::Advice::Random, range.start, range.len()).map_err(|e| Error::io(&self.name, e))
    }

    /// Windows has no read-around to turn off for a mapping, short of FFI (see below).
    #[cfg(not(unix))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn advise_random(&self, _: std::ops::Range<usize>) -> Result<()> {
        Ok(())
    }

    /// Windows has `PrefetchVirtualMemory`, but only through FFI, and FFI lives in
    /// scout-source-fs's win.rs alone; the servers this is for run Linux (E28).
    #[cfg(not(unix))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn will_need(&self, _: std::ops::Range<usize>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::trigram::TrigramExtractor;

    /// `(path, content, unit blocks)`.
    type Doc<'a> = (&'a str, &'a [u8], &'a [&'a [u8]]);

    const MAPPED: NativeBackend = NativeBackend::new(LookupHome::Map, ContentCodec::Raw);
    const HEAP: NativeBackend = NativeBackend::new(LookupHome::Heap, ContentCodec::Raw);
    const LZ4: NativeBackend = NativeBackend::new(LookupHome::Map, ContentCodec::Lz4);

    fn content(s: &dyn Shard, doc: DocId) -> Vec<u8> {
        s.content(doc, &mut Vec::new()).unwrap().to_vec()
    }

    fn whole(docs: &[DocId]) -> Vec<Want<'static>> {
        docs.iter().map(|&doc| Want { doc, extent: Extent::Whole }).collect()
    }

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
        assert_eq!(content(&*s, 1), b"hello\nworld");
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
        // Adjacent and scattered docs, and none; a doc past the table fails as content does.
        s.prefetch(&whole(&[0, 1, 2])).unwrap();
        s.prefetch(&whole(&[0, 2])).unwrap();
        s.prefetch(&[]).unwrap();
        assert!(s.prefetch(&whole(&[1, 3])).is_err());
        assert_eq!(content(&*s, 2), b"nothing");
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
    fn heap_and_mapped_lookups_answer_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        text_shard(&path);
        let (mapped, heap) = (MAPPED.open(&path).unwrap(), HEAP.open(&path).unwrap());
        // Every printable-ASCII trigram: the shard's own and a few hundred thousand misses,
        // through the filter and the dictionary (the shard's last key ends its list at the
        // map's end, which the heap copy does not hold).
        let mut found = 0;
        for t in (0x20_2020u32..0x7f_7f7f).filter(|t| t.to_le_bytes()[..3].iter().all(|b| (0x20..0x7f).contains(b))) {
            let t = Trigram(t);
            assert_eq!(mapped.may_contain(t), heap.may_contain(t), "{t:?}");
            let (count, list) = (mapped.count(t).unwrap(), mapped.postings(t).unwrap());
            assert_eq!(count, heap.count(t).unwrap(), "{t:?}");
            assert_eq!(list, heap.postings(t).unwrap(), "{t:?}");
            found += usize::from(count > 0);
        }
        assert!(found > 10, "the shard's own trigrams were looked up: {found}");
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

    /// `good` with header offset `i` (0 = content, 2 = blocks, 8 = filter, 9 = postings) set
    /// to `to`.
    fn with_offset(good: &[u8], i: usize, to: u64) -> Vec<u8> {
        let mut bad = good.to_vec();
        bad[OFFSETS_AT + i * 8..][..8].copy_from_slice(&to.to_le_bytes());
        bad
    }

    #[test]
    fn rejects_sections_that_do_not_follow_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        text_shard(&path);
        let good = std::fs::read(&path).unwrap();
        let offset = |i: usize| u64::from_le_bytes(good[OFFSETS_AT + i * 8..][..8].try_into().unwrap());
        let (blocks_off, filter_off, postings_off) = (offset(2), offset(8), offset(9));
        let cases = [
            (with_offset(&good, 9, postings_off - 4), "filter is not a whole number of words"),
            (with_offset(&good, 9, filter_off), "empty filter"),
            // A gap between the fence and the filter: the writer never leaves one.
            (with_offset(&good, 8, filter_off + 8), "section offsets out of bounds"),
            (with_offset(&good, 0, 0), "section offsets out of bounds"),
            // keys_off + keys would wrap in a release build and pass every comparison.
            (with_offset(&good, 5, u64::MAX - 3), "section offsets out of bounds"),
            // The blocks table must sit between the docs and the paths.
            (with_offset(&good, 2, blocks_off + 8), "section offsets out of bounds"),
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
        let docs_off = u64::from_le_bytes(good[OFFSETS_AT + 8..][..8].try_into().unwrap()) as usize;
        // Doc 0's row starts with its u64 content offset: one that wraps, and one that runs
        // into the docs table.
        for content_off in [u64::MAX - 1, docs_off as u64 - HEADER_LEN] {
            let mut bad = good.clone();
            bad[docs_off..docs_off + 8].copy_from_slice(&content_off.to_le_bytes());
            std::fs::write(&path, &bad).unwrap();
            let s = MAPPED.open(&path).unwrap();
            let err = s.content(0, &mut Vec::new()).err().unwrap().to_string();
            assert!(err.contains("content out of bounds"), "{content_off}: {err}");
            assert!(s.prefetch(&whole(&[0])).is_err(), "{content_off}");
        }
    }

    #[test]
    fn rejects_doc_blocks_that_do_not_match_the_doc() {
        // (doc to read, (offset, u32) writes, expected error)
        type Case<'a> = (DocId, &'a [(usize, u32)], &'a str);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        text_shard(&path);
        let good = std::fs::read(&path).unwrap();
        let at = |i: usize| u64::from_le_bytes(good[OFFSETS_AT + i * 8..][..8].try_into().unwrap()) as usize;
        let (docs_off, blocks_off) = (at(1), at(2));
        // Doc 0 is one raw block; doc 3, the last, ends at the end of the blocks table.
        let (row_len, last_first) = (docs_off + 24, docs_off + 3 * DOC_LEN + 36);
        let cases: [Case<'_>; 7] = [
            // Doc 1's first block before doc 0's: doc 0 would own no block.
            (0, &[(docs_off + DOC_LEN + 36, 0)], "doc blocks out of range"),
            // The last doc starting at the end of the table owns no block either.
            (3, &[(last_first, 4)], "doc blocks out of range"),
            // Doc 0's block claims more content than its row: decoding would overrun.
            (0, &[(blocks_off + 4, 1000)], "do not add up"),
            // Stored larger than its content: the writer stores such a block as is.
            (0, &[(blocks_off, u32::MAX)], "not a stored or lz4 block"),
            // 10 bytes claiming 4 GiB, row to match: past lz4's expansion, refused before
            // `content` sizes a buffer from it.
            (0, &[(blocks_off, 10), (blocks_off + 4, u32::MAX), (row_len, u32::MAX)], "not a stored or lz4 block"),
            // A reference LZ4 block is bounded by signed 32-bit lengths. Reject this
            // metadata before allocating the claimed two-GiB decode buffer.
            (0, &[(blocks_off, 1 << 24), (blocks_off + 4, 1 << 31), (row_len, 1 << 31)], "decoder's i32 length"),
            // Consistent lengths that run into the docs table.
            (0, &[(blocks_off, 1 << 20), (blocks_off + 4, 1 << 20), (row_len, 1 << 20)], "content out of bounds"),
        ];
        for (doc, writes, want) in cases {
            let mut bad = good.clone();
            for &(offset, value) in writes {
                bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            std::fs::write(&path, &bad).unwrap();
            let s = MAPPED.open(&path).unwrap();
            let err = s.content(doc, &mut Vec::new()).err().unwrap().to_string();
            assert!(err.contains(want), "want {want:?}: {err}");
        }
    }

    #[test]
    fn rejects_an_lz4_block_that_does_not_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.native");
        let text: Vec<u8> = (0..2_000)
            .flat_map(|i| {
                format!(
                    "let value_{i} = compute({});
",
                    i % 7
                )
                .into_bytes()
            })
            .collect();
        let mut w = LZ4.create(&path).unwrap();
        let units = vec![TrigramExtractor::default().extract(&text).to_vec()];
        w.add(&FileMeta { path: "a.rs".into(), size: text.len() as u64, mtime_ns: 0 }, &text, &units).unwrap();
        w.finish().unwrap();
        let good = std::fs::read(&path).unwrap();
        let blocks_off = u64::from_le_bytes(good[OFFSETS_AT + 16..][..8].try_into().unwrap()) as usize;
        let stored = u32::from_le_bytes(good[blocks_off..][..4].try_into().unwrap()) as usize;
        assert!(stored < text.len(), "the block is compressed");
        // Truncate the block by one byte: its lengths still pass every check, but its last
        // sequence is cut short, so decoding cannot fill the content length.
        let mut bad = good.clone();
        bad[blocks_off..][..4].copy_from_slice(&u32::try_from(stored - 1).unwrap().to_le_bytes());
        std::fs::write(&path, &bad).unwrap();
        let s = MAPPED.open(&path).unwrap();
        let err = s.content(0, &mut Vec::new()).err().unwrap().to_string();
        assert!(err.contains("does not decode to its length"), "{err}");
    }

    #[test]
    fn small_lz4_documents_borrow_the_map_without_touching_decode_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.native");
        let files = [vec![b'a'; 4095], vec![b'b'; 4096]];
        let mut writer = LZ4.create(&path).unwrap();
        let mut extractor = TrigramExtractor::default();
        for (doc, content) in files.iter().enumerate() {
            let meta = FileMeta { path: format!("{doc}.txt"), size: content.len() as u64, mtime_ns: 0 };
            writer.add(&meta, content, &[extractor.extract(content).to_vec()]).unwrap();
        }
        writer.finish().unwrap();
        let shard = LZ4.open(&path).unwrap();
        let mut buf = vec![0xA5; 8192];
        assert_eq!(shard.content(0, &mut buf).unwrap(), files[0]);
        assert!(buf.iter().all(|&byte| byte == 0xA5), "raw small files must not copy into scratch");
        assert_eq!(shard.content(1, &mut buf).unwrap(), files[1]);
        assert_eq!(&buf[..4096], files[1], "larger blocks still use the decoder");
    }

    #[test]
    fn lz4_content_round_trips_and_shrinks_the_shard() {
        let dir = tempfile::tempdir().unwrap();
        // Source-like text over several blocks, bytes lz4 cannot shrink (stored as is, next
        // to compressed blocks of the same shard), and an empty file.
        let text: Vec<u8> =
            (0..20_000).flat_map(|i| format!("let value_{i} = compute(input, {});\n", i % 7).into_bytes()).collect();
        let noise: Vec<u8> = (0u32..70_000).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let files: [(&str, &[u8]); 4] =
            [("big.rs", &text), ("noise.bin", &noise), ("empty", b""), ("small.rs", b"fn x() {}\n")];
        let mut sizes = Vec::new();
        for backend in [MAPPED, LZ4] {
            let path = dir.path().join(format!("{:?}.native", backend.codec));
            let mut ex = TrigramExtractor::default();
            let mut w = backend.create(&path).unwrap();
            for (p, c) in files {
                let units: Vec<Vec<Trigram>> =
                    unit_spans(c, UNIT_TARGET).into_iter().map(|r| ex.extract(&c[r]).to_vec()).collect();
                w.add(&FileMeta { path: p.into(), size: c.len() as u64, mtime_ns: 0 }, c, &units).unwrap();
            }
            assert_eq!(
                w.content_bytes(),
                files.iter().map(|(_, c)| c.len() as u64).sum::<u64>(),
                "raw bytes, for the cap"
            );
            w.finish().unwrap();
            let s = MAPPED.open(&path).unwrap();
            let mut buf = Vec::new();
            for (doc, (_, c)) in files.iter().enumerate() {
                let doc = DocId::try_from(doc).unwrap();
                assert_eq!(s.content(doc, &mut buf).unwrap(), *c, "{:?} doc {doc}", backend.codec);
            }
            s.prefetch(&whole(&[0, 1, 2, 3])).unwrap();
            sizes.push(std::fs::metadata(&path).unwrap().len());
        }
        assert!(sizes[1] * 3 < sizes[0], "lz4 shard {} vs raw {}", sizes[1], sizes[0]);
    }

    #[test]
    fn visits_only_the_asked_units_with_their_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        // A small doc, then one of many units, as the indexer splits it.
        let big: Vec<u8> = (0..30_000).flat_map(|i| format!("line {i} = value({});\n", i % 13).into_bytes()).collect();
        let spans = unit_spans(&big, UNIT_TARGET);
        assert!(spans.len() >= 6, "{} units", spans.len());
        let files: [(&str, &[u8]); 2] = [("small.rs", b"fn x() {}\n"), ("big.rs", &big)];
        for backend in [MAPPED, LZ4] {
            let path = dir.path().join(format!("{:?}.native", backend.codec));
            let mut ex = TrigramExtractor::default();
            let mut w = backend.create(&path).unwrap();
            for (p, c) in files {
                let units: Vec<Vec<Trigram>> =
                    unit_spans(c, UNIT_TARGET).into_iter().map(|r| ex.extract(&c[r]).to_vec()).collect();
                w.add(&FileMeta { path: p.into(), size: c.len() as u64, mtime_ns: 0 }, c, &units).unwrap();
            }
            w.finish().unwrap();
            let s = MAPPED.open(&path).unwrap();
            // big.rs is doc 1 and its units start at 1, after small.rs's one.
            let unit = |k: usize| UnitId::try_from(1 + k).unwrap();
            let line_of = |at: usize| memchr::memchr_iter(b'\n', &big[..at]).count() as u64 + 1;
            // Raw adjacent units can be borrowed together; compressed units are decoded
            // one at a time. Both must cover exactly the requested lines, in file order.
            let last = spans.len() - 1;
            let asked = [unit(1), unit(2), unit(4), unit(last)];
            let mut buf = Vec::new();
            let mut actual = Vec::new();
            let mut next = 0;
            s.visit_content(Want { doc: 1, extent: Extent::Units(&asked) }, &mut buf, &mut |bytes, line| {
                let at = asked[next] as usize - 1;
                assert_eq!(line, line_of(spans[at].start));
                actual.extend_from_slice(bytes);
                let mut consumed = 0;
                while consumed < bytes.len() {
                    consumed += spans[asked[next] as usize - 1].len();
                    next += 1;
                }
                assert_eq!(consumed, bytes.len());
                Ok(true)
            })
            .unwrap();
            let expected: Vec<u8> =
                asked.iter().flat_map(|&u| big[spans[u as usize - 1].clone()].iter().copied()).collect();
            assert_eq!(actual, expected);
            assert_eq!(next, asked.len());
            // The first unit starts on line 1; the small doc is its own single unit.
            s.visit_content(Want { doc: 1, extent: Extent::Units(&[unit(0)]) }, &mut buf, &mut |bytes, line| {
                assert_eq!(line, 1);
                assert_eq!(bytes, &big[spans[0].clone()]);
                Ok(true)
            })
            .unwrap();
            s.visit_content(Want { doc: 0, extent: Extent::Units(&[0]) }, &mut buf, &mut |bytes, line| {
                assert_eq!(line, 1);
                assert_eq!(bytes, b"fn x() {}\n");
                Ok(true)
            })
            .unwrap();
            // A unit of another doc, unsorted units and no units are refused.
            for (doc, units) in [(0, vec![1]), (1, vec![unit(2), unit(1)]), (1, vec![])] {
                assert!(
                    s.visit_content(Want { doc, extent: Extent::Units(&units) }, &mut buf, &mut |_, _| Ok(true))
                        .is_err()
                );
            }
            s.prefetch(&[Want { doc: 1, extent: Extent::Units(&asked) }, Want { doc: 0, extent: Extent::Whole }])
                .unwrap();
        }
    }

    #[test]
    fn stopped_visits_never_decode_later_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lazy.native");
        let text = b"needle in a compressible line\n".repeat(12_000);
        let spans = unit_spans(&text, UNIT_TARGET);
        assert!(spans.len() > 2);
        let mut ex = TrigramExtractor::default();
        let units: Vec<Vec<Trigram>> = spans.iter().map(|r| ex.extract(&text[r.clone()]).to_vec()).collect();
        let mut w = LZ4.create(&path).unwrap();
        w.add(&FileMeta { path: "large.txt".into(), size: text.len() as u64, mtime_ns: 0 }, &text, &units).unwrap();
        w.finish().unwrap();
        let shard = NativeShard::open(&path, LookupHome::Map).unwrap();
        let stored = shard.stored(0).unwrap();
        let second = stored.blocks.start + 1;
        let start = stored.bytes.start + shard.block(stored.blocks.start).0;
        let packed = shard.block(second).0;
        drop(shard);
        // Metadata remains valid; only decoding the second block can discover the bad
        // payload. An eager implementation therefore fails even when the visitor stops.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[start..start + packed].fill(0);
        std::fs::write(&path, bytes).unwrap();
        let shard = NativeShard::open(&path, LookupHome::Map).unwrap();
        let mut buf = Vec::new();
        for extent in [Extent::Whole, Extent::Units(&[0, 1])] {
            let mut calls = 0;
            shard
                .visit_content(Want { doc: 0, extent }, &mut buf, &mut |bytes, line| {
                    calls += 1;
                    assert_eq!(bytes, &text[spans[0].clone()]);
                    assert_eq!(line, 1);
                    Ok(false)
                })
                .unwrap();
            assert_eq!(calls, 1);
            assert!(shard.visit_content(Want { doc: 0, extent }, &mut buf, &mut |_, _| Ok(true)).is_err());
        }
        assert!(shard.content(0, &mut buf).is_err());
    }

    #[test]
    fn initialized_decode_storage_survives_smaller_reads_without_exposing_stale_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reuse.native");
        let big = b"a long repeated line of text\n".repeat(9_000);
        let small = b"different short line\n".repeat(8);
        let files: [&[u8]; 3] = [&big, &small, b""];
        let mut ex = TrigramExtractor::default();
        let mut w = LZ4.create(&path).unwrap();
        for text in files {
            let units: Vec<Vec<Trigram>> =
                unit_spans(text, UNIT_TARGET).iter().map(|r| ex.extract(&text[r.clone()]).to_vec()).collect();
            w.add(&FileMeta { path: "text".into(), size: text.len() as u64, mtime_ns: 0 }, text, &units).unwrap();
        }
        w.finish().unwrap();
        let shard = LZ4.open(&path).unwrap();
        let mut buf = vec![0xA5; big.len() + 7];
        let (ptr, initialized) = (buf.as_ptr(), buf.len());
        for doc in [0, 1, 2, 1, 0] {
            assert_eq!(shard.content(doc, &mut buf).unwrap(), files[doc as usize]);
            assert_eq!(buf.as_ptr(), ptr);
            assert_eq!(buf.len(), initialized, "smaller reads must retain initialized storage");
        }
        let mut actual = Vec::new();
        shard
            .visit_content(Want { doc: 0, extent: Extent::Whole }, &mut buf, &mut |bytes, _| {
                actual.extend_from_slice(bytes);
                Ok(true)
            })
            .unwrap();
        assert_eq!(actual, big);
        assert_eq!(buf.as_ptr(), ptr);
        assert_eq!(buf.len(), initialized);
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
