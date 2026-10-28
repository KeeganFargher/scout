#!/bin/bash
# Correctness only: run after timing, lifting the disk cap for rg's source-file reads.
# Usage: run-subsecond-parity.sh <after-bin-dir> <after-index> <completed-ab-dir> <rg>
set -euo pipefail
[ "$#" = 4 ] || { echo "expected after-bin-dir after-index completed-ab-dir rg" >&2; exit 2; }
bin=$1 index=$2 out=$3 rg=$4
P=$(cd "$(dirname "$0")" && pwd)
[ -f "$out/summary.json" ] || { echo "complete the A/B first" >&2; exit 2; }
[ ! -e "$out/rg-parity.json" ] || { echo "parity output already exists" >&2; exit 2; }
bash "$P/ebs-run.sh" 2 4G max max "$bin/scout-bench" \
  --scout "$bin/scout" --index-dir "$index" --queries "$P/../queries-lazy-parity.tsv" \
  --runs 1 --rg-runs 1 --rg "$rg" --scout-flags="--freshness off --threads 2" \
  --label subsecond-rg-parity --json "$out/rg-parity.json" \
  2>&1 | tee "$out/rg-parity.log"
