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
# at 1 CPU, then 2, then 8; then serve-ab.sh on gp3's caps, the build before against E61
# with no flags: cold pooled <= 1.01, no query over 10% slower in both reps (E60's stage 3,
# on this binary: by E60's own reading, one binary's numbers do not certify another's).
# Then perf stat of files_mode on both, raw, for the record. E60's t-class stage (0.902,
# 0.903) is not rerun: its margin is ten times this one's.
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
grep -v -P "^(short_unindexable|huge_output)	" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-gp3" "base=$A" "e61=$B" "$I" 4G /tmp/q18.tsv 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-gp3" base e61 | tee "$OUT/ab-gp3/reps.txt"
# Raw perf stat of a warm files_mode search, 9 runs per binary, interleaved twice.
PERF=$(ls -d /usr/lib/linux-tools/*/perf | head -1)
for b in "$A" "$B" "$A" "$B"; do
  echo "== $b"
  taskset -c 0-1 "$b/scout" --index-dir "$I" search --freshness off --threads 2 -l -- "#include <vector>" > /dev/null || true
  taskset -c 0-1 "$PERF" stat -r 9 -e instructions:u,cycles:u,task-clock     "$b/scout" --index-dir "$I" search --freshness off --threads 2 -l -- "#include <vector>" 2>&1 > /dev/null
done > "$OUT/perf-stat.txt"
