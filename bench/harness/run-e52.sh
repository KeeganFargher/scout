#!/bin/bash
# E52 (root): `prefetch` joins by E50's balance only for a query whose estimated bytes cost
# 1.5x its reads at gp3's rates, and by E42's 32 KiB gaps otherwise. The format is
# unchanged, so both binaries serve one index (E43's base). serve-ab.sh at 2 CPUs / 4 GiB
# (2 reps, 3 warm rounds, 1 cold round) over the 18 big-corpus queries plus the 7 held out
# in queries-big-heldout.tsv, then cold-io.sh at gp3's IOPS cap for each binary. Output is
# unchanged (prefetch is advice; serve-ab.sh checks each body's hash across arms).
# Keep rule, set before the run: cold geomean pooled <= 0.97 and below 1.00 in both reps
# (E51's cold A/A: 0.989 and 0.991); warm pooled <= 1.03; no query, suite or held out,
# over 10% slower cold in both reps.
# Usage: run-e52.sh <base-bin-dir> <new-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir new-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
{ grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv"; grep -v "^#" "$P/../queries-big-heldout.tsv"; } > /tmp/q25.tsv
chmod 644 /tmp/q25.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "q=$B" "$I" 4G /tmp/q25.tsv 2 3 1
bash "$P/cold-io.sh" "$A" "$I" 4G /tmp/q25.tsv 3000 "$OUT/cold-io-base.txt" > /dev/null
bash "$P/cold-io.sh" "$B" "$I" 4G /tmp/q25.tsv 3000 "$OUT/cold-io-q.txt" > /dev/null
