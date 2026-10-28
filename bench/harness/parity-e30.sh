#!/bin/bash
# The bench parity gate (scout output == rg output) for the E30-E32 build on the bench
# corpus (rootless): v9 indexes raw and lz4 at 12 CPUs, then scout-bench on each at 1 CPU /
# 2 GiB and 12 CPUs, which fails on any query whose output differs from rg's.
# Usage: parity-e30.sh <bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 2 ] || { echo "expected bin-dir out-dir" >&2; exit 2; }
B=$1 OUT=$2 H=~/small P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
mkdir -p "$OUT"
# Any index that registers the 137 bench repos lists them.
LISTED=$H/idx-e24-12cpu-infinity
mapfile -t ROOTS < <(python3 -c 'import json, sys; [print(r["root"]) for r in json.load(open(sys.argv[1]))["repos"]]' "$LISTED/catalog.json")
[ ${#ROOTS[@]} -gt 0 ] || { echo "no repos read from $LISTED/catalog.json" >&2; exit 2; }
for c in raw lz4; do
  I=$OUT/idx-$c
  "$B/scout" --index-dir "$I" add "${ROOTS[@]}" > /dev/null
  "$P/prof.sh" 12 infinity "$B/scout" --index-dir "$I" index --content "$c" 2>&1 | grep -E "done in|content"
  for prof in 1:2G 12:infinity; do
    "$P/prof.sh" "${prof%%:*}" "${prof##*:}" "$B/scout-bench" --scout "$B/scout" --index-dir "$I" \
      --queries "$H/queries.tsv" --runs 3 --rg-runs 1 --rg "$H/bin/rg" --scout-flags="--freshness off" \
      --label "$c-${prof%%:*}cpu" --json "$OUT/$c-${prof%%:*}cpu.json" > "$OUT/$c-${prof%%:*}cpu.out" 2> "$OUT/$c-${prof%%:*}cpu.err"
    echo "$c ${prof}: exit 0"
  done
  rm -rf "$I"
done
