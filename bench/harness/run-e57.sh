#!/bin/bash
# E57 (root): E52's read-cost estimate and switch with E56's parallel estimate, proposed
# again as one change after both were reverted (E56), judged against the build before E52.
# An 8-CPU A/A of that build, taken first (bench/results/E57-estimate-retest/search-ms-aa-8.txt),
# moved a pass's geomean at most to 1.007 and a query at most to 1.032.
# Rule, set after that A/A and before this run, stage by stage, stopping at the first that
# fails: search-ms.sh geomean E57/before <= 1.01 in both passes and no query over 1.035 in
# both, at 1 CPU, then 2, then 8; then serve-ab.sh at 2 CPUs / 4 GiB (2 reps, 1 warm round,
# 1 cold round, the big suite): cold pooled <= 0.97 and below 1.00 in both reps, no query
# over 10% slower cold in both reps.
# Usage: run-e57.sh <before-bin-dir> <e57-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected before-bin-dir e57-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
Q="$P/../queries-search-ms.tsv"
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
for cpus in 1 2 8; do
  bash "$P/search-ms.sh" "$A" "$B" "$I" "$Q" "$cpus" > "$OUT/search-ms-$cpus.txt"
  awk -v c="$cpus" '{
    r1 = $3 / $2; r2 = $5 / $4; a += log(r1); b += log(r2); n++
    if (r1 > 1.035 && r2 > 1.035) bad = bad " " $1
  } END {
    g1 = exp(a / n); g2 = exp(b / n)
    printf "%s CPUs: geomean %.3f %.3f%s\n", c, g1, g2, (bad == "" ? "" : ", over 1.035 in both:" bad)
    exit (g1 <= 1.01 && g2 <= 1.01 && bad == "") ? 0 : 1
  }' "$OUT/search-ms-$cpus.txt" | tee -a "$OUT/verdict.txt" || { echo "stage $cpus CPUs fails the rule" | tee -a "$OUT/verdict.txt"; exit 1; }
done
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "e57=$B" "$I" 4G /tmp/q18.tsv 2 1 1
