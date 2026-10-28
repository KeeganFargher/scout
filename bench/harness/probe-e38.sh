#!/bin/bash
# E38's probes on the EBS-like box (root), warm, unconstrained memory, 2 CPUs by taskset:
# codec-probe (encoders on 6 sampled shards, one core), perf profiles of broad queries,
# and `-w`/`-i`/`-S` against `-F` on the same candidates. Output under <out-dir>.
# Usage: probe-e38.sh <bin-dir> <index-dir> <codec-probe> <perf> <out-dir>
set -euo pipefail
[ "$#" = 5 ] || { echo "expected bin-dir index-dir codec-probe perf out-dir" >&2; exit 2; }
B=$1 I=$2 CP=$3 PERF=$4 OUT=$5
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
cat "$I"/shards/*.native > /dev/null
taskset -c 0 "$CP" "$I/shards" 6 > "$OUT/codec-probe.txt"
echo -1 > /proc/sys/kernel/perf_event_paranoid
s() { taskset -c 0,1 "$B/scout" --index-dir "$I" search --threads 2 --freshness off "$@"; }
prof() { local n=$1; shift
  ( cd /tmp && "$PERF" record -q -g -F 4999 -o "/tmp/perf-$n.data" -- taskset -c 0,1 "$B/scout" --index-dir "$I" search --threads 2 --freshness off "$@" > /dev/null 2>&1 )
  { echo "== $n: $*"; "$PERF" report -i "/tmp/perf-$n.data" --no-children --percent-limit 2 --stdio -g none 2>/dev/null | grep -v "^#" | grep -v "^$" | head -8; } >> "$OUT/perf.txt"
}
prof files_mode -l '#include <vector>'
prof literal_common -F 'std::unique_ptr'
prof literal_long -F 'Use of this source code is governed by a BSD-style license'
prof case_insensitive -i deprecated
prof word -w DCHECK
prof alternation 'TODO|FIXME|HACK'
prof anchored '^#include "base/\w+\.h"$'
prof date_regex '\d{4}-\d{2}-\d{2}'
med() { for r in 1 2 3 4 5 6 7 8; do st=$(date +%s%N); s "$@" > /dev/null; echo $(( ($(date +%s%N) - st) / 1000000 )); done | tail -7 | sort -n | sed -n 4p; }
for a in "-w DCHECK" "-F DCHECK" "-i deprecated" "-F deprecated" "-S webassembly" "-F webassembly"; do
  # shellcheck disable=SC2086
  echo "$a wall_ms=$(med $a) $(s --stats $a 2>&1 >/dev/null | sed 's/ plan=.*//' | grep -o 'candidates=[0-9]*\|search_ms=[0-9.]*' | tr '\n' ' ')" >> "$OUT/rewrites.txt"
done
chown -R 1000:1000 "$OUT"
