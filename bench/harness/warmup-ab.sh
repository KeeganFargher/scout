#!/bin/bash
# Interleaved A/B of the background warm-up (E45–E47) on the EBS-like box (root): one
# binary, `--warm-mb 0` against `--warm-mb <warm-mb>`, <reps> reps of warmup-ebs.sh per arm
# and per scenario, alternating which arm goes first, then warmupab.py pools them. A
# scenario is <wait-secs>:<gap-ms> (e.g. "0:0 45:0 0:300": a burst at ready, a burst 45 s
# later, queries 300 ms apart at ready). Both arms serve <index-dir> (see warmup-ebs.sh).
# E45–E47's first runs predate scenarios: "wait0" and "wait45" there are 0:0 and 45:0.
# <warm-mb> 0 makes it an A/A run (E49): both arms off, measuring the harness's noise.
# Usage: warmup-ab.sh <out-dir> <bin-dir> <index-dir> <mem> <queries.tsv> <warm-mb> <scenarios> <reps>
set -euo pipefail
[ "$#" = 8 ] || { echo "expected out-dir bin-dir index-dir mem queries warm-mb scenarios reps" >&2; exit 2; }
out=$1 B=$2 I=$3 mem=$4 Q=$5 WMB=$6 SCEN=$7 reps=$8
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$out" ] || { echo "$out exists" >&2; exit 2; }
for s in $SCEN; do
  [[ $s =~ ^[0-9]+:[0-9]+$ ]] || { echo "scenario $s is not <wait-secs>:<gap-ms>" >&2; exit 2; }
done
install -d -o 1000 -g 1000 "$out"
one() {
  local w=${3%%:*} g=${3##*:}
  bash "$P/warmup-ebs.sh" "$B" "$I" "$mem" "$Q" "$2" "$w" "$g" "$out/wait$w-gap$g/$1" > /dev/null
}
for r in $(seq "$reps"); do
  for s in $SCEN; do
    install -d -o 1000 -g 1000 "$out/wait${s%%:*}-gap${s##*:}"
    if [ $((r % 2)) = 1 ]; then one "$r-off" 0 "$s"; one "$r-on" "$WMB" "$s"; else one "$r-on" "$WMB" "$s"; one "$r-off" 0 "$s"; fi
  done
done
aa=(); [ "$WMB" != 0 ] || aa=(--aa)
python3 "$P/warmupab.py" "${aa[@]}" "$out" "$reps" $SCEN | tee "$out/summary.txt"
