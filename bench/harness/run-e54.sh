#!/bin/bash
# E54 (root): E53's `Shard::size`, plus `prefetch` queuing each block's paths after its
# content, to test E53's cold loss (path pages first read during verification). Base is
# E52. search-ms.sh for warm, serve-ab.sh at 2 CPUs / 4 GiB (2 reps, 1 warm round, 1 cold
# round, the big suite) for cold, then cold-io.sh at gp3's IOPS cap for each binary.
# Rule, set before the run: search-ms geomean E54/E52 <= 0.98 in both passes and no query
# over 1.045 in both (E53's A/A); cold pooled <= 1.00 and no query over 10% slower cold
# in both reps.
# Usage: run-e54.sh <base-bin-dir> <new-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir new-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
bash "$P/search-ms.sh" "$A" "$B" "$I" "$P/../queries-search-ms.tsv" > "$OUT/search-ms.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "paths=$B" "$I" 4G /tmp/q18.tsv 2 1 1
bash "$P/cold-io.sh" "$A" "$I" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-base.txt" > /dev/null
bash "$P/cold-io.sh" "$B" "$I" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-paths.txt" > /dev/null
