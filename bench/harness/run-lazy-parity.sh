#!/bin/bash
# After timing, run the broad-query rg oracle without contaminating the warm A/B.
# Usage: run-lazy-parity.sh <after-bin-dir> <index> <existing-output-dir> <rg>
set -euo pipefail
bin=$1 index=$2 out=$3 rg=$4
P=$(cd "$(dirname "$0")" && pwd)
[ -f "$out/warm.json" ] || { echo "complete the warm A/B first" >&2; exit 2; }
[ ! -e "$out/rg-parity.json" ] || { echo "parity output already exists" >&2; exit 2; }
# This is a correctness oracle, not a storage benchmark. Reading every source file
# under gp3's IOPS cap needlessly takes minutes per query; timing is already complete.
bash "$P/ebs-run.sh" 2 4G max max "$bin/scout-bench" \
  --scout "$bin/scout" --index-dir "$index" --queries "$P/../queries-lazy-parity.tsv" \
  --runs 1 --rg-runs 1 --rg "$rg" --scout-flags="--freshness off --threads 2" \
  --label lazy-blocks-rg-parity --json "$out/rg-parity.json" \
  2>&1 | tee "$out/rg-parity.log"
