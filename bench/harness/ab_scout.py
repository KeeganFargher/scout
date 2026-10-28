"""Interleaved wall-time A/B of scout builds, scout alone (no rg), on one index.

Each round runs every query on every arm, `runs` times each after one warm-up, and
alternates which arm goes first. Prints per query the median of all runs per arm and their
ratio (B / A), and how many rounds B's median beat A's.
Usage: ab_scout.py <index> <tsv> <rounds> <runs> <nameA=scout-exe> <nameB=scout-exe>"""
import statistics, subprocess, sys, time

if len(sys.argv) != 7:
    sys.exit(__doc__)
index, tsv, rounds, runs = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
arms = [a.split("=", 1) for a in sys.argv[5:7]]
queries = []
for line in open(tsv, encoding="utf-8"):
    line = line.rstrip("\n")
    if line and not line.startswith("#"):
        name, flags, pattern = line.split("\t")
        queries.append((name, flags.split(), pattern))

times = {}  # (query, arm) -> [ms]
wins = {}  # query -> rounds B's median beat A's
for r in range(rounds):
    order = arms if r % 2 == 0 else arms[::-1]
    for name, flags, pattern in queries:
        per_round = {}
        for arm, exe in order:
            cmd = [exe, "--index-dir", index, "search", "-n", *flags, "--", pattern]
            subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            ts = []
            for _ in range(runs):
                t = time.perf_counter()
                # Output is read through a pipe, as scout-bench and real callers read it.
                p = subprocess.run(cmd, capture_output=True, check=False)
                ts.append((time.perf_counter() - t) * 1000)
                if p.returncode not in (0, 1):
                    sys.exit(f"{arm} {name}: exit {p.returncode}: {p.stderr.decode(errors='replace')[:300]}")
            per_round[arm] = statistics.median(ts)
            times.setdefault((name, arm), []).extend(ts)
        wins[name] = wins.get(name, 0) + (per_round[arms[1][0]] < per_round[arms[0][0]])
    print(f"round {r + 1}/{rounds} done", file=sys.stderr, flush=True)

a, b = arms[0][0], arms[1][0]
print(f"| query | {a} ms | {b} ms | {b}/{a} | {b} faster in rounds |")
print("|---|---:|---:|---:|---:|")
for name, _, _ in queries:
    ma, mb = statistics.median(times[(name, a)]), statistics.median(times[(name, b)])
    print(f"| {name} | {ma:.1f} | {mb:.1f} | {mb / ma:.3f} | {wins[name]}/{rounds} |", flush=True)
