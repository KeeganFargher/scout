#!/usr/bin/env python3
"""Simulates E24's filter design on the v3 bench index's real shard keys, before it was
built: one u64 word per key (picked by multiply-shift from the top 32 hash bits), k bits set
inside it from the low hash bits, b bits of filter per key; the hash is shardfmt.py's copy
of the one lib.rs shipped. Prints, per literal bench query and (b, k), how many shards the
filter lets through against the exact count; E24 chose b=6, k=4.

    bloomsim.py <v3 index dir> <queries.tsv>
"""
import glob
import os
import sys

import shardfmt

CONFIGS = [(b, k) for b in (4, 5, 6, 8) for k in (2, 3, 4)]


def main():
    index, qpath = sys.argv[1:]
    shards = []
    for path in sorted(glob.glob(os.path.join(index, "shards", "*.native"))):
        (_, tris, _), s = shardfmt.read(path, shardfmt.V3)
        keys = shardfmt.keys(s, tris)
        shards.append((set(keys), [shardfmt.mix(t) for t in keys]))
    qs = []
    for line in open(qpath, encoding="utf8"):
        if line.startswith("#") or not line.strip():
            continue
        name, flags, pattern = line.rstrip("\n").split("\t")
        if "-F" in flags.split():
            qs.append((name, sorted(shardfmt.trigrams(pattern))))
    exact = {n: sum(all(t in keys for t in ts) for keys, _ in shards) for n, ts in qs}
    lets = {c: {n: 0 for n, _ in qs} for c in CONFIGS}
    for keys, hashes in shards:
        for b, k in CONFIGS:
            words = [0] * max(1, -(-len(keys) * b // 64))
            for h in hashes:
                words[shardfmt.word_of(h, len(words))] |= shardfmt.bits_of(h, k)
            for n, ts in qs:
                hs = [shardfmt.mix(t) for t in ts]
                lets[(b, k)][n] += all(
                    words[shardfmt.word_of(h, len(words))] & shardfmt.bits_of(h, k) == shardfmt.bits_of(h, k) for h in hs)
    total = sum(len(keys) for keys, _ in shards)
    print(f"{'query':28s} exact " + " ".join(f"b{b}k{k}" for b, k in CONFIGS))
    for n, _ in qs:
        print(f"{n:28s} {exact[n]:5d} " + " ".join(f"{lets[c][n]:5d}" for c in CONFIGS))
    print(f"{'MiB':28s} {'':5s} " + " ".join(f"{total * b / 8 / 2**20:5.1f}" for b, _ in CONFIGS))
    print(f"{'sum over queries':28s} {sum(exact.values()):5d} " + " ".join(f"{sum(lets[c].values()):5d}" for c in CONFIGS))


main()
