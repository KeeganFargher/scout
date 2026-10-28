#!/bin/bash
# CLI latency on the EBS-like volume (run as root: wsl.exe -u root), steady state and cold:
#   run-ebs.sh <out-dir> <bin-dir> <index-dir> <cpus> <mem> <delay_ms> <warm-rounds> <cold-rounds> [queries.tsv]
# Sets the volume's read delay (ebs-setup.sh), then runs qbench.py as uid 1000 inside an
# ebs-run.sh cgroup at gp3's baseline, 125 MB/s and 3000 IOPS. "warm" here is the steady
# state of a box whose memory holds only part of the index: one untimed pass, then timed
# passes, each query finding whatever the ones before it left in the page cache. "cold"
# evicts the index before every request (a restart). The queries default to
# bench/queries-big.tsv without short_unindexable and huge_output: those print 900 and
# 480 MB, which the harness would hold in the same cgroup's memory, and are bound by the
# pipe (E26) and capped by `serve`'s --max-output-mb anyway.
set -euo pipefail
out=$1 bin=$2 idx=$3 cpus=$4 mem=$5 delay=$6 wr=$7 cr=$8
P=$(cd "$(dirname "$0")" && pwd)
q=${9:-}
if [ -z "$q" ]; then
  q=$(mktemp /tmp/queries-ebs.XXXXXX.tsv)
  grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > "$q"
  chmod 644 "$q"
fi
[ -e "$out" ] && { echo "$out exists: move it away first" >&2; exit 2; }
install -d -o 1000 -g 1000 "$out"
bash "$P/ebs-setup.sh" "$delay" > "$out/volume.txt"
tag="${cpus}cpu-$mem-${delay}ms"
sync; echo 3 > /proc/sys/vm/drop_caches
for mode in warm cold; do
  rounds=$wr; [ $mode = cold ] && rounds=$cr
  [ "$rounds" = 0 ] && continue
  bash "$P/ebs-run.sh" "$cpus" "$mem" 125 3000 python3 "$P/qbench.py" cli "$bin" "$idx" "$q" "$mode" "$rounds" "$out/$mode-$tag.json" \
    2>&1 | tee -a "$out/run.log"
done
python3 "$P/ebssum.py" "$out"/*.json | tee "$out/summary.txt"
