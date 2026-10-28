"""One table per qbench.py report: per query p50 / p90 ms, disk MiB read and major faults
(medians), and the geomean p50. Usage: ebssum.py <report.json>..."""
import json, os, sys

if len(sys.argv) < 2:
    sys.exit(__doc__)
for path in sys.argv[1:]:
    r = json.load(open(path))
    print(f"## {os.path.basename(path)}: {r['mode']}, {r['rounds']} rounds, read_ahead_kb {r['read_ahead_kb']}")
    print("| query | p50 ms | p90 ms | read MiB | major faults |")
    print("|---|---:|---:|---:|---:|")
    for q in r["queries"]:
        print(f"| {q['name']} | {q['p50_ms']:.0f} | {q['p90_ms']:.0f} | {q['read_mib_median']:.1f} | {q['majfaults_median']:.0f} |")
    under = sum(q["p50_ms"] < 1000 for q in r["queries"])
    print(f"geomean p50 {r['geomean_p50_ms']:.0f} ms; {under} of {len(r['queries'])} under 1 s\n")
