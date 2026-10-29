"""Per rep of a serve-ab.sh out dir: the cold geomean B/A; then the queries over 10% slower
in every rep (the cold rule's per-query clause since E52) and every query over 1.05 in any
rep. serveab.py pools the reps; the rule's clauses are per rep (E59).
Usage: serveab-reps.py <out-dir> <nameA> <nameB>"""
import json
import math
import os
import sys

out, a, b = sys.argv[1], sys.argv[2], sys.argv[3]
reps = sorted({d.split("-")[0] for d in os.listdir(out) if "-" in d and d.split("-")[0].isdigit()}, key=int)
per = {}
for r in reps:
    ra = {q["name"]: q["p50_ms"] for q in json.load(open(os.path.join(out, f"{r}-{a}", "cold.json")))["queries"]}
    rb = {q["name"]: q["p50_ms"] for q in json.load(open(os.path.join(out, f"{r}-{b}", "cold.json")))["queries"]}
    ratios = {n: rb[n] / ra[n] for n in ra}
    g = math.exp(sum(math.log(x) for x in ratios.values()) / len(ratios))
    print(f"rep {r}: cold geomean {g:.3f}")
    for n, x in ratios.items():
        per.setdefault(n, []).append(x)
bad = [n for n, xs in per.items() if all(x > 1.10 for x in xs)]
print("over 10% slower in every rep:", bad or "none")
for n, xs in per.items():
    if max(xs) > 1.05:
        print(f"  {n}: {' '.join(f'{x:.3f}' for x in xs)}")
