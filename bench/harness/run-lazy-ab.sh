#!/bin/bash
# Warm, interleaved A/B against a frozen current-tree baseline. Run as root.
# Usage: run-lazy-ab.sh <before> <after> <index> <new-output-directory>
set -euo pipefail
before=$1 after=$2 index=$3 out=$4
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$out" ] || { echo "$out already exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$out"
cp "$P/../queries-lazy.tsv" "$out/queries.tsv"
bash "$P/ebs-run.sh" 2 4G 125 3000 python3 "$P/lazy-ab.py" \
  "$index" "$out/queries.tsv" "$before" "$after" "$out/warm.json" 5 2 \
  2>&1 | tee "$out/warm.log"
