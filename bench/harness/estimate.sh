#!/bin/bash
# Each query's read-cost estimate (E52), as `scout search --stats` prints it: reads, MiB,
# the ratio bytes / (reads × 41.7 KB, gp3's bytes per read) and its side of the 1.5x line
# (BAL: E50's balance, e42: 32 KiB joins). No timing, so the cache state does not matter.
# Usage: estimate.sh <bin-dir> <index-dir> <queries.tsv>...
set -euo pipefail
[ "$#" -ge 3 ] || { echo "expected bin-dir index-dir queries.tsv..." >&2; exit 2; }
B=$1 I=$2
shift 2
echo "# query reads MiB ratio side"
grep -h -v -P "^(#|short_unindexable|huge_output)" "$@" | while IFS= read -r q; do
  # Tab is IFS whitespace, so `read` would collapse an empty flags field.
  name=$(cut -f1 <<< "$q"); flags=$(cut -f2 <<< "$q"); pat=$(cut -f3- <<< "$q")
  # shellcheck disable=SC2086
  st=$("$B/scout" --index-dir "$I" search --freshness off --stats $flags -- "$pat" 2>&1 >/dev/null || true)
  r=$(grep -o 'cost_reads=[0-9]*' <<< "$st" | cut -d= -f2 || true)
  by=$(grep -o 'cost_bytes=[0-9]*' <<< "$st" | cut -d= -f2 || true)
  [ -n "$r" ] && [ -n "$by" ] || { echo "$name: no cost in --stats: $st" >&2; exit 1; }
  awk -v n="$name" -v r="$r" -v b="$by" 'BEGIN {
    x = (r > 0) ? b / (r * 125000000 / 3000) : 0
    printf "%s %d %.1f %.2f %s\n", n, r, b / 1048576, x, (2 * b >= 3 * r * 125000000 / 3000) ? "BAL" : "e42"
  }'
done
