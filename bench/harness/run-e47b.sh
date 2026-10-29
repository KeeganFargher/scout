#!/bin/bash
# E47's confirmation run (root), with the review's changes (an index over the budget is not
# read, sync cycles count as activity): warmup-ab.sh at 4 GiB, 3072 MiB (the default there)
# against 0, scenarios "0:0 45:0 0:300", 2 reps; then warm-budget-check.sh, which runs
# without the flag, at 2G (the index over the budget) and 4G (under it). Search code is
# unchanged, so the parity gate is not rerun.
# Usage: run-e47b.sh <bin-dir> <index-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected bin-dir index-dir out-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/warmup-ab.sh" "$3/ab" "$1" "$2" 4G /tmp/q18.tsv 3072 "0:0 45:0 0:300" 2
for mem in 2G 4G; do bash "$P/warm-budget-check.sh" "$1" "$2" "$mem" > "$3/budget-$mem.txt"; done
