#!/bin/bash
# A/B of two builds on the EBS-like volume (run as root: wsl.exe -u root):
#   ab-ebs.sh <out-dir> <nameA>=<bin-dir>[@<index-dir>] <nameB>=<bin-dir>[@<index-dir>] <index-dir> <cpus> <mem> <delay_ms> <reps> <warm-rounds> <cold-rounds>
# Each rep runs run-ebs.sh for both arms, alternating which goes first, into
# <out-dir>/<rep>-<arm>/. An arm that names an index (a format change) searches it instead of
# <index-dir>. Then ebsab.py pools the reps per arm.
set -euo pipefail
out=$1 a=$2 b=$3 idx=$4 cpus=$5 mem=$6 delay=$7 reps=$8 wr=$9 cr=${10}
P=$(cd "$(dirname "$0")" && pwd)
[ -e "$out" ] && { echo "$out exists: move it away first" >&2; exit 2; }
install -d -o 1000 -g 1000 "$out"
one() { # arm rep
  local name=${1%%=*} bin=${1#*=} i=$idx
  if [[ "$bin" == *@* ]]; then i=${bin#*@}; bin=${bin%%@*}; fi
  bash "$P/run-ebs.sh" "$out/$2-$name" "$bin" "$i" "$cpus" "$mem" "$delay" "$wr" "$cr" > /dev/null
}
for r in $(seq "$reps"); do
  if [ $((r % 2)) = 1 ]; then one "$a" "$r"; one "$b" "$r"; else one "$b" "$r"; one "$a" "$r"; fi
done
python3 "$P/ebsab.py" "$out" "${a%%=*}" "${b%%=*}" | tee "$out/summary.txt"
