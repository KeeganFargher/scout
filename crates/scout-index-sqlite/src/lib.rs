//! SQLite [`IndexBackend`].
//!
//! The same shard contract as the native format, stored in one SQLite file per shard:
//! easy to inspect with any SQLite tool and to load into other systems. It trades speed
//! for that (content comes back as an owned copy, postings go through the SQL layer), and
//! the benchmark keeps the two honest against each other.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, params};
use scout_core::ports::{DocId, DocMeta, FileMeta, IndexBackend, Shard, ShardWriter, UnitId};
use scout_core::trigram::Trigram;
use scout_core::{Error, Result};

const SCHEMA: &str = "
    CREATE TABLE docs (id INTEGER PRIMARY KEY, path TEXT NOT NULL, size INTEGER NOT NULL,
                       mtime_ns INTEGER NOT NULL, content BLOB NOT NULL);
    CREATE TABLE units (id INTEGER PRIMARY KEY, doc INTEGER NOT NULL);
    CREATE TABLE postings (trigram INTEGER PRIMARY KEY, units BLOB NOT NULL) WITHOUT ROWID;
";

#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteBackend;

fn db_err(context: &str, e: rusqlite::Error) -> Error {
    Error::backend(context.to_string(), e)
}

/// SQLite stores integers as i64; mtimes fit until the year 2262.
fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::Invalid(format!("{v} does not fit in an SQLite integer")))
}

fn to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| Error::Invalid(format!("negative value {v} in shard")))
}

impl IndexBackend for SqliteBackend {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn create(&self, dest: &Path) -> Result<Box<dyn ShardWriter>> {
        match std::fs::remove_file(dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(dest, e)),
        }
        let conn = Connection::open(dest).map_err(|e| db_err("opening shard", e))?;
        // The shard is rebuilt from scratch on failure, so durability during the build
        // buys nothing; turning it off makes bulk loading several times faster.
        conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; BEGIN;")
            .and_then(|()| conn.execute_batch(SCHEMA))
            .map_err(|e| db_err("creating schema", e))?;
        Ok(Box::new(SqliteWriter { conn, docs: 0, units: 0, content_bytes: 0, pairs: Vec::new() }))
    }

    fn open(&self, path: &Path) -> Result<Box<dyn Shard>> {
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .map_err(|e| db_err("opening shard", e))?;
        let metas = {
            let mut stmt = conn
                .prepare("SELECT id, path, size, mtime_ns FROM docs ORDER BY id")
                .map_err(|e| db_err("reading docs", e))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
                })
                .map_err(|e| db_err("reading docs", e))?;
            let mut metas = Vec::new();
            for (expected, row) in rows.enumerate() {
                let (id, path, size, mtime) = row.map_err(|e| db_err("reading docs", e))?;
                if to_u64(id)? != expected as u64 {
                    return Err(Error::Corrupt { shard: path, reason: "doc ids are not dense".into() });
                }
                metas.push(FileMeta { path, size: to_u64(size)?, mtime_ns: to_u64(mtime)? });
            }
            metas
        };
        // Loaded at open (4 bytes per unit) so mapping units to docs never hits SQL.
        let unit_docs = {
            let mut stmt =
                conn.prepare("SELECT id, doc FROM units ORDER BY id").map_err(|e| db_err("reading units", e))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| db_err("reading units", e))?;
            let mut unit_docs = Vec::new();
            for (expected, row) in rows.enumerate() {
                let (id, doc) = row.map_err(|e| db_err("reading units", e))?;
                let corrupt =
                    |reason: &str| Error::Corrupt { shard: path.display().to_string(), reason: reason.into() };
                if to_u64(id)? != expected as u64 {
                    return Err(corrupt("unit ids are not dense"));
                }
                let doc = u32::try_from(doc).map_err(|_| corrupt("unit doc out of range"))?;
                if doc as usize >= metas.len() {
                    return Err(corrupt("unit points past the docs table"));
                }
                unit_docs.push(doc);
            }
            unit_docs
        };
        Ok(Box::new(SqliteShard { conn: Mutex::new(conn), metas, unit_docs }))
    }
}

pub struct SqliteWriter {
    conn: Connection,
    docs: u32,
    units: u32,
    content_bytes: u64,
    pairs: Vec<u64>,
}

impl ShardWriter for SqliteWriter {
    fn add(&mut self, meta: &FileMeta, content: &[u8], units: &[Vec<Trigram>]) -> Result<DocId> {
        if units.is_empty() {
            return Err(Error::Invalid(format!("{}: a document needs at least one unit", meta.path)));
        }
        let doc = self.docs;
        let (size, mtime) = (to_i64(meta.size)?, to_i64(meta.mtime_ns)?);
        self.conn
            .prepare_cached("INSERT INTO docs (id, path, size, mtime_ns, content) VALUES (?1, ?2, ?3, ?4, ?5)")
            .and_then(|mut s| s.execute(params![doc, meta.path, size, mtime, content]))
            .map_err(|e| db_err("inserting doc", e))?;
        for trigrams in units {
            let unit = self.units;
            self.conn
                .prepare_cached("INSERT INTO units (id, doc) VALUES (?1, ?2)")
                .and_then(|mut s| s.execute(params![unit, doc]))
                .map_err(|e| db_err("inserting unit", e))?;
            self.pairs.extend(trigrams.iter().map(|t| (u64::from(t.0) << 32) | u64::from(unit)));
            self.units += 1;
        }
        self.docs += 1;
        self.content_bytes += content.len() as u64;
        Ok(doc)
    }

    fn content_bytes(&self) -> u64 {
        self.content_bytes
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.pairs.sort_unstable();
        {
            let mut stmt = self
                .conn
                .prepare_cached("INSERT INTO postings (trigram, units) VALUES (?1, ?2)")
                .map_err(|e| db_err("preparing postings insert", e))?;
            let mut i = 0;
            let mut blob = Vec::new();
            while i < self.pairs.len() {
                let tri = (self.pairs[i] >> 32) as u32;
                blob.clear();
                let mut prev = 0u32;
                while i < self.pairs.len() && (self.pairs[i] >> 32) as u32 == tri {
                    let unit = self.pairs[i] as u32;
                    write_varint(&mut blob, unit - prev);
                    prev = unit;
                    i += 1;
                }
                stmt.execute(params![tri, blob]).map_err(|e| db_err("inserting postings", e))?;
            }
        }
        self.conn.execute_batch("COMMIT;").map_err(|e| db_err("committing shard", e))
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub struct SqliteShard {
    /// `Connection` is `Send` but not `Sync`. The engine searches one repo per worker, so
    /// this lock is uncontended in practice.
    conn: Mutex<Connection>,
    /// Loaded at open so `meta` can hand out borrowed paths like the native backend.
    metas: Vec<FileMeta>,
    unit_docs: Vec<DocId>,
}

impl SqliteShard {
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Shard for SqliteShard {
    fn doc_count(&self) -> u32 {
        self.metas.len() as u32
    }

    fn meta(&self, doc: DocId) -> Result<DocMeta<'_>> {
        let m = self.metas.get(doc as usize).ok_or_else(|| Error::Invalid(format!("doc {doc} out of range")))?;
        Ok(DocMeta { path: Cow::Borrowed(&m.path), size: m.size, mtime_ns: m.mtime_ns })
    }

    fn postings(&self, tri: Trigram) -> Result<Vec<UnitId>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT units FROM postings WHERE trigram = ?1")
            .map_err(|e| db_err("preparing postings query", e))?;
        let blob: Option<Vec<u8>> = match stmt.query_row(params![tri.0], |r| r.get(0)) {
            Ok(b) => Some(b),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(db_err("reading postings", e)),
        };
        let Some(blob) = blob else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let (mut v, mut shift, mut prev) = (0u32, 0u32, 0u32);
        for b in &blob {
            v |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                prev += v;
                out.push(prev);
                v = 0;
                shift = 0;
            } else {
                shift += 7;
            }
        }
        // A blob ending inside a varint was cut short; the native backend says the same.
        if blob.last().is_some_and(|b| b & 0x80 != 0) {
            return Err(Error::Invalid(format!("postings for trigram {:#x} are truncated", tri.0)));
        }
        Ok(out)
    }

    fn unit_doc(&self, unit: UnitId) -> Result<DocId> {
        self.unit_docs.get(unit as usize).copied().ok_or_else(|| Error::Invalid(format!("unit {unit} out of range")))
    }

    fn content(&self, doc: DocId) -> Result<Cow<'_, [u8]>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT content FROM docs WHERE id = ?1")
            .map_err(|e| db_err("preparing content query", e))?;
        let bytes: Vec<u8> = stmt.query_row(params![doc], |r| r.get(0)).map_err(|e| db_err("reading content", e))?;
        Ok(Cow::Owned(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::trigram::TrigramExtractor;

    #[test]
    fn round_trips_docs_content_and_postings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sqlite");
        let docs: [(&str, &[u8]); 2] = [("a.rs", b"fn hello() {}"), ("b.rs", b"hello world")];
        let mut ex = TrigramExtractor::default();
        let mut w = SqliteBackend.create(&path).unwrap();
        for (p, c) in docs {
            w.add(&FileMeta { path: p.into(), size: c.len() as u64, mtime_ns: 7 }, c, &[ex.extract(c).to_vec()])
                .unwrap();
        }
        // A third doc split into two units, as the indexer does for large files.
        let split: &[u8] = b"hello\nworld";
        let units = vec![ex.extract(b"hello\n").to_vec(), ex.extract(b"world").to_vec()];
        w.add(&FileMeta { path: "c.rs".into(), size: 11, mtime_ns: 7 }, split, &units).unwrap();
        w.finish().unwrap();

        let s = SqliteBackend.open(&path).unwrap();
        assert_eq!(s.doc_count(), 3);
        assert_eq!(s.meta(1).unwrap().path, "b.rs");
        assert_eq!(&*s.content(0).unwrap(), b"fn hello() {}");
        assert_eq!(s.postings(Trigram::from_bytes(b'h', b'e', b'l')).unwrap(), vec![0, 1, 2]);
        assert_eq!(s.postings(Trigram::from_bytes(b'w', b'o', b'r')).unwrap(), vec![1, 3]);
        assert_eq!([0, 1, 2, 3].map(|u| s.unit_doc(u).unwrap()), [0, 1, 2, 2]);
        assert!(s.postings(Trigram::from_bytes(b'z', b'z', b'z')).unwrap().is_empty());
    }
}
