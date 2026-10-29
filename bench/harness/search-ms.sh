#!/bin/bash
# Warm search time without the server or index open (root): `scout search --freshness off
# --stats` prints search_ms, the search alone. Each query runs 9 times per binary, two
# interleaved passes (A B A B), pinned to <cpus> CPUs with --threads <cpus>, the index read
# into the page cache first; prints each pass's median. Resolves a few percent where
# serve-ab.sh's 2-rep warm pass cannot (E51's A/A: ±5% a rep), for changes that only cost
# CPU (E53). Fails on any run that does not report search_ms, and on any query whose two
# binaries print different output.
# Usage: search-ms.sh <bin-dir-A> <bin-dir-B> <index-dir> <queries.tsv> [cpus, default 2]
set -euo pipefail
[ "$#" = 4 ] || [ "$#" = 5 ] || { echo "expected bin-dir-A bin-dir-B index-dir queries.tsv [cpus]" >&2; exit 2; }
A=$1 B=$2 I=$3 Q=$4 CPUS=${5:-2}
NPROC=$(nproc)
[ "$CPUS" -ge 1 ] && [ "$CPUS" -le "$NPROC" ] || { echo "cpus must be 1..$NPROC" >&2; exit 2; }
PIN="0-$((CPUS - 1))"
find "$I" -type f -exec cat {} + > /dev/null
OUTA=$(mktemp) OUTB=$(mktemp)
trap 'rm -f "$OUTA" "$OUTB"' EXIT
grep -v "^#" "$Q" | while IFS= read -r q; do
  # Tab is IFS whitespace, so `read` would collapse an empty flags field.
  name=$(cut -f1 <<< "$q"); flags=$(cut -f2 <<< "$q"); pat=$(cut -f3- <<< "$q")
  # Same output from both binaries, or the timing compares different work. Exit 1 is
  # "no match", as rg's.
  # shellcheck disable=SC2086
  "$A/scout" --index-dir "$I" search --freshness off --threads "$CPUS" $flags -- "$pat" > "$OUTA" || [ $? = 1 ]
  # shellcheck disable=SC2086
  "$B/scout" --index-dir "$I" search --freshness off --threads "$CPUS" $flags -- "$pat" > "$OUTB" || [ $? = 1 ]
  cmp -s "$OUTA" "$OUTB" || { echo "$name: output differs between binaries" >&2; exit 1; }
  line="$name"
  for bin in "$A" "$B" "$A" "$B"; do
    ts=""
    for _ in 1 2 3 4 5 6 7 8 9; do
      # shellcheck disable=SC2086
      st=$(taskset -c "$PIN" "$bin/scout" --index-dir "$I" search --freshness off --threads "$CPUS" --stats $flags -- "$pat" 2>&1 >/dev/null || true)
      t=$(grep -o 'search_ms=[0-9.]*' <<< "$st" | cut -d= -f2 || true)
      [ -n "$t" ] || { echo "$name: no search_ms from $bin: $st" >&2; exit 1; }
      ts="$ts $t"
    done
    line="$line $(tr ' ' '\n' <<< "$ts" | grep . | sort -n | sed -n 5p)"
  done
  echo "$line"
done
