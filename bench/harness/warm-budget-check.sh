#!/bin/bash
# What the default `--warm-mb` does on the EBS-like box (root, E47): drops the page cache,
# serves <index-dir> in a 2 CPU / <mem> cgroup without the flag, waits for the warm-up to
# end (300 s at most), and prints its /status state, the server's log line about it, and
# the cgroup's peak memory and reads. The default is the cgroup's memory less two
# generations of tables (three quarters by default): an index over it is not read at all.
# Usage: warm-budget-check.sh <bin-dir> <index-dir> <mem>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected bin-dir index-dir mem" >&2; exit 2; }
B=$1 I=$2 mem=$3
P=$(cd "$(dirname "$0")" && pwd)
port=18800
! curl -s -o /dev/null "http://127.0.0.1:$port/status" || { echo "port $port is in use" >&2; exit 2; }
log=$(mktemp)
spid="" sp=""
# As cold-io.sh: stop a server a failure left running, keep the failure's exit status.
trap 'rc=$?; [ -z "$spid" ] || kill "$spid" 2>/dev/null || true; [ -z "$sp" ] || wait "$sp" 2>/dev/null || true; rm -f "$log"; exit $rc' EXIT
sync; echo 3 > /proc/sys/vm/drop_caches
bash "$P/ebs-run.sh" 2 "$mem" 125 3000 "$B/scout" --index-dir "$I" serve --config /home/keegan/small/serve.toml \
  --listen "127.0.0.1:$port" > "$log" 2>&1 &
sp=$!
done=0
for _ in $(seq 3000); do
  if curl -s "http://127.0.0.1:$port/status" | grep -q '"done": true'; then done=1; break; fi
  sleep 0.1
done
# Before the check, as in cold-io.sh: the trap must kill the server a failure leaves running,
# or its `wait` on ebs-run.sh never returns.
spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1 || true)
[ $done = 1 ] || { echo "the warm-up did not end within 300 s" >&2; cat "$log" >&2; exit 2; }
echo "mem=$mem warm=$(curl -s "http://127.0.0.1:$port/status" | python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin)["warm"]))')"
kill "$spid"; wait "$sp" 2>/dev/null || true
spid="" sp=""
grep -E "warm-up|^ebs " "$log"
