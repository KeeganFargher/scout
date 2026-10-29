#!/usr/bin/env python3
"""Pools warmup-ab.sh's reps (E45–E47): for each scenario (<wait-secs>:<gap-ms>), the pass's
per-query time (median over reps of each arm), on/off ratio, MiB read (the whole cgroup's,
the warm-up's reads included) and in how many reps "on" was faster; the pass geomean per rep
and pooled; the steady-state warm geomean (qbench p50s); time to ready; the index's
residency, the cgroup's peak memory and OOM kills; and the warm-up's state before the pass.
Then the clauses of the keep rule that are not held to the harness's noise: ready time
within 5% (median over reps) and no OOM kill; and in an A/B, after a wait, the pass <= 0.50
pooled and at least 95% of the index resident in "on". The ratios at ready and steady warm
are held to an A/A run's noise by warmuprule.py (E49); E45–E48 held them to fixed
thresholds (<= 1.00 in every rep, <= 1.02), which the A/A showed sit inside the noise.
Fails on a missing file, or on a warm-up state that contradicts its arm.

    warmupab.py [--aa] <out-dir> <reps> <scenario>...

--aa: an A/A run (E49), both arms `--warm-mb 0`: the same tables and clauses measure the
harness's own noise, so each arm's warm-up state must be off.
"""
import json
import math
import os
import statistics
import sys

args = sys.argv[1:]
aa = args[:1] == ["--aa"]
args = args[1:] if aa else args
out, reps, scenarios = args[0], int(args[1]), args[2:]
ARMS = ("off", "on")


def geomean(xs):
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def kv(path):
    return dict(f.split("=", 1) for f in open(path).read().split() if "=" in f)


def run(d, arm):
    lines = open(os.path.join(d, "pass.txt")).read().split("\n")
    if lines[0] != "query us rmib":
        sys.exit(f"{d}: unexpected pass.txt header")
    passed = {f[0]: (int(f[1]) / 1000, int(f[2])) for f in (l.split() for l in lines[1:] if l)}
    warm = json.load(open(os.path.join(d, "warm.json")))
    status = json.load(open(os.path.join(d, "status-before-pass.json")))
    if (status["warm"] is None) != (aa or arm == "off"):
        sys.exit(f"{d}: warm-up state {status['warm']} contradicts arm {arm}")
    return {
        "pass": passed,
        "warm": {q["name"]: q["p50_ms"] for q in warm["queries"]},
        "ready": int(kv(os.path.join(d, "ready.txt"))["ready_ms"]),
        "res": {k: float(v) for k, v in kv(os.path.join(d, "residency.txt")).items()},
        "cg": kv(os.path.join(d, "cgroup.txt")),
        "warmup": status["warm"],
    }


checks = []
for scenario in scenarios:
    wait, gap = scenario.split(":")
    base = os.path.join(out, f"wait{wait}-gap{gap}")
    runs = {arm: [run(os.path.join(base, f"{r}-{arm}"), arm) for r in range(1, reps + 1)] for arm in ARMS}
    names = list(runs["off"][0]["pass"])
    print(f"## pass {wait} s after ready, queries {gap} ms apart, {reps} reps")
    print("| query | off ms | on ms | on/off | off MiB read | on MiB read | on faster in reps |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    for name in names:
        ms = {arm: statistics.median(r["pass"][name][0] for r in runs[arm]) for arm in ARMS}
        mib = {arm: statistics.median(r["pass"][name][1] for r in runs[arm]) for arm in ARMS}
        wins = sum(on["pass"][name][0] < off["pass"][name][0] for off, on in zip(runs["off"], runs["on"]))
        print(
            f"| {name} | {ms['off']:.1f} | {ms['on']:.1f} | {ms['on'] / ms['off']:.3f} "
            f"| {mib['off']:.0f} | {mib['on']:.0f} | {wins}/{reps} |"
        )
    per_rep = [geomean([on["pass"][n][0] / off["pass"][n][0] for n in names]) for off, on in zip(runs["off"], runs["on"])]
    med = {arm: {n: statistics.median(r["pass"][n][0] for r in runs[arm]) for n in names} for arm in ARMS}
    pooled = geomean([med["on"][n] / med["off"][n] for n in names])
    print(f"pass geomean on/off: {pooled:.3f} (per rep: {', '.join(f'{g:.3f}' for g in per_rep)})")
    print(f"pass total ms: off {[round(sum(v[0] for v in r['pass'].values())) for r in runs['off']]}, "
          f"on {[round(sum(v[0] for v in r['pass'].values())) for r in runs['on']]}")
    warm = geomean(
        [
            statistics.median(r["warm"][n] for r in runs["on"]) / statistics.median(r["warm"][n] for r in runs["off"])
            for n in names
        ]
    )
    print(f"steady warm geomean on/off (qbench p50s after the pass): {warm:.3f}")
    if wait != "0" and not aa:
        checks.append((scenario, "pass <= 0.50", pooled <= 0.50, f"{pooled:.3f}"))
    for arm in ARMS:
        rs = runs[arm]
        print(
            f"{arm}: ready_ms {[r['ready'] for r in rs]}, resident MiB {[round(r['res']['index_resident_mib']) for r in rs]}"
            f" of {round(rs[0]['res']['index_mib'])}, peak MiB {[r['cg']['cgroup_peak_mib'] for r in rs]},"
            f" oom_kills {[r['cg']['oom_kills'] for r in rs]}, warm-up before the pass {[r['warmup'] for r in rs]}"
        )
    ready = {arm: statistics.median(r["ready"] for r in runs[arm]) for arm in ARMS}
    checks.append((scenario, "ready within 5%", abs(ready["on"] / ready["off"] - 1) <= 0.05, f"{ready['on'] / ready['off']:.3f}"))
    ooms = sum(int(r["cg"]["oom_kills"]) for arm in ARMS for r in runs[arm])
    checks.append((scenario, "no OOM kill", ooms == 0, str(ooms)))
    if wait != "0" and not aa:
        least = min(r["res"]["index_resident_mib"] / r["res"]["index_mib"] for r in runs["on"])
        checks.append((scenario, "on: >= 95% of the index resident", least >= 0.95, f"{least:.3f}"))
    print()

print("## checks")
for scenario, what, ok, value in checks:
    print(f"{scenario} {what}: {'ok' if ok else 'FAIL'} ({value})")
