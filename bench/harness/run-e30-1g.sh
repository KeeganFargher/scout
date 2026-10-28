#!/bin/bash
# E30 at 2 CPUs / 1 GiB (root), where the v9 lz4 index's steady state no longer fits and
# warm rounds read from disk: no MADV_RANDOM against it on postings only and on dictionary +
# postings. Every arm keeps E30's per-list MADV_WILLNEED in `postings`.
# Usage: run-e30-1g.sh <results-subdir> <none-bin> <postings-bin> <both-bin> <index-dir>
set -euo pipefail
[ "$#" = 5 ] || { echo "expected results-subdir none-bin postings-bin both-bin index-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
R="$P/../results/$1"
bash "$P/ab-ebs.sh" "$R/ab-1G-postings" "none=$2" "postings=$3" "$5" 2 1G 0 2 3 1
bash "$P/ab-ebs.sh" "$R/ab-1G-both" "none=$2" "both=$4" "$5" 2 1G 0 2 3 1
