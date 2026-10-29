#!/bin/bash
# E61, the build that ships (root), after review: the flags now belong to `search` and `serve`
# only and `--stats` reports each opened repo's join, so this binary is neither the one whose
# warm stages passed nor the one counted in run-e61-final.sh. By E60's own reading one
# binary's numbers do not certify another's, so every stage the keep rests on runs here, on
# this binary. Rules, committed with this script before it runs:
# 1. Instructions retired (`perf stat -e instructions:u`, 9 runs, interleaved twice, 1 CPU),
#    the build before E60 against this one, every query of queries-search-ms.tsv: within
#    +0.1% in both passes. That bound was first set in run-e61-final.sh, after the warm
#    rule had failed; it is reused unchanged, not re-chosen.
# 2. gp3's caps, the build before against this one with no flags: cold pooled <= 1.01, no
#    query over 10% slower in both reps (E60's stage 3).
# 3. A t3.medium's caps (43 MB/s, 2000 IOPS), this binary told them against itself not told:
#    cold pooled <= 0.97, below 1.00 in both reps, no query over 10% slower in both (E60's
#    stage 2).
# Every stage runs whatever an earlier one gave, so the record is whole; verdict.txt says
# which pass.
# Usage: run-e61-cert.sh <before-bin-dir> <e61-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected before-bin-dir e61-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
shopt -s nullglob
perfs=(/usr/lib/linux-tools/*/perf)
[ "${#perfs[@]}" -gt 0 ] || { echo "no perf under /usr/lib/linux-tools" >&2; exit 2; }
PERF=${perfs[0]}
grep -v -e '^#' -e '^$' "$P/../queries-search-ms.tsv" | while IFS= read -r line; do
  name=${line%%$'\t'*} rest=${line#*$'\t'}
  flags=${rest%%$'\t'*} pat=${rest#*$'\t'}
  for b in "$A" "$B" "$A" "$B"; do
    # Warm-up; a search that matches nothing exits 1, as rg does, so only its exit is ignored.
    # shellcheck disable=SC2086
    taskset -c 0 "$b/scout" --index-dir "$I" search --freshness off --threads 1 $flags -- "$pat" < /dev/null > /dev/null || true
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
  print "1 instructions: " (bad == "" ? "pass" : "fail," bad)
}' "$OUT/instructions-ratio.txt" | tee "$OUT/verdict.txt"
Q=$(mktemp); chmod 644 "$Q"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > "$Q"
bash "$P/serve-ab.sh" "$OUT/ab-gp3" "base=$A" "e61=$B" "$I" 4G "$Q" 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-gp3" base e61 | tee "$OUT/ab-gp3/reps.txt"
EBS_MBPS=43 EBS_IOPS=2000 SERVE_FLAGS_B="--read-mbps 43 --read-iops 2000" \
  bash "$P/serve-ab.sh" "$OUT/ab-tclass" "gp3join=$B" "tjoin=$B" "$I" 4G "$Q" 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-tclass" gp3join tjoin | tee "$OUT/ab-tclass/reps.txt"
rm "$Q"
# Stages 2 and 3 from each pair's summary (the cold section's pooled geomean) and reps.txt.
python3 - "$OUT" << 'PY' | tee -a "$OUT/verdict.txt"
import re, sys
out = sys.argv[1]
def read(d):
    summary = open(f"{out}/{d}/summary.txt").read()
    cold = summary[summary.index("## cold"):]
    pooled = float(re.search(r"geomean \S+: ([0-9.]+)", cold).group(1))
    reps = open(f"{out}/{d}/reps.txt").read()
    per = [float(x) for x in re.findall(r"cold geomean ([0-9.]+)", reps)]
    slow = re.search(r"every rep: (.*)", reps).group(1)
    return pooled, per, slow
pooled, per, slow = read("ab-gp3")
ok = pooled <= 1.01 and slow == "none"
print(f"2 gp3: pooled {pooled:.3f}, reps {per}, slow in both: {slow}: {'pass' if ok else 'fail'}")
pooled, per, slow = read("ab-tclass")
ok = pooled <= 0.97 and all(x < 1.0 for x in per) and slow == "none"
print(f"3 t-class: pooled {pooled:.3f}, reps {per}, slow in both: {slow}: {'pass' if ok else 'fail'}")
PY
