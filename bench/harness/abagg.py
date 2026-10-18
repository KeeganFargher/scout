#!/usr/bin/env python3
"""Aggregates ab.sh rounds: per profile and query, the median of per-round medians for each
arm, the ratio b/a, and in how many rounds b was faster (rounds are paired).

    abagg.py <dir> <armA> <armB>
"""
import json
import math
import os
import re
import statistics
import sys

d, a, b = sys.argv[1:4]
by = {}  # profile -> query -> arm -> {round: median}
parity = {}
for f in sorted(os.listdir(d)):
    m = re.fullmatch(r"(.+)-(\d+cpu)-(\d+)\.json", f)
    if not m:
        continue
    arm, prof, rnd = m.group(1), m.group(2), int(m.group(3))
    j = json.load(open(os.path.join(d, f)))
    parity[(arm, prof)] = parity.get((arm, prof), True) and j["all_parity"]
    for q in j["queries"]:
        by.setdefault(prof, {}).setdefault(q["name"], {}).setdefault(arm, {})[rnd] = q["scout_median_ms"]

for prof in sorted(by, key=lambda p: int(p[:-3])):
    print(f"\n## {prof}: query | {a} | {b} | {b}/{a} | rounds {b} faster")
    logs = []
    for name, arms in by[prof].items():
        rounds = sorted(set(arms[a]) & set(arms[b]))
        x = statistics.median(arms[a][r] for r in rounds)
        y = statistics.median(arms[b][r] for r in rounds)
        wins = sum(arms[b][r] < arms[a][r] for r in rounds)
        logs.append(math.log(y / x))
        print(f"{name} | {x:.1f} | {y:.1f} | {y / x:.3f} | {wins}/{len(rounds)}")
    print(f"geomean {b}/{a}: {math.exp(sum(logs) / len(logs)):.3f}")
print("\nparity", {f"{k[0]}-{k[1]}": v for k, v in sorted(parity.items())})
