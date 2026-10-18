#!/usr/bin/env python3
"""Checks E24's trigram-filter gate end to end on a real v5 native index. For each literal
(-F) bench query it probes every shard's on-disk filter with an independent copy of the hash
(shardfmt.py) and counts the shards the filters let through, and the shards that hold every
trigram (exact). `scout search --stats` must report shards_filtered = shards - let through,
and no shard holding every trigram may be rejected. Other queries' counts are printed,
unchecked (their planned trees are not a plain AND of the pattern's trigrams).

    filtercheck.py <scout> <index dir> <queries.tsv>
"""
import json
import os
import subprocess
import sys

import shardfmt


def scout_stats(scout, index, flags, pattern):
    p = subprocess.run([scout, "--index-dir", index, "search", "--stats", "--freshness", "off", *flags, "--", pattern],
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if p.returncode not in (0, 1):
        raise SystemExit(f"scout exited {p.returncode} for {pattern!r}: {p.stderr[:300]}")
    line = next(l for l in p.stderr.splitlines() if "shards_filtered=" in l)
    return {k: int(v) for k, v in (w.split("=", 1) for w in line.split() if "=" in w) if v.isdigit()}


def main():
    scout, index, qpath = sys.argv[1:]
    cat = json.load(open(os.path.join(index, "catalog.json")))
    shards = []
    for r in cat["repos"]:
        for s in (r["index"]["shards"] if r.get("index") else []):
            counts, sections = shardfmt.read(os.path.join(index, "shards", s), shardfmt.V5)
            shards.append((set(shardfmt.keys(sections, counts[1])), shardfmt.filter_words(sections)))
    print(f"{len(shards)} shards\n{'query':28s} {'exact':>5s} {'filter':>6s} {'want':>5s} {'scout':>5s}")
    bad = 0
    for line in open(qpath, encoding="utf8"):
        if line.startswith("#") or not line.strip():
            continue
        name, flags, pattern = line.rstrip("\n").split("\t")
        flags = flags.split()
        stats = scout_stats(scout, index, flags, pattern)
        if stats["shards"] != len(shards):
            raise SystemExit(f"{name}: scout searched {stats['shards']} shards, the catalog has {len(shards)}")
        if "-F" not in flags:
            print(f"{name:28s} {'':5s} {'':6s} {'':5s} {stats['shards_filtered']:5d}")
            continue
        ts = shardfmt.trigrams(pattern)
        exact = [all(t in keys for t in ts) for keys, _ in shards]
        passed = [all(shardfmt.may_contain(words, t) for t in ts) for _, words in shards]
        missed = sum(e and not p for e, p in zip(exact, passed))
        want = len(shards) - sum(passed)
        ok = missed == 0 and stats["shards_filtered"] == want
        bad += not ok
        print(f"{name:28s} {sum(exact):5d} {sum(passed):6d} {want:5d} {stats['shards_filtered']:5d}"
              + ("" if ok else f"  MISMATCH ({missed} shards holding every trigram rejected)"))
    if bad:
        raise SystemExit(f"{bad} literal queries disagree")
    print("filter gate OK")


main()
