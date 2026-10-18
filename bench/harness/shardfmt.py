"""The native shard layouts the bench scripts read (crates/scout-index-native/src/lib.rs),
and an independent copy of the v5 trigram filter's hash and of trigram packing, so a script
can check the Rust writer and reader against code they do not share."""
import struct

# (magic, section names in file order); every section ends where the next one starts.
V3 = (b"SCOUTNI3", ["content", "docs", "paths", "units", "keys", "vals", "fence", "postings"])
V5 = (b"SCOUTNI5", ["content", "docs", "paths", "units", "keys", "vals", "fence", "filter", "postings"])
FILTER_K = 4  # FILTER_K in lib.rs
M = (1 << 64) - 1


def read(path, fmt):
    """((docs, trigrams, units), {section name: bytes}) of the shard at path; fails unless
    the file has fmt's magic."""
    magic, names = fmt
    data = open(path, "rb").read()
    if data[:8] != magic:
        raise SystemExit(f"{path}: magic {data[:8]!r}, want {magic!r}")
    counts = struct.unpack_from("<3I", data, 12)
    offs = struct.unpack_from(f"<{len(names)}Q", data, 24)
    ends = list(offs[1:]) + [len(data)]
    return counts, {n: data[o:e] for n, o, e in zip(names, offs, ends)}


def keys(sections, trigram_count):
    """The shard's sorted dictionary keys."""
    return struct.unpack(f"<{trigram_count}I", sections["keys"])


def filter_words(sections):
    f = sections["filter"]
    if not f or len(f) % 8:
        raise SystemExit(f"a filter of {len(f)} bytes is not a whole, non-zero number of words")
    return struct.unpack(f"<{len(f) // 8}Q", f)


def mix(x):
    """splitmix64's finaliser, as filter_slot."""
    z = (x + 0x9E3779B97F4A7C15) & M
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M
    return z ^ (z >> 31)


def bits_of(h, k):
    """The bits of its word a key with hash h sets, k of them from the low 6k bits."""
    bits = 0
    for i in range(k):
        bits |= 1 << ((h >> (6 * i)) & 63)
    return bits


def word_of(h, words):
    """The word of a words-word filter a key with hash h lives in (multiply-shift)."""
    return ((h >> 32) * words) >> 32


def may_contain(words, tri):
    h = mix(tri)
    bits = bits_of(h, FILTER_K)
    return words[word_of(h, len(words))] & bits == bits


def trigrams(literal):
    """Trigram::new over a literal: ASCII-folded bytes packed into the low 24 bits."""
    b = literal.encode().lower()
    return {(b[i] << 16) | (b[i + 1] << 8) | b[i + 2] for i in range(len(b) - 2)}
