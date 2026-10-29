#!/bin/bash
# E49 (root): an A/A run of warmup-ab.sh at 4 GiB (both arms `--warm-mb 0`) to measure the
# harness's noise, then the A/B (3072 MiB against 0), both with scenarios "0:0 45:0 0:300"
# and 4 reps; then warmuprule.py, the keep rule against the A/A's noise, and
# warm-budget-check.sh at 2G and 4G. Search code is unchanged, so the parity gate is not
# rerun. Usage: run-e49.sh <bin-dir> <index-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected bin-dir index-dir out-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
SCEN="0:0 45:0 0:300"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/warmup-ab.sh" "$3/aa" "$1" "$2" 4G /tmp/q18.tsv 0 "$SCEN" 4
bash "$P/warmup-ab.sh" "$3/ab" "$1" "$2" 4G /tmp/q18.tsv 3072 "$SCEN" 4
python3 "$P/warmuprule.py" "$3/aa" "$3/ab" 4 $SCEN | tee "$3/rule.txt"
for mem in 2G 4G; do bash "$P/warm-budget-check.sh" "$1" "$2" "$mem" > "$3/budget-$mem.txt"; done
