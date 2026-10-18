#!/usr/bin/env python3
"""Summarises E21: CLI vs serve, warm vs cold, per profile; parity of serve against the CLI;
and the 1-CPU memory cliff.  e21sum.py <results/e21>"""
import glob
import json
import os
import sys

d = sys.argv[1]
load = lambda f: json.load(open(f))
ok = True

print("## geomean p50 ms (20 queries)")
print("profile | CLI warm | CLI cold | serve warm | serve cold")
profiles = [p for p in ("1cpu", "4cpu", "12cpu") if os.path.exists(f"{d}/serve-warm-{p}.json")]
for p in profiles:
    r = {k: load(f"{d}/{k}-{p}.json") for k in ("cli-warm", "cli-cold", "serve-warm", "serve-cold")}
    print(p, "|", " | ".join(f"{r[k]['geomean_p50_ms']:.1f}" for k in r))
    # Parity: serve must return exactly what the CLI prints, on every query.
    cli = {q["name"]: q["sha256"] for q in r["cli-warm"]["queries"]}
    for k in ("cli-cold", "serve-warm", "serve-cold"):
        for q in r[k]["queries"]:
            if q["sha256"] != cli[q["name"]]:
                ok = False
                print(f"PARITY FAIL {p} {k} {q['name']}")

for p in profiles:
    print(f"\n## {p} per query: p50 ms (median major faults)")
    print("query | CLI warm | CLI cold | serve warm | serve cold")
    r = {k: {q["name"]: q for q in load(f"{d}/{k}-{p}.json")["queries"]} for k in ("cli-warm", "cli-cold", "serve-warm", "serve-cold")}
    for name in r["cli-warm"]:
        cells = [f"{r[k][name]['p50_ms']:.1f} ({r[k][name]['majfaults_median']:.0f})" for k in r]
        print(name, "|", " | ".join(cells))

cliff = sorted(glob.glob(f"{d}/serve-warm-cliff-1cpu-*.json"), key=os.path.getmtime)
if cliff:
    print("\n## memory cliff, serve at 1 CPU, warm steady state")
    print("limit | geomean p50 ms | worst query p90 ms | median major faults/query | page cache MiB | events")
    for f in cliff:
        m = f.split("cliff-1cpu-")[1][:-5]
        r = load(f)
        qs = r["queries"]
        worst = max(qs, key=lambda q: q["p90_ms"])
        majf = sorted(q["majfaults_median"] for q in qs)[len(qs) // 2]
        cache = sorted(q["file_mib_before_median"] for q in qs)[len(qs) // 2]
        ev = open(f"{d}/events-cliff-1cpu-{m}.txt").read().strip() if os.path.exists(f"{d}/events-cliff-1cpu-{m}.txt") else ""
        print(f"{m} | {r['geomean_p50_ms']:.1f} | {worst['p90_ms']:.0f} ({worst['name']}) | {majf:.0f} | {cache:.0f} | {ev}")
    for m in ("2G", "1536M", "1280M", "1G", "768M", "512M"):
        if not os.path.exists(f"{d}/serve-warm-cliff-1cpu-{m}.json"):
            print(f"{m} | run failed or not run")

print("\nparity", "ALL OK" if ok else "FAILURES")
sys.exit(0 if ok else 1)
