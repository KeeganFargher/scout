#!/bin/bash
# E61 (root): E60's read join from the volume's caps, retested warm after E60's stage 1
# failed (search-ms at 2 CPUs 1.023 and 1.002 with the box busy, 1.020 and 1.003 quiet,
# `files_mode` 1.059 and 1.054) though E60 and the build before it retire the same
# instructions on `files_mode` (+0.015%) in the same time. Rebuilt with E60's review fixes
# (--stats prints read_join; the join is computed only for search and serve), so its code
# layout differs. An A/A that day (bench/results/E60-read-join/search-ms-2-aa.txt): 1.004
# and 0.999, worst query 1.032.
# Rule, set before this run, stage by stage, stopping at the first that fails, as E57:
# search-ms.sh geomean E61/before <= 1.011 in both passes and no query over 1.035 in both,
# at 1 CPU, then 2, then 8. E60's cold stages, unchanged code on that path, stand.
# Usage: run-e61.sh <before-bin-dir> <e61-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected before-bin-dir e61-bin-dir out-dir" >&2; exit 2; }
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
    exit (g1 <= 1.011 && g2 <= 1.011 && bad == "") ? 0 : 1
  }' "$OUT/search-ms-$cpus.txt" | tee -a "$OUT/verdict.txt" || { echo "stage $cpus CPUs fails the rule" | tee -a "$OUT/verdict.txt"; exit 1; }
done
