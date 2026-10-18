#!/usr/bin/env python3
"""Checks that E24's native index (format v5) holds the same shards as the v3 one it is
compared with, apart from the trigram filter: same repos and shard split, and byte-identical
content, docs, paths, units, dictionary, fence and postings in every shard. Then probes every
v5 shard's filter with every one of its own dictionary keys through an independent copy of
the hash (shardfmt.py): a key the filter rejected would be matches lost. Prints totals.

    v3v5same.py <v3 index dir> <v5 index dir>
"""
import json
import os
import sys

import shardfmt


def shards_of(index):
    cat = json.load(open(os.path.join(index, "catalog.json")))
    return {r["name"]: r["index"]["shards"] for r in cat["repos"] if r.get("index")}


def main():
    v3_dir, v5_dir = sys.argv[1:]
    old, new = shards_of(v3_dir), shards_of(v5_dir)
    if old.keys() != new.keys():
        raise SystemExit(f"repos differ: {sorted(old.keys() ^ new.keys())}")
    shards = total_keys = filter_bytes = 0
    for repo in sorted(old):
        if len(old[repo]) != len(new[repo]):
            raise SystemExit(f"{repo}: {len(old[repo])} v3 shards, {len(new[repo])} v5")
        for a, b in zip(old[repo], new[repo]):
            ca, sa = shardfmt.read(os.path.join(v3_dir, "shards", a), shardfmt.V3)
            cb, sb = shardfmt.read(os.path.join(v5_dir, "shards", b), shardfmt.V5)
            if ca != cb:
                raise SystemExit(f"{repo}: counts {ca} vs {cb}")
            for name in shardfmt.V3[1]:
                if sa[name] != sb[name]:
                    raise SystemExit(f"{repo} {a} / {b}: section {name} differs")
            words = shardfmt.filter_words(sb)
            missed = sum(not shardfmt.may_contain(words, k) for k in shardfmt.keys(sb, cb[1]))
            if missed:
                raise SystemExit(f"{repo} {b}: the filter rejects {missed} of the shard's own keys")
            shards += 1
            total_keys += cb[1]
            filter_bytes += len(sb["filter"])
    print(f"same: {shards} shards in {len(old)} repos, {total_keys:,} keys, every key passes its filter, "
          f"filters {filter_bytes / 2**20:.2f} MiB ({8 * filter_bytes / total_keys:.2f} bits a key)")


main()
