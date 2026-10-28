#!/bin/bash
# Interleaved A/B of two `scout serve` builds on the EBS-like box (root): <reps> reps of
# serve-ebs.sh per arm, alternating which goes first, then serveab.py pools them.
# Usage: serve-ab.sh <out-dir> <nameA>=<bin-dir> <nameB>=<bin-dir> <index-dir> <mem> <queries.tsv> <reps> <warm-rounds> <cold-rounds>
set -euo pipefail
[ "$#" = 9 ] || { echo "expected out-dir armA armB index-dir mem queries reps warm-rounds cold-rounds" >&2; exit 2; }
out=$1 a=$2 b=$3 idx=$4 mem=$5 q=$6 reps=$7 wr=$8 cr=$9
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$out" ] || { echo "$out exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$out"
one() { bash "$P/serve-ebs.sh" "${1#*=}" "$idx" "$mem" "$q" "$wr" "$cr" "$out/$2-${1%%=*}" > /dev/null; }
for r in $(seq "$reps"); do
  if [ $((r % 2)) = 1 ]; then one "$a" "$r"; one "$b" "$r"; else one "$b" "$r"; one "$a" "$r"; fi
done
python3 "$P/serveab.py" "$out" "${a%%=*}" "${b%%=*}" | tee "$out/summary.txt"
