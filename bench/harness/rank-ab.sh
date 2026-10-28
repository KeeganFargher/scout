#!/bin/bash
# Interleaved A/B of `scout search --rank --top <top>` on a warm index (2 CPUs by taskset):
# per query, both builds' output at --top 200 must be identical, then <runs> alternating
# runs each, median wall ms. Queries as queries-big.tsv (name, flags, pattern).
# Usage: rank-ab.sh <scout-A> <scout-B> <index-dir> <queries.tsv> <top> <runs> <out-file>
set -euo pipefail
[ "$#" = 7 ] || { echo "expected scout-A scout-B index-dir queries top runs out-file" >&2; exit 2; }
A=$1 B=$2 I=$3 Q=$4 TOP=$5 N=$6 OUT=$7
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
cat "$I"/shards/*.native > /dev/null
s() { local bin=$1; shift; taskset -c 0,1 "$bin" --index-dir "$I" search --threads 2 --freshness off "$@"; }
ms() { local st; st=$(date +%s%N); s "$@" > /dev/null; echo $(( ($(date +%s%N) - st) / 1000000 )); }
med() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
echo "query identical a_ms b_ms b/a" > "$OUT"
grep -v '^#' "$Q" | while IFS=$'\t' read -r name flags pat; do
  [ -n "$pat" ] || { pat=$flags; flags=""; }
  # shellcheck disable=SC2086
  same=$(cmp -s <(s "$A" --rank --top 200 $flags -- "$pat") <(s "$B" --rank --top 200 $flags -- "$pat") && echo yes || echo NO)
  ta=() tb=()
  for r in $(seq "$N"); do
    # shellcheck disable=SC2086
    if [ $((r % 2)) = 1 ]; then ta+=("$(ms "$A" --rank --top "$TOP" $flags -- "$pat")"); tb+=("$(ms "$B" --rank --top "$TOP" $flags -- "$pat")")
    else tb+=("$(ms "$B" --rank --top "$TOP" $flags -- "$pat")"); ta+=("$(ms "$A" --rank --top "$TOP" $flags -- "$pat")"); fi
  done
  a=$(printf '%s\n' "${ta[@]}" | med) b=$(printf '%s\n' "${tb[@]}" | med)
  echo "$name $same $a $b $(awk -v a="$a" -v b="$b" 'BEGIN {printf "%.3f", b / a}')" >> "$OUT"
done
awk 'NR > 1 {l += log($5); n++} END {printf "geomean b/a %.3f over %d queries\n", exp(l / n), n}' "$OUT" >> "$OUT"
cat "$OUT"
