#!/usr/bin/env python3
"""Summarises serve-coldab.sh: per profile and mode, B/A geomean of per-query p50 (each
query's p50 pooled over reps), per-query ratios with major faults and MiB read from disk,
and parity (every result hash equal across arms, reps and modes). Every run in the
directory must have had the same read-ahead (qbench.py records it), or the comparison fails.

    coldabsum.py <dir> <armA> <armB>
"""
import glob
import json
import os
import re
import statistics
import sys

d, A, B = sys.argv[1:]
runs = {}  # (arm, cpu, mode) -> {query: [ms...]}, plus faults and MiB read
faults = {}
reads = {}
hashes = {}
read_ahead = {}  # read_ahead_kb -> files
for p in glob.glob(os.path.join(d, "*.json")):
    m = re.match(r"(cold|warm)-(.+)-(\d+)cpu-(\d+)\.json$", os.path.basename(p))
    if not m:
        continue
    mode, arm, cpu, _ = m.groups()
    rep = json.load(open(p))
    read_ahead.setdefault(rep["read_ahead_kb"], []).append(os.path.basename(p))
    for q in rep["queries"]:
        k = (arm, cpu, mode)
        runs.setdefault(k, {}).setdefault(q["name"], []).extend(x["ms"] for x in q["runs"])
        faults.setdefault(k, {}).setdefault(q["name"], []).extend(x["majfaults"] for x in q["runs"])
        reads.setdefault(k, {}).setdefault(q["name"], []).extend(x["read_mib"] for x in q["runs"])
        h = hashes.setdefault(q["name"], q["sha256"])
        if h != q["sha256"]:
            raise SystemExit(f"PARITY FAIL: {q['name']} differs in {p}")

if len(read_ahead) != 1:
    raise SystemExit(f"runs with different read-ahead cannot be compared: {read_ahead}")
print(f"read_ahead_kb={next(iter(read_ahead))}\n")
cpus = sorted({k[1] for k in runs}, key=int)
for cpu in cpus:
    for mode in ("warm", "cold"):
        ra, rb = runs.get((A, cpu, mode)), runs.get((B, cpu, mode))
        if not ra or not rb:
            continue
        print(f"## {cpu} CPU {mode}: p50 ms {A} -> {B} (median major faults; median MiB read)")
        ratios = []
        for q in ra:
            pa, pb = statistics.median(ra[q]), statistics.median(rb[q])
            fa, fb = statistics.median(faults[(A, cpu, mode)][q]), statistics.median(faults[(B, cpu, mode)][q])
            ma, mb = statistics.median(reads[(A, cpu, mode)][q]), statistics.median(reads[(B, cpu, mode)][q])
            ratios.append(pb / pa)
            print(f"{q:28s} {pa:9.1f} -> {pb:9.1f}  x{pb / pa:.3f}  ({fa:.0f} -> {fb:.0f}; {ma:.1f} -> {mb:.1f})")
        ga = statistics.geometric_mean(statistics.median(v) for v in ra.values())
        gb = statistics.geometric_mean(statistics.median(v) for v in rb.values())
        print(f"geomean {ga:.1f} -> {gb:.1f} ms, ratio {statistics.geometric_mean(ratios):.3f}\n")
print("parity ALL OK")
