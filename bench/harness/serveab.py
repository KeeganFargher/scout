#!/usr/bin/env python3
"""Pools serve-ab.sh's reps: per query and mode, the median over reps of each arm's p50,
the ratio B/A, MiB read, and in how many reps B was faster; geomeans; and each arm's time
to ready. Fails if the arms' outputs differ (sha256) for any query.

    serveab.py <out-dir> <nameA> <nameB>
"""
import glob
import json
import math
import os
import statistics
import sys

out, a, b = sys.argv[1], sys.argv[2], sys.argv[3]


def reps(arm):
    dirs = sorted(glob.glob(os.path.join(out, f"*-{arm}")))
    if not dirs:
        sys.exit(f"no runs of {arm} in {out}")
    return dirs


for mode in ("warm", "cold"):
    runs = {arm: [json.load(open(os.path.join(d, f"{mode}.json"))) for d in reps(arm)] for arm in (a, b)}
    if len(runs[a]) != len(runs[b]):
        sys.exit("arms have different rep counts")
    names = [q["name"] for q in runs[a][0]["queries"]]
    print(f"## {mode}, {len(runs[a])} reps")
    print(f"| query | {a} ms | {b} ms | {b}/{a} | {a} MiB read | {b} MiB read | {b} faster in reps |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    logs = []
    for i, name in enumerate(names):
        rows = {arm: [r["queries"][i] for r in runs[arm]] for arm in (a, b)}
        hashes = {q["sha256"] for arm in (a, b) for q in rows[arm]}
        if len(hashes) != 1:
            sys.exit(f"{mode} {name}: output differs between arms or reps")
        pa = statistics.median(q["p50_ms"] for q in rows[a])
        pb = statistics.median(q["p50_ms"] for q in rows[b])
        ma = statistics.median(q["read_mib_median"] for q in rows[a])
        mb = statistics.median(q["read_mib_median"] for q in rows[b])
        wins = sum(qb["p50_ms"] < qa["p50_ms"] for qa, qb in zip(rows[a], rows[b]))
        logs.append(math.log(pb / pa))
        print(f"| {name} | {pa:.1f} | {pb:.1f} | {pb / pa:.3f} | {ma:.1f} | {mb:.1f} | {wins}/{len(rows[a])} |")
    print(f"geomean {b}/{a}: {math.exp(sum(logs) / len(logs)):.3f}\n")

for arm in (a, b):
    # ready.txt is key=value pairs; ready_ms is the first (E59 added the volume's caps).
    ready = [int(dict(kv.split("=", 1) for kv in open(os.path.join(d, "ready.txt")).read().split())["ready_ms"]) for d in reps(arm)]
    print(f"{arm} time to ready on an evicted index: {ready} ms")
