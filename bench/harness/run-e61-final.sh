#!/bin/bash
# E61, the final binary (root), after its warm stage failed at 1 CPU (1.000 and 1.013,
# `crbug_regex` 1.042 in one pass and 0.997 in the other) on a change that adds no work.
# Adopted after that failure, and disclosed as such: `perf stat` instructions retired at 1
# CPU on every query of queries-search-ms.tsv, the build before against E61, interleaved
# twice, 9 runs each; every query within 0.1% means no added CPU work. Then E60's gp3 cold
# stage on this binary: serve-ab.sh, cold pooled <= 1.01, no query over 10% slower in both
# reps.
# Usage: run-e61-final.sh <before-bin-dir> <e61-bin-dir> <out-dir>
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
    # shellcheck disable=SC2086
    taskset -c 0 "$b/scout" --index-dir "$I" search --freshness off --threads 1 $flags -- "$pat" < /dev/null > /dev/null || true
    # shellcheck disable=SC2086
    n=$(taskset -c 0 "$PERF" stat -x, -r 9 -e instructions:u "$b/scout" --index-dir "$I" search --freshness off --threads 1 \
      $flags -- "$pat" < /dev/null 2>&1 > /dev/null | awk -F, '$3 == "instructions:u" { print $1 }')
    [ -n "$n" ] || { echo "$name: no instruction count from perf" >&2; exit 1; }
    echo "$name $(basename "$b") $n"
  done
done > "$OUT/instructions.txt"
# Per query: B's instructions over A's, each pass.
awk '{ v[$1, ++k[$1]] = $3 } END {
  for (q in k) printf "%s %.5f %.5f\n", q, v[q, 2] / v[q, 1], v[q, 4] / v[q, 3]
}' "$OUT/instructions.txt" | sort | tee "$OUT/instructions-ratio.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-gp3" "base=$A" "e61=$B" "$I" 4G /tmp/q18.tsv 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-gp3" base e61 | tee "$OUT/ab-gp3/reps.txt"
