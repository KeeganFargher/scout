#!/bin/bash
# A restart on the EBS-like box (root), for the background warm-up (E45): drops the page
# cache, starts `scout serve --warm-mb <warm-mb>` in ebs-run.sh's 2 CPU cgroup with <mem>,
# waits for ready, then <wait-secs> more, and runs each query once in order (the "pass"),
# <gap-ms> apart (0: back to back, as a client's burst), timing each with the volume's bytes from the cgroup's io.stat. Then qbench.py's warm rounds
# (steady state), the index's residency, the /status warm-up and the cgroup's peak memory.
# qbench's cold rounds and cold-io.sh evict the index before every query, so they cannot
# see a warm-up: this is the measurement that can. The server's sync adds one tiny repo to
# the index, as in serve-ebs.sh, so <index-dir> must be a copy made for serving.
# Times include curl's start (a few ms), the same in both arms.
# Usage: warmup-ebs.sh <bin-dir> <index-dir> <mem> <queries.tsv> <warm-mb> <wait-secs> <gap-ms> <out-dir>
set -euo pipefail
[ "$#" = 8 ] || { echo "expected bin-dir index-dir mem queries warm-mb wait-secs gap-ms out-dir" >&2; exit 2; }
B=$1 I=$2 mem=$3 Q=$4 WMB=$5 WAIT=$6 GAP=$7 OUT=$8
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
dev=$(cat "/sys/block/$(basename "$(readlink -f /dev/mapper/ebs)")/dev")
port=18700
! curl -s -o /dev/null "http://127.0.0.1:$port/status" || { echo "port $port is in use" >&2; exit 2; }
spid="" sp=""
# As cold-io.sh: stop a server a failure left running, keep the failure's exit status.
trap 'rc=$?; [ -z "$spid" ] || kill "$spid" 2>/dev/null || true; [ -z "$sp" ] || wait "$sp" 2>/dev/null || true; exit $rc' EXIT
sync; echo 3 > /proc/sys/vm/drop_caches
t0=$(date +%s%N)
bash "$P/ebs-run.sh" 2 "$mem" 125 3000 "$B/scout" --index-dir "$I" serve --config /home/keegan/small/serve.toml \
  --listen "127.0.0.1:$port" --max-output-mb 4096 --search-timeout-secs 900 --warm-mb "$WMB" > "$OUT/serve.log" 2>&1 &
sp=$!
up=0
for _ in $(seq 600); do
  if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
  sleep 0.05
done
# Before the check, as in cold-io.sh: the trap must kill the server a failure leaves running,
# or its `wait` on ebs-run.sh never returns.
spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1 || true)
[ $up = 1 ] || { echo "serve did not become ready" >&2; cat "$OUT/serve.log" >&2; exit 2; }
t_ready=$(date +%s%N)
echo "ready_ms=$(( (t_ready - t0) / 1000000 ))" > "$OUT/ready.txt"
cg=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
io() { grep "^$dev " "$cg/io.stat" | tr ' ' '\n' | awk -F= -v k="$1" '$1==k{print $2}'; }
# Both arms wait the same wall time from ready, warm-up or not.
while [ $(( ($(date +%s%N) - t_ready) / 1000000000 )) -lt "$WAIT" ]; do sleep 0.2; done
curl -s "http://127.0.0.1:$port/status" > "$OUT/status-before-pass.json"
echo "query us rmib" > "$OUT/pass.txt"
grep -v -e '^#' -e '^$' "$Q" | while IFS= read -r line; do
  # As cold-io.sh: three tab-separated fields, flags possibly empty.
  name=${line%%$'\t'*} rest=${line#*$'\t'}
  flags=${rest%%$'\t'*} pat=${rest#*$'\t'}
  [ "$name" != "$line" ] && [ "$flags" != "$rest" ] || { echo "not name<TAB>flags<TAB>pattern: $line" >&2; exit 2; }
  params="n=1"
  for f in $flags; do
    case $f in -F | -i | -S | -w | -c | -l) params+="&${f#-}=1" ;; *) echo "$name: unknown flag $f" >&2; exit 2 ;; esac
  done
  [ "$GAP" = 0 ] || sleep "$(awk -v g="$GAP" 'BEGIN{print g/1000}')"
  b0=$(io rbytes) q0=$(date +%s%N)
  curl -s --fail -G "http://127.0.0.1:$port/search?$params" --data-urlencode "q=$pat" > /dev/null
  echo "$name $(( ($(date +%s%N) - q0) / 1000 )) $(( ($(io rbytes) - b0) / 1048576 ))" >> "$OUT/pass.txt"
done
curl -s "http://127.0.0.1:$port/status" > "$OUT/status-after-pass.json"
# 10 rounds from E49 (3 before): a p50 of 3 let the ~1.4 ms `literal_none` swing 0.8–1.4x
# between arms, enough to move the steady geomean by 2%.
taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$cg" "$I" "$Q" warm 10 "$OUT/warm.json"
# With status-after-pass: what the walk read during qbench's rounds (E47b's 0:300 steady).
curl -s "http://127.0.0.1:$port/status" > "$OUT/status-after-warm.json"
python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import pagecache; r, t = pagecache.residency(sys.argv[2]); print(f"index_resident_mib={r:.0f} index_mib={t:.0f}")' "$P" "$I" > "$OUT/residency.txt"
kill "$spid"; wait "$sp" 2>/dev/null || true
spid="" sp=""
grep "^ebs " "$OUT/serve.log" > "$OUT/cgroup.txt"
cat "$OUT/ready.txt" "$OUT/residency.txt" "$OUT/cgroup.txt"
