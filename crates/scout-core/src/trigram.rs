//! Trigram keys and extraction, shared by the indexer and the query planner so the two
//! can never disagree about what gets indexed.

/// A 3-byte sequence packed into the low 24 bits, or a tagged 2-byte sequence,
/// after ASCII case folding.
///
/// Folding at index time means one index serves both case-sensitive and insensitive
/// queries; the regex engine re-checks case on the (few) candidates afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Trigram(pub u32);

/// Pseudo-trigram (outside the 24-bit key space): docs whose bytes the trigram filter
/// cannot reason about (binary up to the first NUL, UTF-16 that rg transcodes), so they
/// are candidates for every query.
pub const ALWAYS_SEARCH: Trigram = Trigram(u32::MAX);

/// Pseudo-trigram: docs that must be searched through rg's reader path (BOM sniffing and
/// transcoding, binary detection with its warning line) rather than as a raw slice.
pub const READER_MODE: Trigram = Trigram(u32::MAX - 1);

/// Number of distinct trigram keys (2^24).
pub const TRIGRAM_SPACE: usize = 1 << 24;
const PAIR_TAG: u32 = 1 << 24;
const PAIR_SPACE: usize = 1 << 16;

#[inline]
#[must_use]
pub fn fold(byte: u8) -> u8 {
    byte.to_ascii_lowercase()
}

impl Trigram {
    #[inline]
    #[must_use]
    pub fn from_pair(a: u8, b: u8) -> Self {
        Self(PAIR_TAG | (u32::from(fold(a)) << 8) | u32::from(fold(b)))
    }
    #[inline]
    #[must_use]
    pub fn from_bytes(a: u8, b: u8, c: u8) -> Self {
        Self((u32::from(fold(a)) << 16) | (u32::from(fold(b)) << 8) | u32::from(fold(c)))
    }
}

/// Whether a trigram can ever be part of a match.
///
/// Searches are line-oriented (the matcher is built with `\n` as line terminator, as
/// rg does), so no match can contain a newline. Skipping these trigrams shrinks the
/// index for free.
#[inline]
fn indexable(a: u8, b: u8, c: u8) -> bool {
    a != b'\n' && b != b'\n' && c != b'\n'
}

/// Reusable per-thread scratch space for extracting distinct three- and two-byte keys.
///
/// A 2^24-bit set (2 MiB), plus 8 KiB for pairs, gives O(1) dedup without hashing; only the bits that were set
/// are cleared afterwards, so cost is proportional to the doc, not the bitset.
pub struct TrigramExtractor {
    seen: Vec<u64>,
    pairs_seen: Vec<u64>,
    found: Vec<Trigram>,
}

impl Default for TrigramExtractor {
    fn default() -> Self {
        Self { seen: vec![0; TRIGRAM_SPACE / 64], pairs_seen: vec![0; PAIR_SPACE / 64], found: Vec::new() }
    }
}

impl TrigramExtractor {
    /// Returns the distinct indexable trigrams of `content`, sorted ascending.
    pub fn extract(&mut self, content: &[u8]) -> &[Trigram] {
        for tri in &self.found {
            if tri.0 & PAIR_TAG != 0 {
                self.pairs_seen[((tri.0 & 0xFFFF) >> 6) as usize] = 0;
            } else {
                self.seen[(tri.0 >> 6) as usize] = 0;
            }
        }
        self.found.clear();
        for w in content.windows(3) {
            self.pair(w[0], w[1]);
            if !indexable(w[0], w[1], w[2]) {
                continue;
            }
            let t = Trigram::from_bytes(w[0], w[1], w[2]);
            let (word, bit) = ((t.0 >> 6) as usize, 1u64 << (t.0 & 63));
            if self.seen[word] & bit == 0 {
                self.seen[word] |= bit;
                self.found.push(t);
            }
        }
        if let Some(pair) = content.windows(2).next_back() {
            self.pair(pair[0], pair[1]);
        }
        self.found.sort_unstable();
        &self.found
    }

    #[inline]
    fn pair(&mut self, a: u8, b: u8) {
        if a == b'\n' || b == b'\n' {
            return;
        }
        let key = Trigram::from_pair(a, b);
        let packed = key.0 & 0xFFFF;
        let (word, bit) = ((packed >> 6) as usize, 1u64 << (packed & 63));
        if self.pairs_seen[word] & bit == 0 {
            self.pairs_seen[word] |= bit;
            self.found.push(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_distinct_folded_trigrams_skipping_newlines() {
        let mut ex = TrigramExtractor::default();
        let got = ex.extract(b"AbcAbc\nab").to_vec();
        let want = {
            let mut v = vec![
                Trigram::from_bytes(b'a', b'b', b'c'),
                Trigram::from_bytes(b'b', b'c', b'a'),
                Trigram::from_bytes(b'c', b'a', b'b'),
                Trigram::from_pair(b'a', b'b'),
                Trigram::from_pair(b'b', b'c'),
                Trigram::from_pair(b'c', b'a'),
            ];
            v.sort_unstable();
            v
        };
        assert_eq!(got, want);
        // Scratch state must not leak into the next doc.
        assert_eq!(
            ex.extract(b"xyz").to_vec(),
            vec![Trigram::from_bytes(b'x', b'y', b'z'), Trigram::from_pair(b'x', b'y'), Trigram::from_pair(b'y', b'z')]
        );
        assert_eq!(ex.extract(b"IF").to_vec(), vec![Trigram::from_pair(b'i', b'f')]);
    }
}
