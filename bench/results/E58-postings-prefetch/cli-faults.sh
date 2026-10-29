#!/bin/bash
# Probe (root): where a cold small search's reads are issued from. block_rq_issue with call
# graphs: a read issued under a page fault is a synchronous wait; one under madvise is not.
set -euo pipefail
PERF=/usr/lib/linux-tools/6.8.0-147-generic/perf
B=/home/keegan/small/bin-head
I=/mnt/ebs/idx-e43-base
O=/tmp/e58p
rm -rf "$O"; mkdir -p "$O"
for q in "llvm_regex||dyn_cast<\\w+Inst>" "selective_symbol|-F|getAsCXXRecordDecl"; do
  name=${q%%|*}; rest=${q#*|}; flags=${rest%%|*}; pat=${rest#*|}
  sync; echo 3 > /proc/sys/vm/drop_caches
  # shellcheck disable=SC2086
  taskset -c 0-1 "$PERF" record -q -g -e block:block_rq_issue -e major-faults -o "$O/$name.data" -- \
    "$B/scout" --index-dir "$I" search --freshness off --threads 2 $flags -- "$pat" > /dev/null || true
  echo "== $name"
  "$PERF" report -i "$O/$name.data" --stdio --no-children -e block:block_rq_issue -g caller,0.5,callee --percent-limit 3 2>/dev/null \
    | grep -v '^$' | grep -v '^#' | head -60
done
