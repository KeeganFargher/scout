#!/bin/bash
# E59 (root): `prefetch` joining gaps of up to 16 KiB instead of 32, on a t-class volume.
# A t3.medium's own EBS baseline, ~43 MB/s and 2000 IOPS, caps a 4 GiB t-class box below
# gp3's 125 MB/s and 3000, and a read saved there is worth ~21 KB of transfer, half gp3's
# ~41.7 KB that set 32 KiB (E42).
# Rule, set before this run: serve-ab.sh at 2 CPUs / 4 GiB with EBS_MBPS=43 EBS_IOPS=2000
# (2 reps, 1 warm round, 1 cold round, the big suite): cold pooled <= 0.97, below 1.00 in
# both reps, no query over 10% slower cold in both reps. If it passes, the same pair at
# gp3's caps: within 1.01 pooled keeps 16 KiB outright; over it, the gap is the volume's
# to set, and that is the next experiment.
# Usage: run-e59.sh <base-bin-dir> <gap16-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir gap16-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
EBS_MBPS=43 EBS_IOPS=2000 bash "$P/serve-ab.sh" "$OUT/ab-tclass" "base=$A" "gap16=$B" "$I" 4G /tmp/q18.tsv 2 1 1
