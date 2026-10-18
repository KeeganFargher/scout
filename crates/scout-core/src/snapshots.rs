//! Every repo's source snapshot, in one store file.
//!
//! One file rather than one per repo: a strict search reads every snapshot, and on Windows
//! each file open costs ~85 µs where an on-access scanner looks at it (measured, E8a) —
//! 137 opens were ~12 ms of a single-threaded search. One file also makes persisting
//! cheap enough to do often: advancing every repo's journal cursor is one write, not 137.
//!
//! Concurrent searches may each rewrite the pack; the last writer wins, and an update from
//! the other writer can be lost. That is sound because every snapshot is valid at its own
//! journal cursor: a lost update only means the next search starts from an older snapshot
//! and replays more of the journal (or scans, if the journal no longer reaches back).
//!
//! ```text
//! magic "SCOUTSP1"
//! u32 count
//! count × { u32 name_len, name (UTF-8), u64 len, bytes }      little-endian
//! ```

use std::collections::BTreeMap;

use crate::ports::ShardStore;
use crate::{Error, Result};

/// Store name of the pack.
pub const PACK_NAME: &str = "snapshots.pack";
const MAGIC: &[u8; 8] = b"SCOUTSP1";

/// Snapshot bytes by repo name. The bytes are opaque here; the repo's source decodes them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SnapshotPack {
    pub entries: BTreeMap<String, Vec<u8>>,
}

impl SnapshotPack {
    /// The stored pack; empty when there is none yet (a fresh index directory).
    pub fn load(store: &dyn ShardStore) -> Result<Self> {
        let path = store.local_path(PACK_NAME)?;
        match std::fs::read(&path) {
            Ok(bytes) => Self::decode(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Writes to a per-process staging name and publishes by rename, so readers see the
    /// old or the new pack, never a torn one, and concurrent writers never share a file.
    pub fn save(&self, store: &dyn ShardStore) -> Result<()> {
        let staged = store.staging_path(&format!("{PACK_NAME}.{}", std::process::id()))?;
        std::fs::write(&staged, self.encode()?).map_err(|e| Error::io(&staged, e))?;
        store.replace(&staged, PACK_NAME)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = MAGIC.to_vec();
        let size = self.entries.iter().map(|(n, b)| 12 + n.len() + b.len()).sum::<usize>();
        out.reserve(4 + size);
        out.extend_from_slice(&len_u32(self.entries.len())?.to_le_bytes());
        for (name, bytes) in &self.entries {
            out.extend_from_slice(&len_u32(name.len())?.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader { bytes, pos: 0 };
        if r.take(MAGIC.len())? != MAGIC {
            return Err(corrupt("bad magic"));
        }
        let count = r.u32()?;
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let name_len = r.u32()? as usize;
            let name = std::str::from_utf8(r.take(name_len)?).map_err(|_| corrupt("repo name is not UTF-8"))?;
            let len = usize::try_from(r.u64()?).map_err(|_| corrupt("entry too large"))?;
            let data = r.take(len)?.to_vec();
            if entries.insert(name.to_string(), data).is_some() {
                return Err(corrupt(&format!("duplicate entry {name:?}")));
            }
        }
        if r.pos != bytes.len() {
            return Err(corrupt("trailing bytes"));
        }
        Ok(Self { entries })
    }
}

fn len_u32(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| Error::Invalid(format!("snapshot pack field too large: {n}")))
}

fn corrupt(reason: &str) -> Error {
    Error::Corrupt { shard: PACK_NAME.to_string(), reason: reason.to_string() }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.bytes.len()).ok_or_else(|| corrupt("truncated"))?;
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| corrupt("truncated"))?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| corrupt("truncated"))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> SnapshotPack {
        let mut p = SnapshotPack::default();
        p.entries.insert("a".into(), vec![1, 2, 3]);
        p.entries.insert("empty".into(), vec![]);
        p.entries.insert("ünï".into(), vec![0; 1000]);
        p
    }

    #[test]
    fn round_trips() {
        assert_eq!(SnapshotPack::decode(&pack().encode().unwrap()).unwrap(), pack());
        assert_eq!(SnapshotPack::decode(&SnapshotPack::default().encode().unwrap()).unwrap(), SnapshotPack::default());
    }

    #[test]
    fn damage_is_an_error() {
        let bytes = pack().encode().unwrap();
        for cut in [0, 7, 8, 11, 20, bytes.len() - 1] {
            assert!(SnapshotPack::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(SnapshotPack::decode(&trailing).is_err());
        let mut magic = bytes;
        magic[0] = b'X';
        assert!(SnapshotPack::decode(&magic).is_err());
    }
}
