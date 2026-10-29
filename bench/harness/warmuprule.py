#!/usr/bin/env python3
"""E49's keep rule, set before its runs: the warm-up must stay inside the harness's own noise.
An A/A run (warmup-ab.sh with --warm-mb 0, both arms off) measures that noise: for each
scenario, the largest per-rep deviation from 1 of the pass geomean and of the steady warm
geomean. The A/B run's pooled ratios (median over reps per query, then geomean) must be at
most 1 + that deviation: the pass for scenarios at ready (wait 0), steady warm for all.
The clauses that are not ratios against noise (45 s pass <= 0.50, ready within 5%, no OOM
kill, >= 95% resident) stay in warmupab.py's checks, which this does not repeat.

    warmuprule.py <aa-dir> <ab-dir> <reps> <scenario>...
"""
import json
import math
import os
import statistics
import sys

aa_dir, ab_dir, reps, scenarios = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4:]


def geomean(xs):
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def load(d):
    lines = open(os.path.join(d, "pass.txt")).read().split("\n")
    if lines[0] != "query us rmib":
        sys.exit(f"{d}: unexpected pass.txt header")
    passed = {f[0]: int(f[1]) for f in (l.split() for l in lines[1:] if l)}
    warm = {q["name"]: q["p50_ms"] for q in json.load(open(os.path.join(d, "warm.json")))["queries"]}
    return passed, warm


def ratios(base, scenario):
    wait, gap = scenario.split(":")
    runs = {
        arm: [load(os.path.join(base, f"wait{wait}-gap{gap}", f"{r}-{arm}")) for r in range(1, reps + 1)]
        for arm in ("off", "on")
    }
    out = {}
    for i, metric in enumerate(("pass", "steady")):
        names = list(runs["off"][0][i])
        per_rep = [geomean([on[i][n] / off[i][n] for n in names]) for off, on in zip(runs["off"], runs["on"])]
        med = {arm: {n: statistics.median(r[i][n] for r in runs[arm]) for n in names} for arm in runs}
        out[metric] = (per_rep, geomean([med["on"][n] / med["off"][n] for n in names]))
    return out


ok = True
for scenario in scenarios:
    aa, ab = ratios(aa_dir, scenario), ratios(ab_dir, scenario)
    metrics = ("pass", "steady") if scenario.startswith("0:") else ("steady",)
    for metric in metrics:
        noise = max(abs(g - 1) for g in aa[metric][0])
        pooled = ab[metric][1]
        passed = pooled <= 1 + noise
        ok &= passed
        print(
            f"{scenario} {metric}: A/B pooled {pooled:.3f} (per rep {', '.join(f'{g:.3f}' for g in ab[metric][0])})"
            f" <= 1 + A/A noise {noise:.3f} (A/A per rep {', '.join(f'{g:.3f}' for g in aa[metric][0])},"
            f" pooled {aa[metric][1]:.3f}): {'ok' if passed else 'FAIL'}"
        )
print("rule:", "ok" if ok else "FAIL")
