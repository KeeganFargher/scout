#!/bin/bash
# E31 alone (root): the count hint off against on, same tree and index, at 2 CPUs / 4 GiB
# (cold: the dictionary lookups the hint saves are disk reads) and 1 GiB (warm misses too).
# Usage: run-e31.sh <results-subdir> <off-bin-dir> <on-bin-dir> <index-dir>
set -euo pipefail
[ "$#" = 4 ] || { echo "expected results-subdir off-bin-dir on-bin-dir index-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
R="$P/../results/$1"
for mem in 4G 1G; do
  bash "$P/ab-ebs.sh" "$R/ab-$mem" "off=$2" "hint=$3" "$4" 2 "$mem" 0 2 3 1
done
