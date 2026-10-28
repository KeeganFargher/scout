"""Scout-only timings: 1 warm-up, then N runs per query, each a fresh process, as scout-bench
times them. Usage: scout_only.py <scout> <index> <tsv> <runs>"""
import re, statistics, subprocess, sys, time

scout, idx, tsv, runs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
print("| query | median ms | p90 | max | output lines | candidates | matched files | in-process total ms |")
print("|---|---:|---:|---:|---:|---:|---:|---:|")
for line in open(tsv, encoding="utf-8"):
    line = line.rstrip("\n")
    if not line or line.startswith("#"):
        continue
    name, flags, pattern = line.split("\t")
    cmd = [scout, "--index-dir", idx, "search", "-n", "--stats", *flags.split(), "--", pattern]
    subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    times = []
    for _ in range(runs):
        t = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, check=False)
        times.append((time.perf_counter() - t) * 1000)
        # rg exits 1 for "no match"; scout follows it. Anything else is a failed search.
        if p.returncode not in (0, 1):
            sys.exit(f"{name}: exit {p.returncode}: {p.stderr.decode(errors='replace')[:300]}")
    stats = dict(re.findall(r"(\w+)=([\d.]+)", p.stderr.decode(errors="replace")))
    lines = p.stdout.count(b"\n")
    times.sort()
    print(f"| {name} | {statistics.median(times):.1f} | {times[int(len(times) * 0.9) - 1]:.1f} | {times[-1]:.1f}"
          f" | {lines} |{stats.get('candidates', '?')} | {stats.get('matched_files', '?')}"
          f" | {stats.get('total_ms', '?')} |", flush=True)
