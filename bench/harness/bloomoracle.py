#!/usr/bin/env python3
"""E24's upper bound, run on the v3 bench index before the filter was built: per literal
bench query, how many shards hold all of its trigrams (all an exact per-shard filter lets
through), and how many a Bloom filter of b bits a key would let through by its expected
false-positive rate (FPR^m for a shard lacking m of the query's trigrams). Also the size of
each shard section, summed.

    bloomoracle.py <v3 index dir> <queries.tsv>
"""
import glob
import math
import os
import sys

import shardfmt

BITS = [2, 3, 4, 6, 10]


def fpr(b):
    k = max(1, round(b * math.log(2)))
    return k, (1 - math.exp(-k / b)) ** k


def main():
    index, qpath = sys.argv[1:]
    shards = []
    for path in sorted(glob.glob(os.path.join(index, "shards", "*.native"))):
        (_, tris, _), s = shardfmt.read(path, shardfmt.V3)
        shards.append({"keys": set(shardfmt.keys(s, tris)), "tris": tris, "sections": {n: len(b) for n, b in s.items()}})
    n = len(shards)
    tot = sum(s["tris"] for s in shards)
    counts = sorted(s["tris"] for s in shards)
    print(f"shards {n}, trigram keys total {tot:,}, per shard min {counts[0]:,} median {counts[n // 2]:,} max {counts[-1]:,}")
    size = {name: sum(s["sections"][name] for s in shards) / 2**20 for name in shardfmt.V3[1]}
    print("MiB per section: " + ", ".join(f"{name} {mib:.1f}" for name, mib in size.items()))
    for b in [2, 3, 4, 5, 6, 8, 10]:
        k, p = fpr(b)
        print(f"  {b:2d} bits/key: {tot * b / 8 / 2**20:5.1f} MiB, k={k}, FPR {p:.3f}")
    print(f"\n{'query':28s} {'tris':>4s} {'need':>5s} " + " ".join(f"{b:>5d}b" for b in BITS)
          + f"   (shards a filter lets through, of {n})")
    for line in open(qpath, encoding="utf8"):
        if line.startswith("#") or not line.strip():
            continue
        name, flags, pattern = line.rstrip("\n").split("\t")
        if "-F" not in flags.split():
            continue
        ts = shardfmt.trigrams(pattern)
        absent = [sum(t not in s["keys"] for t in ts) for s in shards]
        row = [sum(1 if a == 0 else fpr(b)[1] ** a for a in absent) for b in BITS]
        print(f"{name:28s} {len(ts):4d} {sum(a == 0 for a in absent):5d} " + " ".join(f"{r:6.1f}" for r in row))


main()
