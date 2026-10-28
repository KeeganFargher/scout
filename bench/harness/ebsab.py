"""Pools ab-ebs.sh's reps per arm: per mode and query, the median of every run's ms and of
disk MiB read, the ratio B / A, and how many reps B's p50 beat A's; then the geomean ratio.
Every run of a query must print the same bytes in both arms (sha256), or it fails.
Usage: ebsab.py <out-dir> <nameA> <nameB>"""
import glob, json, os, statistics, sys

if len(sys.argv) != 4:
    sys.exit(__doc__)
out, a, b = sys.argv[1:]


def reports(arm, mode):
    paths = sorted(glob.glob(os.path.join(out, f"*-{arm}", f"{mode}-*.json")))
    return [json.load(open(p)) for p in paths]


for mode in ("warm", "cold"):
    ra, rb = reports(a, mode), reports(b, mode)
    if not ra and not rb:
        continue
    if len(ra) != len(rb):
        sys.exit(f"{mode}: {len(ra)} reps of {a}, {len(rb)} of {b}")
    print(f"## {mode}, {len(ra)} reps")
    print(f"| query | {a} ms | {b} ms | {b}/{a} | {a} MiB read | {b} MiB read | {b} faster in reps |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    ratios = []
    for qa in ra[0]["queries"]:
        name = qa["name"]
        per = lambda rs: [next(q for q in r["queries"] if q["name"] == name) for r in rs]
        pa, pb = per(ra), per(rb)
        shas = {q["sha256"] for q in pa + pb}
        if len(shas) != 1:
            sys.exit(f"{mode} {name}: output differs between arms or reps")
        ms = lambda ps: statistics.median(x["ms"] for p in ps for x in p["runs"])
        mib = lambda ps: statistics.median(x["read_mib"] for p in ps for x in p["runs"])
        ma, mb = ms(pa), ms(pb)
        wins = sum(x["p50_ms"] < y["p50_ms"] for x, y in zip(pb, pa))
        ratios.append(mb / ma)
        print(f"| {name} | {ma:.0f} | {mb:.0f} | {mb / ma:.3f} | {mib(pa):.1f} | {mib(pb):.1f} | {wins}/{len(pa)} |")
    print(f"geomean {b}/{a}: {statistics.geometric_mean(ratios):.3f}\n")
