#!/bin/bash
# `scout serve` on the EBS-like box (root): the server runs in ebs-run.sh's 2 CPU cgroup with
# <mem>, qbench.py drives it from CPUs 8-11 outside it, warm then cold, over <queries.tsv>.
# The server's sync config adds one tiny repo to the index, so <index-dir> must be a copy
# made for serving, never a baseline another run compares against.
# The volume's caps are gp3's baseline, 125 MB/s and 3000 IOPS, unless EBS_MBPS and EBS_IOPS
# name others: a t3.medium's own EBS baseline (~43 MB/s, 2000 IOPS) is below gp3's, so a 4
# GiB t-class box is capped by the instance (E59). They are written to ready.txt.
# Usage: [EBS_MBPS=n EBS_IOPS=n] serve-ebs.sh <bin-dir> <index-dir> <mem> <queries.tsv> <warm-rounds> <cold-rounds> <out-dir>
set -euo pipefail
[ "$#" = 7 ] || { echo "expected bin-dir index-dir mem queries warm-rounds cold-rounds out-dir" >&2; exit 2; }
B=$1 I=$2 mem=$3 Q=$4 WR=$5 CR=$6 OUT=$7
mbps=${EBS_MBPS:-125} iops=${EBS_IOPS:-3000}
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
port=18500
# The server starts on an evicted index, as after a restart: the time to ready is what an
# open reads (E36 reads every shard's dictionary at open). No background warm-up (E47):
# the warm and cold rounds measure the server's own reads, as they did before it; a
# restart with warm-up is warmup-ebs.sh's. Binaries before E47 refuse the flag.
sync; echo 3 > /proc/sys/vm/drop_caches
t0=$(date +%s%N)
bash "$P/ebs-run.sh" 2 "$mem" "$mbps" "$iops" "$B/scout" --index-dir "$I" serve --config /home/keegan/small/serve.toml \
  --listen "127.0.0.1:$port" --max-output-mb 4096 --search-timeout-secs 900 --warm-mb 0 > "$OUT/serve.log" 2>&1 &
sp=$!
up=0
for _ in $(seq 600); do
  if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
  sleep 0.05
done
[ $up = 1 ] || { echo "serve did not become ready" >&2; cat "$OUT/serve.log" >&2; kill $sp; exit 2; }
echo "ready_ms=$(( ($(date +%s%N) - t0) / 1000000 )) mbps=$mbps iops=$iops" | tee "$OUT/ready.txt"
spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1)
cg=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$cg" "$I" "$Q" warm "$WR" "$OUT/warm.json"
taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$cg" "$I" "$Q" cold "$CR" "$OUT/cold.json"
kill "$spid"; wait "$sp" 2>/dev/null || true
python3 "$P/ebssum.py" "$OUT/warm.json" "$OUT/cold.json" | tee "$OUT/summary.txt"
