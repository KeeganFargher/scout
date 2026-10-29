#!/bin/bash
# E45–E47 (background warm-up after ready) on the EBS-like box (root): warmup-ab.sh with the
# new binary, warm-up off against 3072 MiB (more than the whole index), queries at ready
# and 45 s after it, 2 reps. Search code is unchanged, so the parity gate is not rerun.
# The runs logged were made before warmup-ab.sh took scenarios ("wait0", "wait45").
# Usage: run-e45.sh <bin-dir> <index-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected bin-dir index-dir out-dir" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/warmup-ab.sh" "$3" "$1" "$2" 4G /tmp/q18.tsv 3072 "0:0 45:0" 2
