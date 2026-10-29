#!/bin/bash
# E53 (root): planning and read-ahead look up a candidate's size with `Shard::size`, not
# `meta`, which checked the path's UTF-8 on every lookup. CPU only, so warm is judged by
# search-ms.sh (median of 9, two interleaved passes) over queries-search-ms.tsv, then
# serve-ab.sh at 2 CPUs / 4 GiB (2 reps, 1 warm round, 1 cold round, the big suite) guards
# cold.
# A first search-ms run failed a rule set without this harness's noise (faster on every
# query in both passes: `alternation` 1.035 in one). Its A/A (one binary twice) moved a
# query up to 1.044 in a pass and a pass's geomean up to 1.011. Rule, set after that A/A
# and before this run: geomean E53/E52 <= 0.98 in both passes; no query over 1.045 in
# both passes; cold pooled <= 1.02 and no query over 10% slower cold in both reps.
# Usage: run-e53.sh <base-bin-dir> <new-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir new-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
bash "$P/search-ms.sh" "$A" "$B" "$I" "$P/../queries-search-ms.tsv" > "$OUT/search-ms.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "size=$B" "$I" 4G /tmp/q18.tsv 2 1 1
