#!/bin/bash
# Interleaved A/B of two `scout serve` builds on the EBS-like box (root): <reps> reps of
# serve-ebs.sh per arm, alternating which goes first, then serveab.py pools them. An arm
# may name its own index (`<name>=<bin-dir>@<index-dir>`) when the arms differ in what
# they write, such as the content codec (E38); otherwise both serve <index-dir>.
# SERVE_FLAGS_A and SERVE_FLAGS_B are each arm's extra server flags (serve-ebs.sh's
# SERVE_FLAGS), so one build can be compared with itself under other flags (E60).
# Usage: serve-ab.sh <out-dir> <nameA>=<bin-dir>[@<index-dir>] <nameB>=<bin-dir>[@<index-dir>] <index-dir> <mem> <queries.tsv> <reps> <warm-rounds> <cold-rounds>
set -euo pipefail
[ "$#" = 9 ] || { echo "expected out-dir armA armB index-dir mem queries reps warm-rounds cold-rounds" >&2; exit 2; }
out=$1 a=$2 b=$3 idx=$4 mem=$5 q=$6 reps=$7 wr=$8 cr=$9
P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$out" ] || { echo "$out exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$out"
one() {
  # Split at the last @, so a bin path may contain one.
  local spec=${1#*=} bin=${1#*=} i=$idx
  if [ "${spec%@*}" != "$spec" ]; then
    bin=${spec%@*} i=${spec##*@}
    [ -n "$i" ] || { echo "empty index in $1" >&2; exit 2; }
  fi
  SERVE_FLAGS=$3 bash "$P/serve-ebs.sh" "$bin" "$i" "$mem" "$q" "$wr" "$cr" "$out/$2-${1%%=*}" > /dev/null
}
fa=${SERVE_FLAGS_A:-} fb=${SERVE_FLAGS_B:-}
for r in $(seq "$reps"); do
  if [ $((r % 2)) = 1 ]; then one "$a" "$r" "$fa"; one "$b" "$r" "$fb"; else one "$b" "$r" "$fb"; one "$a" "$r" "$fa"; fi
done
python3 "$P/serveab.py" "$out" "${a%%=*}" "${b%%=*}" | tee "$out/summary.txt"
