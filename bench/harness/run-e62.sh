#!/bin/bash
# E62 (root): an AND prices every part, an OR or nested AND as well as a trigram, and decodes
# cheapest first under E22's budget, instead of decoding every non-trigram part whole first.
# No query of queries-big.tsv or queries-search-ms.tsv has such an AND; queries-andor.tsv is
# the held-out set that does. Rules, committed with this script before it runs; every stage
# runs whatever an earlier one gave, and verdict.txt says which pass:
# 1. Held-out, warm: search-ms.sh on queries-andor.tsv at 2 CPUs, geomean <= 0.95 in both
#    passes (search-ms.sh also stops if the two binaries' output differs, on every query).
# 2. Everything else, warm: instructions retired (`perf stat -e instructions:u`, 9 runs,
#    interleaved twice, 1 CPU) on every query of queries-search-ms.tsv within +0.1% in both
#    passes (E61's bound): there, only the order of equal-count trigrams can change.
# 3. Cold, on a t3.medium's caps (both arms told them) and then on gp3's: serve-ab.sh over
#    queries-big.tsv's 18 served queries plus the held-out set, 2 reps. The 18: geomean of
#    per-query ratios of the reps' mean p50 <= 1.01, and none over 10% slower in both reps.
#    The held-out: the same geomean <= 0.97.
# Usage: run-e62.sh <before-bin-dir> <e62-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected before-bin-dir e62-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
shopt -s nullglob
perfs=(/usr/lib/linux-tools/*/perf)
[ "${#perfs[@]}" -gt 0 ] || { echo "no perf under /usr/lib/linux-tools" >&2; exit 2; }
PERF=${perfs[0]}

bash "$P/search-ms.sh" "$A" "$B" "$I" "$P/../queries-andor.tsv" 2 > "$OUT/search-ms-andor-2.txt"
awk '{ a += log($3 / $2); b += log($5 / $4); n++ } END {
  g1 = exp(a / n); g2 = exp(b / n)
  printf "1 held-out warm: geomean %.3f %.3f: %s\n", g1, g2, (g1 <= 0.95 && g2 <= 0.95) ? "pass" : "fail"
}' "$OUT/search-ms-andor-2.txt" | tee "$OUT/verdict.txt"

grep -v -e '^#' -e '^$' "$P/../queries-search-ms.tsv" | while IFS= read -r line; do
  name=${line%%$'\t'*} rest=${line#*$'\t'}
  flags=${rest%%$'\t'*} pat=${rest#*$'\t'}
  for b in "$A" "$B" "$A" "$B"; do
    # Warm-up; a search that matches nothing exits 1, as rg does, so 1 is allowed and any
    # other failure stops the run.
    rc=0
    # shellcheck disable=SC2086
    taskset -c 0 "$b/scout" --index-dir "$I" search --freshness off --threads 1 $flags -- "$pat" < /dev/null > /dev/null || rc=$?
    [ "$rc" -le 1 ] || { echo "$name: warm-up exited $rc" >&2; exit 1; }
    # shellcheck disable=SC2086
    n=$(taskset -c 0 "$PERF" stat -x, -r 9 -e instructions:u "$b/scout" --index-dir "$I" search --freshness off --threads 1 \
      $flags -- "$pat" < /dev/null 2>&1 > /dev/null | awk -F, '$3 == "instructions:u" { print $1 }')
    # A "<not counted>" or empty field is a failed measurement, not a count.
    [[ "$n" =~ ^[0-9]+$ ]] || { echo "$name: no instruction count from perf: '$n'" >&2; exit 1; }
    echo "$name $(basename "$b") $n"
  done
done > "$OUT/instructions.txt"
awk '{ v[$1, ++k[$1]] = $3 } END {
  for (q in k) printf "%s %.5f %.5f\n", q, v[q, 2] / v[q, 1], v[q, 4] / v[q, 3]
}' "$OUT/instructions.txt" | sort | tee "$OUT/instructions-ratio.txt"
awk '$2 > 1.001 || $3 > 1.001 { bad = bad " " $1 } END {
  print "2 instructions: " (bad == "" ? "pass" : "fail," bad)
}' "$OUT/instructions-ratio.txt" | tee -a "$OUT/verdict.txt"

Q=$(mktemp); chmod 644 "$Q"
{ grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv"; grep -v '^#' "$P/../queries-andor.tsv"; } > "$Q"
T="--read-mbps 43 --read-iops 2000"
EBS_MBPS=43 EBS_IOPS=2000 SERVE_FLAGS_A="$T" SERVE_FLAGS_B="$T" \
  bash "$P/serve-ab.sh" "$OUT/ab-tclass" "base=$A" "e62=$B" "$I" 4G "$Q" 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-tclass" base e62 | tee "$OUT/ab-tclass/reps.txt"
bash "$P/serve-ab.sh" "$OUT/ab-gp3" "base=$A" "e62=$B" "$I" 4G "$Q" 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-gp3" base e62 | tee "$OUT/ab-gp3/reps.txt"
rm "$Q"
python3 - "$OUT" << 'PY' | tee -a "$OUT/verdict.txt"
import json, math, os, sys
out = sys.argv[1]
for stage, d in (("3 t-class", "ab-tclass"), ("3 gp3", "ab-gp3")):
    root = os.path.join(out, d)
    reps = sorted({x.split("-")[0] for x in os.listdir(root) if x.split("-")[0].isdigit()}, key=int)
    p50 = {}
    for r in reps:
        for arm in ("base", "e62"):
            for q in json.load(open(os.path.join(root, f"{r}-{arm}", "cold.json")))["queries"]:
                p50.setdefault((arm, q["name"]), []).append(q["p50_ms"])
    names = sorted({n for _, n in p50})
    ratio = {n: (sum(p50[("e62", n)]) / len(reps)) / (sum(p50[("base", n)]) / len(reps)) for n in names}
    slow = [n for n in names if all(b / a > 1.10 for a, b in zip(p50[("base", n)], p50[("e62", n)]))]
    geo = lambda ns: math.exp(sum(math.log(ratio[n]) for n in ns) / len(ns))
    held = [n for n in names if n.startswith("andor_")]
    rest = [n for n in names if not n.startswith("andor_")]
    slow_rest = [n for n in slow if n in rest]
    ok = geo(rest) <= 1.01 and not slow_rest and geo(held) <= 0.97
    print(f"{stage}: the 18 {geo(rest):.3f}, over 10% slower in both: {slow_rest or 'none'}; "
          f"held-out {geo(held):.3f} ({' '.join(f'{n[6:]} {ratio[n]:.3f}' for n in held)}): {'pass' if ok else 'fail'}")
PY
