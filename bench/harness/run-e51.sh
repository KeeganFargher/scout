#!/bin/bash
# E51 (root): `serve` sends every body with Content-Length instead of chunked transfer
# encoding. serve-ab.sh at 2 CPUs / 4 GiB on E43's base index (2 reps, 3 warm rounds,
# 1 cold round); serveab.py fails on any body that differs between arms.
# Keep rule, set before the run: warm geomean pooled <= 0.99 and below 1.00 in both reps;
# cold pooled <= 1.02; no query over 10% slower warm in both reps.
# Usage: run-e51.sh <base-bin-dir> <new-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir new-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "len=$B" /mnt/ebs/idx-e43-base 4G /tmp/q18.tsv 2 3 1
