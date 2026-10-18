#!/bin/bash
# Interleaved A/B on the small-server harness: rounds alternate which arm goes first, so
# drift hits both equally; both arms search the same index unless an arm names its own
# (an index format change): <arm>=<bin-dir>@<index-dir>.
#   ab.sh <out-label> <index-dir> <armA>=<bin-dir>[@idx] <armB>=<bin-dir>[@idx] <rounds> <runs> [profiles...]
# The output directory ~/small/ab/<out-label> must not exist yet: abagg.py summarises every
# run in it, so an earlier run's files would pool with this one's.
set -euo pipefail
label=$1 idx=$2 a=$3 b=$4 rounds=$5 runs=$6; shift 6
profiles=("$@"); [ ${#profiles[@]} -eq 0 ] && profiles=("1:2G" "4:4G" "12:infinity")
H=~/small; P=$(cd "$(dirname "$0")" && pwd); OUT=$H/ab/$label
if [ -e "$OUT" ]; then echo "$OUT exists: its runs would pool with these; move it away first" >&2; exit 2; fi
mkdir -p "$OUT"
run() { # arm=bin profile round
  local arm=${1%%=*} B=${1#*=} prof=$2 r=$3 I=$idx
  if [[ "$B" == *@* ]]; then I=${B#*@}; B=${B%%@*}; fi
  local c=${prof%%:*} m=${prof##*:}
  "$P/prof.sh" "$c" "$m" "$B/scout-bench" --scout "$B/scout" --index-dir "$I" --queries "$H/queries.tsv" \
    --runs "$runs" --rg-runs 1 --rg "$H/bin/rg" --scout-flags="--freshness off" --label "$arm" \
    --json "$OUT/$arm-${c}cpu-$r.json" > /dev/null 2> "$OUT/$arm-${c}cpu-$r.err"
}
for prof in "${profiles[@]}"; do
  for r in $(seq "$rounds"); do
    if [ $((r % 2)) = 1 ]; then run "$a" "$prof" "$r"; run "$b" "$prof" "$r"; else run "$b" "$prof" "$r"; run "$a" "$prof" "$r"; fi
  done
done
python3 "$P/abagg.py" "$OUT" "${a%%=*}" "${b%%=*}" | tee "$OUT/summary.txt"
