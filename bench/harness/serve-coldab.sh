#!/bin/bash
# Interleaved A/B of `scout serve`, warm and honestly cold (qbench.py), per profile.
#   serve-coldab.sh <label> <armA>=<bin-dir>[@<index-dir>] <armB>=<bin-dir>[@<index-dir>] <reps> <warm-rounds> <cold-rounds> [profiles...]
# An arm serves its own index when it names one (an index format change), otherwise
# run-e21.sh's private copy. Either must already hold serve-e21.toml's mirrored repo, so no
# rep's first cycle indexes it while another rep's does not (prime a fresh copy of a bench
# index with one serve run first). One server at a time; reps alternate which arm goes
# first so drift hits both equally. The output directory ~/small/ab/<label> must not exist
# yet: coldabsum.py summarises every run in it, so an earlier run's would pool with these.
# `--warm-mb 0`: a background warm-up (E47) would refill what the cold rounds reclaim;
# binaries before E47 refuse the flag.
set -euo pipefail
label=$1 a=$2 b=$3 reps=$4 WR=$5 CR=$6; shift 6
profiles=("$@"); [ ${#profiles[@]} -eq 0 ] && profiles=("1:2G" "4:4G")
H=~/small; P=$(cd "$(dirname "$0")" && pwd); OUT=$H/ab/$label
[ -f "$H/serve-e21.toml" ] || { echo "missing serve-e21.toml (run run-e21.sh first)" >&2; exit 2; }
if [ -e "$OUT" ]; then echo "$OUT exists: its runs would pool with these; move it away first" >&2; exit 2; fi
mkdir -p "$OUT"
port=18400

evict() { # $1 = bin dir, $2 = index dir; nothing may map the index while its pages are dropped
  if pgrep -f -- "--index-dir $2" > /dev/null; then echo "a scout still has $2 open" >&2; exit 2; fi
  sync
  python3 "$P/pagecache.py" evict "$2" > /dev/null
  python3 "$P/pagecache.py" evict "$1" > /dev/null
}

one() { # arm=bin[@idx] profile rep
  local arm=${1%%=*} B=${1#*=} c=${2%%:*} m=${2##*:} r=$3 I=$H/idx-e21-serve
  if [[ "$B" == *@* ]]; then I=${B#*@}; B=${B%%@*}; fi
  [ -d "$I" ] || { echo "missing $I" >&2; exit 2; }
  local tag="$arm-${c}cpu-$r"
  evict "$B" "$I"
  "$P/prof.sh" "$c" "$m" "$B/scout" --index-dir "$I" serve --config "$H/serve-e21.toml" --listen "127.0.0.1:$port" \
    --max-output-mb 4096 --search-timeout-secs 900 --warm-mb 0 > "$OUT/serve-$tag.log" 2>&1 &
  local sp=$! up=0
  for _ in $(seq 300); do
    if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
    sleep 0.2
  done
  [ $up = 1 ] || { echo "serve did not become ready on $tag" >&2; cat "$OUT/serve-$tag.log" >&2; exit 2; }
  # A cycle that reindexed means the index lacked the mirrored repo: this rep would differ.
  if ! curl -s "http://127.0.0.1:$port/status" | python3 -c 'import json, sys; sys.exit(bool(json.load(sys.stdin)["last_cycle"]["reindexed"]))'; then
    echo "$tag: the first cycle reindexed; prime $I first" >&2; pkill -f -- "^$B/scout --index-dir $I serve"; exit 2
  fi
  local spid; spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1)
  local cg=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
  # Cold first: the server has just started on an evicted index, the production cold case.
  taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$cg" "$I" "$H/queries.tsv" cold "$CR" "$OUT/cold-$tag.json"
  taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$cg" "$I" "$H/queries.tsv" warm "$WR" "$OUT/warm-$tag.json"
  pkill -f -- "^$B/scout --index-dir $I serve"; wait "$sp" 2>/dev/null || true
  port=$((port + 1))
}

for prof in "${profiles[@]}"; do
  for r in $(seq "$reps"); do
    if [ $((r % 2)) = 1 ]; then one "$a" "$prof" "$r"; one "$b" "$prof" "$r"; else one "$b" "$prof" "$r"; one "$a" "$prof" "$r"; fi
  done
done
python3 "$P/coldabsum.py" "$OUT" "${a%%=*}" "${b%%=*}" | tee "$OUT/summary.txt"
