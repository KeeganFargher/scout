#!/bin/bash
# E60 (root): the read join from the volume's caps (--read-mbps, --read-iops; gp3's by
# default), instead of a constant 32 KiB.
# Rule, set before this run, stage by stage:
# 1. search-ms.sh at 2 CPUs, the build before E60 against E60 with no flags (output parity
#    checked): geomean <= 1.011 in both passes (E53's A/A).
# 2. serve-ab.sh at 2 CPUs / 4 GiB on the t-class caps (EBS_MBPS=43 EBS_IOPS=2000), E60
#    against itself told those caps (--read-mbps 43 --read-iops 2000): cold pooled <= 0.97,
#    below 1.00 in both reps, no query over 10% slower in both reps (E59's 16 KiB, now
#    from the flags).
# 3. serve-ab.sh on gp3's caps, the build before E60 against E60 with no flags: cold
#    pooled <= 1.01, no query over 10% slower in both reps (the default is unchanged).
# Usage: run-e60.sh <before-bin-dir> <e60-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected before-bin-dir e60-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
bash "$P/search-ms.sh" "$A" "$B" "$I" "$P/../queries-search-ms.tsv" 2 > "$OUT/search-ms-2.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
EBS_MBPS=43 EBS_IOPS=2000 SERVE_FLAGS_B="--read-mbps 43 --read-iops 2000" \
  bash "$P/serve-ab.sh" "$OUT/ab-tclass" "gp3join=$B" "tjoin=$B" "$I" 4G /tmp/q18.tsv 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-tclass" gp3join tjoin | tee "$OUT/ab-tclass/reps.txt"
bash "$P/serve-ab.sh" "$OUT/ab-gp3" "base=$A" "e60=$B" "$I" 4G /tmp/q18.tsv 2 1 1
python3 "$P/serveab-reps.py" "$OUT/ab-gp3" base e60 | tee "$OUT/ab-gp3/reps.txt"
