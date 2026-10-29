#!/bin/bash
# E50 (root): `prefetch` joins the shortest gaps first and only while a block's reads cost
# more than its bytes, instead of every gap up to 32 KiB. The format is unchanged, so both
# binaries serve one index (E43's base). serve-ab.sh at 2 CPUs / 4 GiB (2 reps, 3 warm
# rounds, 1 cold round), then cold-io.sh at gp3's IOPS cap for each binary (reads and
# bytes per query). Search output is unchanged (prefetch is advice only; serve-ab.sh
# checks each query's output hash across arms), so the parity gate is not rerun.
# Keep rule, set before the run: cold geomean pooled <= 0.98 and below 1.00 in both reps;
# warm geomean pooled <= 1.03; no query over 10% slower cold in both reps.
# Usage: run-e50.sh <base-bin-dir> <new-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir new-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "bal=$B" "$I" 4G /tmp/q18.tsv 2 3 1
bash "$P/cold-io.sh" "$A" "$I" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-base.txt" > /dev/null
bash "$P/cold-io.sh" "$B" "$I" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-bal.txt" > /dev/null
