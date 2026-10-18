//! Native shard format (v3).
//!
//! One file per shard, read through `mmap` so the OS decides what stays in RAM. That is
//! what keeps scout usable on a 2 GB machine with an index far larger than memory.
//!
//! ```text
//! magic    8  b"SCOUTNI3"
//! header  80  u32 version, u32 doc_count, u32 trigram_count, u32 unit_count,
//!             u64 content_off, u64 docs_off, u64 paths_off, u64 units_off, u64 keys_off,
//!             u64 vals_off, u64 fence_off, u64 postings_off
//! content     corpus bin: every document's bytes, concatenated
//! docs        doc_count × 40: u64 content_off, u64 size, u64 mtime_ns,
//!             u32 content_len, u32 path_off, u32 path_len, u32 pad
//! paths       UTF-8 relative paths, concatenated
//! units       unit_count × 4: u32 doc of each posting unit (see `UnitId`)
//! keys        trigram_count × 4, sorted: u32 trigram
//! vals        trigram_count × 12, same order: u32 unit_count, u64 postings offset
//! fence       ceil(trigram_count / FENCE) × 4: every FENCE-th key
//! postings    per trigram: unit ids, delta + LEB128 varint encoded
//! ```
//!
//! The dictionary is split so a lookup touches few pages (E19). With v2's interleaved
//! 16-byte entries, a binary search over a shard's ~44k trigrams made ~16 probes over ~10
//! distinct pages, and dictionary lookups cost 3–4.5 ms of a selective query's ~10 ms at
//! 1 CPU: a fresh CLI process faults in every mmapped page it touches. Now the fence
//! (~1.4 KiB, one or two pages, shared by every lookup) picks a block, the block's keys
//! (512 bytes, one page or straddling two) find the trigram, and one value read finishes
//! it. Sections are not page-aligned, so those are typical counts, not bounds.
//!
//! Content comes first so the writer can stream it straight to disk; everything else is
//! small enough to buffer and append in `finish`. All integers are little-endian.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use scout_core::ports::{DocId, DocMeta, FileMeta, IndexBackend, Shard, ShardWriter, UnitId};
use scout_core::trigram::Trigram;
use scout_core::{Error, Result};

const MAGIC: &[u8; 8] = b"SCOUTNI3";
const VERSION: u32 = 3;
const HEADER_LEN: u64 = 8 + 80;
const DOC_LEN: usize = 40;
const VAL_LEN: usize = 12;
/// Keys per fence block: 128 × 4 bytes is one short cache-friendly run (usually within one
/// page), and the fence of a 44k-trigram shard is ~350 keys, ~1.4 KiB.
const FENCE: usize = 128;

#[derive(Debug, Default, Clone, Copy)]
pub struct NativeBackend;

impl IndexBackend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn create(&self, dest: &Path) -> Result<Box<dyn ShardWriter>> {
        Ok(Box::new(NativeWriter::create(dest)?))
    }

    fn open(&self, path: &Path) -> Result<Box<dyn Shard>> {
        Ok(Box::new(NativeShard::open(path)?))
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
        let vals_off = keys_off + keys.len() as u64;
        let fence_off = vals_off + vals.len() as u64;
        let postings_off = fence_off + fence.len() as u64;
        let trigram_count = to_u32(keys.len() / 4, "trigram count")?;

        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&to_u32(self.docs.len(), "doc count")?.to_le_bytes());
        header.extend_from_slice(&trigram_count.to_le_bytes());
        header.extend_from_slice(&to_u32(self.units.len(), "unit count")?.to_le_bytes());
        for off in [HEADER_LEN, docs_off, paths_off, units_off, keys_off, vals_off, fence_off, postings_off] {
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
    fence_off: usize,
    postings_off: usize,
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
    pub fn open(path: &Path) -> Result<Self> {
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
        let shard = Self {
            doc_count: le_u32(&map, 12),
            trigram_count: le_u32(&map, 16),
            unit_count: le_u32(&map, 20),
            content_off: off(0)?,
            docs_off: off(1)?,
            paths_off: off(2)?,
            units_off: off(3)?,
            keys_off: off(4)?,
            vals_off: off(5)?,
            fence_off: off(6)?,
            postings_off: off(7)?,
            name: name.clone(),
            map,
        };
        let docs_end = shard.docs_off + shard.doc_count as usize * DOC_LEN;
        let units_end = shard.units_off + shard.unit_count as usize * 4;
        let n = shard.trigram_count as usize;
        let keys_end = shard.keys_off + n * 4;
        let vals_end = shard.vals_off + n * VAL_LEN;
        let fence_end = shard.fence_off + n.div_ceil(FENCE) * 4;
        if docs_end > shard.paths_off
            || shard.paths_off > shard.units_off
            || units_end > shard.keys_off
            || keys_end > shard.vals_off
            || vals_end > shard.fence_off
            || fence_end > shard.postings_off
            || shard.postings_off > shard.map.len()
        {
            return Err(corrupt("section offsets out of bounds"));
        }
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
        let fences = n.div_ceil(FENCE);
        // The last block whose first key is <= tri.
        let (mut lo, mut hi) = (0usize, fences);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if le_u32(&self.map, self.fence_off + mid * 4) <= tri.0 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let block = lo.checked_sub(1)?;
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
        let start = self.content_off + le_u64(&self.map, r) as usize;
        let end = start + le_u32(&self.map, r + 24) as usize;
        self.map.get(start..end).map(Cow::Borrowed).ok_or_else(|| self.corrupt("content out of bounds"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::trigram::TrigramExtractor;

    /// `(path, content, unit blocks)`.
    type Doc<'a> = (&'a str, &'a [u8], &'a [&'a [u8]]);

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
        let mut w = NativeBackend.create(&path).unwrap();
        for (i, (p, c, blocks)) in docs.iter().enumerate() {
            let meta = FileMeta { path: (*p).to_string(), size: c.len() as u64, mtime_ns: i as u64 * 10 };
            let units: Vec<Vec<Trigram>> = blocks.iter().map(|b| ex.extract(b).to_vec()).collect();
            w.add(&meta, c, &units).unwrap();
        }
        assert!(w.add(&FileMeta { path: "empty".into(), size: 0, mtime_ns: 0 }, b"", &[]).is_err());
        w.finish().unwrap();

        let s = NativeBackend.open(&path).unwrap();
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
    }

    #[test]
    fn finds_every_trigram_across_fence_blocks() {
        // Several full fence blocks plus a partial last one, an exact multiple of the block
        // (full last block), one key, and none at all (a shard of files under 3 bytes). One
        // doc per trigram, so trigram k's postings are exactly [k].
        for n in [FENCE * 3 + 17, FENCE * 3, 1, 0].map(|n| n as u32) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.native");
            let mut w = NativeBackend.create(&path).unwrap();
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
            let s = NativeBackend.open(&path).unwrap();
            for k in 0..n {
                assert_eq!(s.postings(key(k)).unwrap(), vec![k], "{n} keys: trigram {k}");
                assert!(s.postings(Trigram(key(k).0 + 1)).unwrap().is_empty(), "{n} keys: after {k}");
            }
            assert!(s.postings(Trigram(0x0f_ffff)).unwrap().is_empty(), "{n} keys: below the first");
            assert!(s.postings(Trigram(key(n).0 + 4)).unwrap().is_empty(), "{n} keys: past the last");
        }
    }

    #[test]
    fn rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad");
        std::fs::write(&path, b"not a shard at all, definitely not").unwrap();
        assert!(NativeBackend.open(&path).is_err());
        // An older format names the fix instead of claiming corruption.
        std::fs::write(&path, b"SCOUTNI2 and the rest of an old shard").unwrap();
        let err = NativeBackend.open(&path).err().unwrap().to_string();
        assert!(err.contains("rebuild the index"), "{err}");
    }
}
