#!/bin/bash
# E56 (root): E52's read-cost estimate in parallel over every shard of every repo, not
# over repos. E52 was measured only at 2 CPUs; its review asked for 1 and 8, and at 8 its
# estimate cost 7.5-8.8% of a broad warm search against the build before it
# (bench/results/E56-parallel-estimate/e52-*.txt). search-ms.sh of E52 against E56 at 8
# and 2 CPUs, of the build before E52 against E56 at 8, then serve-ab.sh at 2 CPUs / 4 GiB
# (2 reps, 1 warm round, 1 cold round, the big suite) as a cold guard.
# Rule, set before the run: E56/E52 geomean <= 0.97 in both passes at 8 CPUs and <= 1.011
# (E53's A/A) in both at 2; cold pooled <= 1.01 (E51's cold A/A) and no query over 10%
# slower cold in both reps. And E52 itself: if E56 is still over 1.03 against the build
# before E52 at 8 CPUs in either pass, E52 regresses scale-up and is reverted with E56.
# Usage: run-e56.sh <pre-e52-bin-dir> <e52-bin-dir> <e56-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 4 ] || { echo "expected pre-e52-bin-dir e52-bin-dir e56-bin-dir out-dir" >&2; exit 2; }
P0=$1 A=$2 B=$3 OUT=$4 P=$(cd "$(dirname "$0")" && pwd)
I=/mnt/ebs/idx-e43-base
Q="$P/../queries-search-ms.tsv"
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
bash "$P/search-ms.sh" "$A" "$B" "$I" "$Q" 8 > "$OUT/search-ms-8.txt"
bash "$P/search-ms.sh" "$A" "$B" "$I" "$Q" 2 > "$OUT/search-ms-2.txt"
bash "$P/search-ms.sh" "$P0" "$B" "$I" "$Q" 8 > "$OUT/search-ms-pre-e52-8.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A" "par=$B" "$I" 4G /tmp/q18.tsv 2 1 1
