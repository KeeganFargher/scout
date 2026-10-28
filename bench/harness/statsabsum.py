#!/usr/bin/env python3
"""Summarises stats-ab.sh: per profile and query, the candidates each arm verified, the
median of each `--stats` timing and of the process's wall time, and, for the search's total
and the wall time, B/A, the interquartile ranges and in how many pairs (one run of each arm,
back to back) B was lower.

    statsabsum.py <dir> <armA> <armB>
"""
import os
import re
import statistics
import sys

TIMINGS = ("open_ms", "postings_ms", "verify_ms", "search_ms", "total_ms", "wall_ms")

if len(sys.argv) != 4:
    sys.exit(__doc__)
d, a, b = sys.argv[1:4]
by = {}  # profile -> query -> arm -> [stats per run, in run order]
for f in sorted(os.listdir(d)):
    m = re.fullmatch(r"(.+)-(\d+cpu)\.txt", f)
    if not m:
        continue
    runs = {a: [], b: []}
    for line in open(os.path.join(d, f)):
        arm, rest = line.split(" ", 1)
        if arm not in runs:
            sys.exit(f"{f}: a run from arm {arm!r}, neither {a!r} nor {b!r}")
        # The plan comes last and is free text, so it is cut off before reading numbers.
        fields = rest.split(" plan=", 1)[0]
        run = {k: float(v) for k, v in re.findall(r"(\w+)=([\d.]+)", fields)}
        run["wall_ms"] = run.pop("wall_us") / 1000
        runs[arm].append(run)
    if len(runs[a]) != len(runs[b]) or len(runs[a]) < 2:
        sys.exit(f"{f}: {len(runs[a])} {a} runs and {len(runs[b])} {b} runs; want equal, at least 2")
    by.setdefault(m.group(2), {})[m.group(1)] = runs
if not by:
    sys.exit(f"no runs in {d}")

print(
    "medians over the runs; postings_ms and verify_ms sum every thread's time, the rest are"
    " wall time; wall_ms is the whole process with its output piped, total_ms the search in it"
)


def compare(runs, t):
    """B/A of the medians, both interquartile ranges, and the pairs in which B was lower."""
    med = {arm: statistics.median(r[t] for r in runs[arm]) for arm in (a, b)}
    iqr = {arm: statistics.quantiles([r[t] for r in runs[arm]], n=4) for arm in (a, b)}
    lower = sum(y[t] < x[t] for x, y in zip(runs[a], runs[b]))
    return (
        f"{med[b] / med[a]:.3f} | {iqr[a][0]:.1f}-{iqr[a][2]:.1f}; {iqr[b][0]:.1f}-{iqr[b][2]:.1f}"
        f" | {lower}/{len(runs[a])}"
    )


for prof in sorted(by, key=lambda p: int(p[:-3])):
    print(
        f"\n## {prof}: query | candidates {a} / {b} | "
        + " | ".join(f"{t} {a} -> {b}" for t in TIMINGS)
        + "".join(f" | {t} {b}/{a} | {t} p25-p75 {a}; {b} | pairs {b} lower" for t in ("total_ms", "wall_ms"))
    )
    for name, runs in sorted(by[prof].items()):
        cand = {arm: sorted({int(r["candidates"]) for r in runs[arm]}) for arm in (a, b)}
        med = {arm: {t: statistics.median(r[t] for r in runs[arm]) for t in TIMINGS} for arm in (a, b)}
        print(
            f"{name} | {','.join(map(str, cand[a]))} / {','.join(map(str, cand[b]))} | "
            + " | ".join(f"{med[a][t]:.2f} -> {med[b][t]:.2f}" for t in TIMINGS)
            + "".join(f" | {compare(runs, t)}" for t in ("total_ms", "wall_ms"))
        )
