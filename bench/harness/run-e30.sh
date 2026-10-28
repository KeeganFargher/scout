#!/bin/bash
# E30 (root): two read-advice builds on one lz4 chromium + llvm-project index of the EBS
# volume, at 2 CPUs / 4 GiB (the index fits: cold reads) and 2 CPUs / 2 GiB (it does not:
# the steady state reads too).
# Usage: run-e30.sh <results-subdir> <nameA>=<bin-dir> <nameB>=<bin-dir> <index-dir>
set -euo pipefail
[ "$#" = 4 ] || { echo "expected results-subdir armA armB index-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
R="$P/../results/$1"
for mem in 4G 2G; do
  bash "$P/ab-ebs.sh" "$R/ab-$mem" "$2" "$3" "$4" 2 "$mem" 0 2 3 1
done
