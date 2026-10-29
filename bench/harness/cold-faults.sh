#!/bin/bash
# Where a cold `serve` query waits (root), at 2 CPUs / 4 GiB / gp3's caps (E58): per query,
# twice, the index reclaimed from the server's cgroup, then the request timed with the
# volume's reads, reads per second, average queue depth (time_in_queue / wall) and busy
# fraction (io_ticks / wall) from /sys/block/<dm>/stat. These are the whole volume's, so
# anything else reading it shows up too (a busy fraction over 1 is such a run). The first
# run of each query is under `perf record`, and after the last, each query's reads are
# counted by where they were issued: under `madvise` (queued, no wait) or under a page
# fault (a wait), with the first scout frame. Reads per second near 3000 mean the IOPS cap
# binds, and only fewer reads help; well under it with a queue depth near 1 means the
# query waits on one read at a time.
# Usage: cold-faults.sh <bin-dir> <index-dir> <queries.tsv> <out-dir>
set -euo pipefail
[ "$#" = 4 ] || { echo "expected bin-dir index-dir queries.tsv out-dir" >&2; exit 2; }
B=$1 I=$2 Q=$3 OUT=$4
P=$(cd "$(dirname "$0")" && pwd)
# /usr/bin/perf is a wrapper that refuses WSL's kernel; any linux-tools build reads its events.
PERF=$(ls -d /usr/lib/linux-tools/*/perf | head -1)
[ -x "$PERF" ] || { echo "no perf under /usr/lib/linux-tools" >&2; exit 2; }
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
mkdir -p "$OUT"
port=18600
dm=$(basename "$(readlink -f /dev/mapper/ebs)")
! curl -s -o /dev/null "http://127.0.0.1:$port/status" || { echo "port $port is in use" >&2; exit 2; }
spid="" sp=""
trap 'rc=$?; [ -z "$spid" ] || kill "$spid" 2>/dev/null || true; [ -z "$sp" ] || wait "$sp" 2>/dev/null || true; exit $rc' EXIT
sync; echo 3 > /proc/sys/vm/drop_caches
bash "$P/ebs-run.sh" 2 4G 125 3000 "$B/scout" --index-dir "$I" serve --config /home/keegan/small/serve.toml \
  --listen "127.0.0.1:$port" --max-output-mb 4096 --search-timeout-secs 900 --warm-mb 0 > /dev/null 2>&1 &
sp=$!
up=0
for _ in $(seq 600); do
  if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
  sleep 0.05
done
[ $up = 1 ] || { echo "serve did not become ready" >&2; exit 2; }
spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1)
cg=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
echo "query ms reads reads_per_s queue_depth busy" > "$OUT/cold.txt"
grep -v -e '^#' -e '^$' "$Q" | while IFS= read -r line; do
  # As cold-io.sh: split by hand, since `read` with a tab IFS folds an empty flags field.
  name=${line%%$'\t'*} rest=${line#*$'\t'}
  flags=${rest%%$'\t'*} pat=${rest#*$'\t'}
  [ "$name" != "$line" ] && [ "$flags" != "$rest" ] || { echo "not name<TAB>flags<TAB>pattern: $line" >&2; exit 2; }
  params="n=1"
  for f in $flags; do
    case $f in -F | -i | -S | -w | -c | -l) params+="&${f#-}=1" ;; *) echo "$name: unknown flag $f" >&2; exit 2 ;; esac
  done
  for rep in 1 2; do
    for _ in 1 2 3 4 5; do
      echo 64G > "$cg/memory.reclaim" 2>/dev/null || true
      left=$(python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import pagecache; print(int(pagecache.residency(sys.argv[2])[0]))' "$P" "$I")
      [ "$left" -gt 2 ] || break
    done
    [ "$left" -le 2 ] || { echo "$name: $left MiB of the index still cached, not cold" >&2; exit 2; }
    pp=""
    if [ $rep = 1 ]; then
      "$PERF" record -q -g -e block:block_rq_issue -p "$spid" -o "$OUT/$name.data" > /dev/null 2>&1 &
      pp=$!
      # perf attaches asynchronously; a request sent before it has would go uncounted.
      sleep 0.5
    fi
    s0=($(cat "/sys/block/$dm/stat")) t0=$(date +%s%N)
    curl -s --fail -G "http://127.0.0.1:$port/search?$params" --data-urlencode "q=$pat" > /dev/null
    t1=$(date +%s%N) s1=($(cat "/sys/block/$dm/stat"))
    [ -z "$pp" ] || { kill -INT "$pp"; wait "$pp" 2>/dev/null || true; }
    awk -v n="$name" -v w=$(( (t1 - t0) / 1000000 )) -v r=$(( s1[0] - s0[0] )) -v q=$(( s1[10] - s0[10] )) \
      -v u=$(( s1[9] - s0[9] )) 'BEGIN { printf "%s %d %d %.0f %.2f %.2f\n", n, w, r, r * 1000 / w, q / w, u / w }' >> "$OUT/cold.txt"
  done
done
kill "$spid"; wait "$sp" 2>/dev/null || true
spid="" sp=""
for f in "$OUT"/*.data; do
  echo "== $(basename "$f" .data)"
  "$PERF" script -i "$f" -F ip,sym 2>/dev/null | awk '
    # A sample is its stack, one frame per line, then a blank line.
    NF == 0 { if (n) print (k == "" ? "other" : k) "\t" u; n = 0; k = ""; u = ""; next }
    {
      n = 1; $1 = ""; s = substr($0, 2)
      if (k == "" && s ~ /^(do_madvise|filemap_fault)/) k = s
      if (u == "" && s ~ /scout/) u = s
    }
    END { if (n) print (k == "" ? "other" : k) "\t" u }' | sed -E 's/::h[0-9a-f]{16}//g' | sort | uniq -c | sort -rn | head -8
done > "$OUT/issued.txt"
rm -f "$OUT"/*.data
cat "$OUT/cold.txt" "$OUT/issued.txt"
