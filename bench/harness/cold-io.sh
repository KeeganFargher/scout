#!/bin/bash
# Where a cold `serve` query's time goes on the EBS-like box (root): one server per IOPS cap
# (e.g. "3000 max": gp3's 3000, then none), and for each query the index reclaimed from the
# server's cgroup, then the request timed with the volume's reads (bios) and bytes from the
# cgroup's io.stat. Equal times at both caps mean bytes or CPU bound the query; a large gap
# means the number of reads does (E42). The server runs without its background warm-up
# (E47), which would refill what each reclaim evicts; binaries before E47 refuse the flag.
# Usage: cold-io.sh <bin-dir> <index-dir> <mem> <queries.tsv> <iops-caps> <out-file>
set -euo pipefail
[ "$#" = 6 ] || { echo "expected bin-dir index-dir mem queries iops-caps out-file" >&2; exit 2; }
B=$1 I=$2 mem=$3 Q=$4 CAPS=$5 OUT=$6
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
dev=$(cat "/sys/block/$(basename "$(readlink -f /dev/mapper/ebs)")/dev")
port=18600
# MiB of the index left in the page cache after a reclaim; more means the query would not
# start cold (qbench.py's COLD_SLACK, 2 MiB).
cold_slack_mib=2
# A server still holding the port would answer for this run's binary: refuse instead.
! curl -s -o /dev/null "http://127.0.0.1:$port/status" || { echo "port $port is in use" >&2; exit 2; }
spid="" sp=""
# Stops a server left running by a failure, and waits for it so none outlives the script. It
# keeps the failure's exit status: set -e still holds in the trap, so the killed server's
# status from `wait` (143) would otherwise end the script before `exit $rc`.
trap 'rc=$?; [ -z "$spid" ] || kill "$spid" 2>/dev/null || true; [ -z "$sp" ] || wait "$sp" 2>/dev/null || true; exit $rc' EXIT
echo "query iops_cap ms rios rmib avg_kib_per_read" > "$OUT"
for iops in $CAPS; do
  sync; echo 3 > /proc/sys/vm/drop_caches
  bash "$P/ebs-run.sh" 2 "$mem" 125 "$iops" "$B/scout" --index-dir "$I" serve --config /home/keegan/small/serve.toml \
    --listen "127.0.0.1:$port" --max-output-mb 4096 --search-timeout-secs 900 --warm-mb 0 > /dev/null 2>&1 &
  sp=$!
  up=0
  for _ in $(seq 600); do
    if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
    sleep 0.05
  done
  spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1)
  [ $up = 1 ] || { echo "serve did not become ready at iops=$iops" >&2; exit 2; }
  cg=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
  io() { grep "^$dev " "$cg/io.stat" | tr ' ' '\n' | awk -F= -v k="$1" '$1==k{print $2}'; }
  grep -v -e '^#' -e '^$' "$Q" | while IFS= read -r line; do
    # Three tab-separated fields, flags possibly empty: split by hand, since `read` with a
    # tab IFS would fold an empty field into its neighbour.
    name=${line%%$'\t'*} rest=${line#*$'\t'}
    flags=${rest%%$'\t'*} pat=${rest#*$'\t'}
    [ "$name" != "$line" ] && [ "$flags" != "$rest" ] || { echo "not name<TAB>flags<TAB>pattern: $line" >&2; exit 2; }
    params="n=1"
    for f in $flags; do
      # qbench.py's FLAG_PARAM: the flag's letter is the parameter, for these flags only.
      case $f in -F | -i | -S | -w | -c | -l) params+="&${f#-}=1" ;; *) echo "$name: unknown flag $f" >&2; exit 2 ;; esac
    done
    # One reclaim can leave pages the server touched last (they are on the active list), so
    # reclaim until the residue is cold, up to 5 times. EAGAIN only says less than 64G was
    # reclaimable; the residue check decides.
    for _ in 1 2 3 4 5; do
      echo 64G > "$cg/memory.reclaim" 2>/dev/null || true
      left=$(python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import pagecache; print(int(pagecache.residency(sys.argv[2])[0]))' "$P" "$I")
      [ "$left" -gt "$cold_slack_mib" ] || break
    done
    [ "$left" -le "$cold_slack_mib" ] || { echo "$name: $left MiB of the index still cached, not cold" >&2; exit 2; }
    r0=$(io rios) b0=$(io rbytes) t0=$(date +%s%N)
    curl -s --fail -G "http://127.0.0.1:$port/search?$params" --data-urlencode "q=$pat" > /dev/null
    ms=$(( ($(date +%s%N) - t0) / 1000000 )) r=$(( $(io rios) - r0 )) b=$(( $(io rbytes) - b0 ))
    echo "$name $iops $ms $r $((b / 1048576)) $(( r > 0 ? b / 1024 / r : 0 ))" >> "$OUT"
  done
  kill "$spid"; wait "$sp" 2>/dev/null || true
  spid="" sp=""
done
cat "$OUT"
