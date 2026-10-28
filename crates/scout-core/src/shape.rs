//! A conservative character-shape filter for regexes without literal trigrams.
//! Four normalized characters retain punctuation around digit runs while collapsing
//! letters and Unicode characters. A Bloom hit is only a candidate, never a match.

use regex_syntax::hir::{Class, ClassBytes, ClassBytesRange, Hir, HirKind, literal::Extractor};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeMask(pub [u64; 4]);

impl ShapeMask {
    #[must_use]
    pub fn from_content(content: &[u8]) -> Self {
        let mut mask = Self([0; 4]);
        let (mut gram, mut width, mut previous) = (0u32, 0usize, None);
        let mut push = |ch: Option<char>| {
            let ch = match ch {
                Some(ch) if ch != '\n' => ch,
                _ => {
                    // A Unicode regex cannot match across invalid bytes, and Scout's
                    // matches never cross newlines. Do not invent adjacency there.
                    width = 0;
                    previous = None;
                    return;
                }
            };
            gram = (gram << 8) | u32::from(normalize(ch));
            width += 1;
            if width >= 4 && previous != Some(gram) {
                mask.insert(gram);
                previous = Some(gram);
            }
        };
        for chunk in content.utf8_chunks() {
            for ch in chunk.valid().chars() {
                push(Some(ch));
            }
            if !chunk.invalid().is_empty() {
                push(None);
            }
        }
        mask
    }

    fn insert(&mut self, gram: u32) {
        // Ordinary letter/punctuation shapes saturate a small Bloom filter. Numeric
        // quads retain the useful constraints of class-heavy numeric regexes without
        // spending its bits on ubiquitous source-code syntax.
        let [a, b, c, d] = gram.to_be_bytes();
        let digits = u8::from(a == b'0') + u8::from(b == b'0') + u8::from(c == b'0') + u8::from(d == b'0');
        if digits < 2 {
            return;
        }
        let mut hash = gram.wrapping_mul(0x9E37_79B9);
        hash ^= hash >> 16;
        hash = hash.wrapping_mul(0x85EB_CA6B);
        for bit in [hash & 255, (hash >> 16) & 255] {
            self.0[(bit >> 6) as usize] |= 1 << (bit & 63);
        }
    }

    #[must_use]
    pub fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (word, dest) in self.0.iter().zip(bytes.chunks_exact_mut(8)) {
            dest.copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let array: &[u8; 32] =
            bytes.try_into().map_err(|_| Error::Invalid("shape mask must contain 32 bytes".into()))?;
        Ok(Self(std::array::from_fn(|i| {
            let word = &array[i * 8..i * 8 + 8];
            u64::from_le_bytes([word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7]])
        })))
    }
}

fn normalize(ch: char) -> u8 {
    if ch.is_ascii_alphabetic() {
        b'a'
    } else if ch.is_ascii_digit() || !ch.is_ascii() {
        b'0'
    } else {
        ch as u8
    }
}

#[derive(Debug)]
pub struct ShapeQuery(Vec<ShapeMask>);

impl ShapeQuery {
    #[must_use]
    pub fn accepts(&self, mask: ShapeMask) -> bool {
        self.0.iter().any(|required| mask.0.iter().zip(required.0).all(|(have, need)| have & need == need))
    }

    pub(crate) fn from_hir(hir: &Hir) -> Option<Self> {
        let mut classes = false;
        let normalized = project(hir, &mut classes)?;
        if !classes {
            return None;
        }
        let literals = Extractor::new()
            .limit_total(64)
            .limit_class(16)
            .limit_repeat(16)
            .limit_literal_len(64)
            .extract(&normalized);
        let mut alternatives = Vec::new();
        for literal in literals.literals()? {
            let bytes = literal.as_bytes();
            // A short alternative imposes no quad constraint. Keeping the other
            // alternatives alone would silently remove its matches.
            if bytes.len() < 4 {
                return None;
            }
            let mut mask = ShapeMask([0; 4]);
            for gram in bytes.windows(4) {
                mask.insert(u32::from_be_bytes(gram.try_into().expect("four-byte window")));
            }
            if mask.0 == [0; 4] {
                return None;
            }
            alternatives.push(mask);
        }
        if alternatives.is_empty() { None } else { Some(Self(alternatives)) }
    }
}

fn project(hir: &Hir, classes: &mut bool) -> Option<Hir> {
    Some(match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Hir::empty(),
        HirKind::Literal(literal) => {
            let text = std::str::from_utf8(&literal.0).ok()?;
            Hir::literal(text.chars().map(normalize).collect::<Vec<_>>())
        }
        HirKind::Class(class) => {
            *classes = true;
            let mut symbols = Vec::new();
            match class {
                Class::Unicode(class) => {
                    for byte in 0..=127u8 {
                        if class.ranges().iter().any(|r| r.start() <= char::from(byte) && char::from(byte) <= r.end()) {
                            symbols.push(normalize(char::from(byte)));
                        }
                    }
                    if class.ranges().iter().any(|r| r.end() > '\u{7F}') {
                        symbols.push(b'0');
                    }
                }
                Class::Bytes(class) => {
                    if class.ranges().iter().any(|r| r.end() > 127) {
                        return None;
                    }
                    for range in class.ranges() {
                        symbols.extend((range.start()..=range.end()).map(|b| normalize(char::from(b))));
                    }
                }
            }
            symbols.sort_unstable();
            symbols.dedup();
            Hir::class(Class::Bytes(ClassBytes::new(symbols.into_iter().map(|b| ClassBytesRange::new(b, b)))))
        }
        HirKind::Repetition(rep) => Hir::repetition(rep.with(project(&rep.sub, classes)?)),
        HirKind::Capture(capture) => project(&capture.sub, classes)?,
        HirKind::Concat(parts) => {
            Hir::concat(parts.iter().map(|part| project(part, classes)).collect::<Option<Vec<_>>>()?)
        }
        HirKind::Alternation(parts) => {
            Hir::alternation(parts.iter().map(|part| project(part, classes)).collect::<Option<Vec<_>>>()?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_include_every_unicode_character_without_relying_on_a_unicode_version() {
        let hir = regex_syntax::parse(r"\d{4}-\d{2}-\d{2}").unwrap();
        let query = ShapeQuery::from_hir(&hir).unwrap();
        for text in ["2026-10-27", "٢٠٢٦-١٠-٢٧", "２０２６-１０-２７", "2026-١٠-２７"] {
            let mask = ShapeMask::from_content(format!("prefix {text} suffix").as_bytes());
            assert!(query.accepts(mask), "{text}");
            assert_eq!(ShapeMask::from_bytes(&mask.to_bytes()).unwrap(), mask);
        }
        assert!(!query.accepts(ShapeMask::from_content(b"Copyright 2012-2026\nordinary letters\n")));
    }

    #[test]
    fn short_alternatives_and_raw_invalid_byte_patterns_impose_no_filter() {
        for pattern in [r"\d{4}|x", r"\d+", r"(?-u:\xFF)\d{4}", "literal"] {
            let hir = regex_syntax::ParserBuilder::new().utf8(false).build().parse(pattern).unwrap();
            assert!(ShapeQuery::from_hir(&hir).is_none(), "{pattern}");
        }
        assert!(ShapeMask::from_bytes(&[0; 31]).is_err());
    }

    #[test]
    fn projected_prefixes_never_reject_real_unicode_or_byte_edge_matches() {
        use grep_matcher::Matcher;
        use grep_regex::RegexMatcherBuilder;
        let samples: &[&[u8]] = &[
            b"2026-10-27",
            "٢٠٢٦-١٠-٢٧".as_bytes(),
            "２０２６-１０-２７".as_bytes(),
            "αβγδ-１２".as_bytes(),
            "2026�12".as_bytes(),
            b"invalid\xff2026-10-27",
            b"2026-10-27\xf0\x90",
            b"2026\xff10-27",
            b"abcd",
            b"2026abcd",
            b"1234-56\r\n2026-10-27\n",
            b"word-1234",
        ];
        for pattern in [
            r"\d{4}-\d{2}-\d{2}",
            r"\p{Greek}{4}-\d{2}",
            r"\d{4}�\d{2}",
            r"(?:\d{4})?abcd",
            r"\d{4}[-_]\d{2}",
            r"\d{4}|abcd",
            r"\b\d{4}-\d{2}",
            r"\D{4}-\d{2}",
        ] {
            let hir = regex_syntax::ParserBuilder::new().multi_line(true).utf8(false).build().parse(pattern).unwrap();
            let query = ShapeQuery::from_hir(&hir);
            let matcher =
                RegexMatcherBuilder::new().multi_line(true).line_terminator(Some(b'\n')).build(pattern).unwrap();
            for &sample in samples {
                if matcher.is_match(sample).unwrap()
                    && let Some(query) = &query
                {
                    assert!(query.accepts(ShapeMask::from_content(sample)), "{pattern}: {sample:?}");
                }
            }
        }
    }
}
