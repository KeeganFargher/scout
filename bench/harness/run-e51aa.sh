#!/bin/bash
# E51's noise check (root): serve-ab.sh with one binary in both arms, same settings as
# run-e51.sh (2 CPUs / 4 GiB, E43's base index, 2 reps, 3 warm rounds, 1 cold round). The
# review of E51 found queries whose wire format E51 cannot change moving by up to 5%.
# Rule, set before the run: E51 stays kept only if its warm pooled geomean (0.979) is below
# 1 - d, where d is this A/A's largest per-rep warm geomean deviation from 1, and below
# this A/A's pooled warm geomean; otherwise E51 is reverted as not shown.
# Usage: run-e51aa.sh <bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 2 ] || { echo "expected bin-dir out-dir" >&2; exit 2; }
A=$1 OUT=$2 P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "a1=$A" "a2=$A" /mnt/ebs/idx-e43-base 4G /tmp/q18.tsv 2 3 1
