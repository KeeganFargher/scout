#!/bin/bash
# Instructions and cycles of a warm files_mode search, base and E60, interleaved.
set -euo pipefail
PERF=/usr/lib/linux-tools/6.8.0-147-generic/perf
I=/mnt/ebs/idx-e43-base
for b in bin-head bin-e60 bin-head bin-e60; do
  taskset -c 0-1 /home/keegan/small/$b/scout --index-dir $I search --freshness off --threads 2 -l -- "#include <vector>" > /dev/null || true
  echo "== $b"
  taskset -c 0-1 "$PERF" stat -r 9 -e instructions:u,cycles:u,task-clock /home/keegan/small/$b/scout --index-dir $I search --freshness off --threads 2 -l -- "#include <vector>" 2>&1 >/dev/null | grep -E "instructions|cycles|task-clock|elapsed"
done
