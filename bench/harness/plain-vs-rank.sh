#!/bin/bash
# What ranking costs over the plain search, one build on a warm index (2 CPUs by taskset):
# per query, <runs> interleaved runs of the plain search and of `--rank --top <top>`
# (alternating which goes first), median wall ms each, and their ratio. Queries as queries-big.tsv (name, flags, pattern).
# Usage: plain-vs-rank.sh <scout> <index-dir> <queries.tsv> <top> <runs> <out-file>
set -euo pipefail
[ "$#" = 6 ] || { echo "expected scout index-dir queries top runs out-file" >&2; exit 2; }
S=$1 I=$2 Q=$3 TOP=$4 N=$5 OUT=$6
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
cat "$I"/shards/*.native > /dev/null
ms() { local st; st=$(date +%s%N); taskset -c 0,1 "$S" --index-dir "$I" search --threads 2 --freshness off "$@" > /dev/null; echo $(( ($(date +%s%N) - st) / 1000000 )); }
med() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
echo "query plain_ms rank${TOP}_ms rank/plain" > "$OUT"
grep -v '^#' "$Q" | while IFS=$'\t' read -r name flags pat; do
  [ -n "$pat" ] || { pat=$flags; flags=""; }
  p=() r=()
  for i in $(seq "$N"); do
    # Alternate which goes first, so neither always runs on the other's warm-up.
    # shellcheck disable=SC2086
    if [ $((i % 2)) = 1 ]; then p+=("$(ms $flags -- "$pat")"); r+=("$(ms --rank --top "$TOP" $flags -- "$pat")")
    else r+=("$(ms --rank --top "$TOP" $flags -- "$pat")"); p+=("$(ms $flags -- "$pat")"); fi
  done
  a=$(printf '%s\n' "${p[@]}" | med) b=$(printf '%s\n' "${r[@]}" | med)
  echo "$name $a $b $(awk -v a="$a" -v b="$b" 'BEGIN {printf "%.2f", b / a}')" >> "$OUT"
done
cat "$OUT"
