#!/bin/bash
# Build a new benchmark index without modifying any existing baseline.
# Usage (root): index-subsecond.sh <scout-exe> <new-index-dir>
set -euo pipefail
[ "$#" = 2 ] || { echo "expected scout-exe new-index-dir" >&2; exit 2; }
bin=$1 index=$2
[ ! -e "$index" ] || { echo "index directory already exists" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
mkdir "$index"
chown 1000:1000 "$index"
bash "$P/ebs-run.sh" 2 4G 125 3000 "$bin" --index-dir "$index" add /mnt/ebs/big/chromium /mnt/ebs/big/llvm-project
bash "$P/ebs-run.sh" 2 4G 125 3000 "$bin" --index-dir "$index" index --threads 2 --content lz4
# Eviction cannot discard dirty pages. Finish writeback before a benchmark checks
# that this new index starts with zero cache residency in its own limited cgroup.
sync -f "$index/catalog.json"
du -sh "$index"
