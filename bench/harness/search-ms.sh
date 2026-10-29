#!/bin/bash
# Warm search time without the server or index open (root): `scout search --freshness off
# --stats` prints search_ms, the search alone. Each query runs 9 times per binary, two
# interleaved passes (A B A B), pinned to 2 CPUs with the index read into the page cache
# first; prints each pass's median. Resolves a few percent where serve-ab.sh's 2-rep warm
# pass cannot (E51's A/A: ±5% a rep), for changes that only cost CPU (E53).
# Usage: search-ms.sh <bin-dir-A> <bin-dir-B> <index-dir> <queries.tsv>
set -uo pipefail
[ "$#" = 4 ] || { echo "expected bin-dir-A bin-dir-B index-dir queries.tsv" >&2; exit 2; }
A=$1 B=$2 I=$3 Q=$4
find "$I" -type f -exec cat {} + > /dev/null
grep -v "^#" "$Q" | while IFS= read -r q; do
  # Tab is IFS whitespace, so `read` would collapse an empty flags field.
  name=$(cut -f1 <<< "$q"); flags=$(cut -f2 <<< "$q"); pat=$(cut -f3- <<< "$q")
  line="$name"
  for bin in "$A" "$B" "$A" "$B"; do
    ts=""
    for _ in 1 2 3 4 5 6 7 8 9; do
      # shellcheck disable=SC2086
      t=$(taskset -c 0,1 "$bin/scout" --index-dir "$I" search --freshness off --threads 2 --stats $flags -- "$pat" 2>&1 >/dev/null | grep -o 'search_ms=[0-9.]*' | cut -d= -f2)
      ts="$ts $t"
    done
    line="$line $(tr ' ' '\n' <<< "$ts" | grep . | sort -n | sed -n 5p)"
  done
  echo "$line"
done
