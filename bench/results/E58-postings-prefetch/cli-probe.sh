#!/bin/bash
# Probe (root): for cold CLI searches at 2 CPUs / 4 GiB / gp3 caps, how busy is the volume?
# From /sys/block/<dm>/stat around each run: reads, read rate over the whole run, average
# queue depth (time_in_queue / wall) and utilisation (io_ticks / wall).
set -euo pipefail
H="/mnt/c/Users/User/source/test space/scout/bench/harness"
Q="/mnt/c/Users/User/source/test space/scout/bench/queries-big.tsv"
B=/home/keegan/small/bin-head
I=/mnt/ebs/idx-e43-base
dm=$(basename "$(readlink -f /dev/mapper/ebs)")
echo "# query iops wall_ms search_ms reads r/s aqu util"
for iops in 3000 max; do
for name in llvm_regex smart_case literal_none selective_symbol selective_long anchored alternation; do
  line=$(grep -P "^$name\t" "$Q")
  flags=$(cut -f2 <<< "$line"); pat=$(cut -f3- <<< "$line")
  for rep in 1 2; do
    sync; echo 3 > /proc/sys/vm/drop_caches
    s0=($(cat /sys/block/$dm/stat)); t0=$(date +%s%N)
    # shellcheck disable=SC2086
    st=$(bash "$H/ebs-run.sh" 2 4G 125 "$iops" "$B/scout" --index-dir "$I" search --freshness off --threads 2 --stats $flags -- "$pat" 2>&1 >/dev/null || true)
    t1=$(date +%s%N); s1=($(cat /sys/block/$dm/stat))
    sm=$(grep -o 'search_ms=[0-9.]*' <<< "$st" | cut -d= -f2)
    [ -n "$sm" ] || { echo "$name: no search_ms: $st" >&2; exit 1; }
    awk -v n="$name" -v c="$iops" -v w=$(( (t1 - t0) / 1000000 )) -v sm="$sm" \
        -v r=$(( s1[0] - s0[0] )) -v q=$(( s1[10] - s0[10] )) -v u=$(( s1[9] - s0[9] )) \
        'BEGIN { printf "%s %s %d %s %d %.0f %.2f %.2f\n", n, c, w, sm, r, r * 1000 / w, q / w, u / w }'
  done
done
done
