#!/bin/bash
# Where a wall-clock A/B's (ab.sh) gap lives, for the queries it read slower: per query, in
# a small-server profile, `scout search --stats` gives the search's own work (candidates,
# postings, verification) and, around the same run, the process's wall time with its
# output piped, as scout-bench reads it (start, output and exit included). Each profile and
# query runs in one scope: 3 untimed runs of each arm, then <runs> pairs whose order
# alternates, so drift and going second hit both arms equally. Arms name their index
# (<name>=<bin-dir>@<index-dir>), as an index format change needs.
#   stats-ab.sh <out-label> <runs> <armA> <armB> <profiles> <query>...
# <profiles> is a comma-separated list such as 4:4G,12:infinity, and the queries are names
# in ~/small/queries.tsv. The output directory ~/small/ab/<out-label> must not exist yet:
# statsabsum.py summarises every run in it.
set -euo pipefail
[ $# -ge 6 ] || { sed -n 9p "$0" >&2; exit 2; }
label=$1 runs=$2 a=$3 b=$4 profiles=$5; shift 5
H=~/small; P=$(cd "$(dirname "$0")" && pwd); OUT=$H/ab/$label
[ "$runs" -ge 2 ] || { echo "runs must be at least 2 (the summary takes quartiles)" >&2; exit 2; }
for arm in "$a" "$b"; do
  spec=${arm#*=}
  [[ "$arm" == *=*@* ]] || { echo "$arm: an arm is <name>=<bin-dir>@<index-dir>" >&2; exit 2; }
  [ -x "${spec%%@*}/scout" ] || { echo "$arm: no scout in ${spec%%@*}" >&2; exit 2; }
  [ -d "${spec#*@}" ] || { echo "$arm: no index at ${spec#*@}" >&2; exit 2; }
done
if [ -e "$OUT" ]; then echo "$OUT exists: its runs would pool with these; move it away first" >&2; exit 2; fi
mkdir -p "$OUT"

# One scope's work. Prints "<arm> wall_us=<µs> <scout-stats line>" per timed run. The wall
# time spans the whole pipeline (the subshell, scout, cat reading its output, grep), the
# same overhead for both arms. $flags is unquoted on purpose: a query's flags field can
# hold several flags (`-c -F`).
inner='
  runs=$1 flags=$2 pattern=$3 a=$4 b=$5
  one() {
    local name=${1%%=*} spec=${1#*=} t0 t1 line
    t0=$EPOCHREALTIME
    line=$({ "${spec%%@*}/scout" --index-dir "${spec#*@}" search -n --stats --freshness off $flags -- "$pattern" | cat > /dev/null; } 2>&1 | grep "^scout-stats" || true)
    t1=$EPOCHREALTIME
    [ -n "$line" ] || { echo "$name printed no scout-stats line" >&2; exit 1; }
    echo "$name wall_us=$((${t1/./} - ${t0/./})) $line"
  }
  for i in 1 2 3; do one "$a" > /dev/null; one "$b" > /dev/null; done
  for i in $(seq "$runs"); do
    if [ $((i % 2)) = 1 ]; then one "$a"; one "$b"; else one "$b"; one "$a"; fi
  done'

IFS=, read -ra profs <<< "$profiles"
for prof in "${profs[@]}"; do
  c=${prof%%:*} m=${prof##*:}
  for name in "$@"; do
    # \037 between the fields, not a tab: a tab is IFS whitespace, so read would collapse
    # an empty flags field (alternation has none) and shift the pattern into it.
    row=$(awk -F'\t' -v n="$name" '$1 == n {print $2 "\037" $3}' "$H/queries.tsv")
    [ -n "$row" ] || { echo "$name is not in $H/queries.tsv" >&2; exit 2; }
    IFS=$'\037' read -r flags pattern <<< "$row"
    "$P/prof.sh" "$c" "$m" bash -c "$inner" _ "$runs" "$flags" "$pattern" "$a" "$b" \
      > "$OUT/$name-${c}cpu.txt" 2> "$OUT/$name-${c}cpu.err" \
      || { echo "$name at $prof failed: $OUT/$name-${c}cpu.err" >&2; exit 1; }
  done
done
python3 "$P/statsabsum.py" "$OUT" "${a%%=*}" "${b%%=*}" | tee "$OUT/summary.txt"
