#!/usr/bin/env python3
"""Size of every native v9 shard section, summed over an index's shards, in MiB.

    sections.py <index-dir>

The question it answers: how much memory holding a set of sections on the heap would
take (E36), e.g. everything a query reads before it reaches content.
"""
import os
import struct
import sys

SECTIONS = ["content", "docs", "blocks", "paths", "units", "keys", "vals", "fence", "filter", "postings"]
shards = os.path.join(sys.argv[1], "shards")
total = dict.fromkeys(SECTIONS, 0)
count = 0
for name in sorted(n for n in os.listdir(shards) if n.endswith(".native")):
    path = os.path.join(shards, name)
    with open(path, "rb") as f:
        head = f.read(112)
    if head[:8] != b"SCOUTNI9":
        raise SystemExit(f"{path}: not a v9 shard")
    offs = list(struct.unpack_from("<10Q", head, 32))
    ends = offs[1:] + [os.path.getsize(path)]
    for s, a, b in zip(SECTIONS, offs, ends):
        total[s] += b - a
    count += 1
print(f"{count} shards")
for s in SECTIONS:
    print(f"{s:9} {total[s] / 2**20:9.1f} MiB")
print(f"{'non-content':9} {sum(v for k, v in total.items() if k != 'content') / 2**20:9.1f} MiB")
