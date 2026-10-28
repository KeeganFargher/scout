#!/bin/bash
# E31 + E32 (root): e29 (fixed: /home/keegan/small/bin-e29b on /mnt/ebs/idx-e29, its v6
# lz4 index) against a build-e32.sh build on a v7 lz4 index of the same repos, at 2 CPUs /
# 4 GiB and 2 CPUs / 2 GiB.
# Usage: run-e32.sh <results-subdir> <e32-bin-dir> <v7-index-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected results-subdir e32-bin-dir v7-index-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
R="$P/../results/$1"
for mem in 4G 2G; do
  bash "$P/ab-ebs.sh" "$R/ab-$mem" e29=/home/keegan/small/bin-e29b "e32=$2@$3" /mnt/ebs/idx-e29 2 "$mem" 0 2 3 1
done
