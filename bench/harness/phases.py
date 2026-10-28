#!/usr/bin/env python3
"""Where a warm CLI query's time goes: wall time against scout's own `--stats` phases.

    phases.py <scout-exe> <index-dir> <queries.tsv> <runs> <out.json>

Every query runs once untimed, then <runs> times with `--stats --freshness off`, stdout to
/dev/null. Prints per query the median wall time and the median of every `*_ms` stat, plus
wall minus total_ms: the process start, catalog read and exit that no phase counts. Run it
inside ebs-run.sh so the box's limits apply.
"""
import json
import statistics
import subprocess
import sys
import time

exe, index, queries, runs, out = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
if runs < 1:
    raise SystemExit("runs must be at least 1")


def load(path):
    rows = []
    for line in open(path, encoding="utf8"):
        if line.startswith("#") or not line.strip():
            continue
        name, flags, pattern = line.rstrip("\n").split("\t")
        rows.append((name, flags.split(), pattern))
    return rows


def once(flags, pattern):
    cmd = [exe, "--index-dir", index, "search", "--freshness", "off", "--stats", *flags, "--", pattern]
    t = time.perf_counter()
    p = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
    wall = (time.perf_counter() - t) * 1000
    if p.returncode > 1:
        raise SystemExit(f"{cmd} exited {p.returncode}: {p.stderr.decode(errors='replace')}")
    line = next(l for l in p.stderr.decode().splitlines() if l.startswith("scout-stats"))
    stats = {}
    for field in line.split()[1:]:
        if "=" not in field or field.startswith("plan="):
            break
        k, v = field.split("=", 1)
        stats[k] = float(v)
    stats["wall_ms"] = wall
    return stats


report = {}
for name, flags, pattern in load(queries):
    once(flags, pattern)
    samples = [once(flags, pattern) for _ in range(runs)]
    med = {k: statistics.median(s[k] for s in samples) for k in samples[0]}
    med["outside_ms"] = med["wall_ms"] - med["total_ms"]
    report[name] = med
    keys = ["wall_ms", "outside_ms", "total_ms", "open_ms", "postings_ms", "verify_ms", "candidates"]
    print(name.ljust(18), " ".join(f"{k[:-3] if k.endswith('_ms') else k}={med.get(k, float('nan')):.1f}" for k in keys), flush=True)
json.dump(report, open(out, "w"), indent=1)
