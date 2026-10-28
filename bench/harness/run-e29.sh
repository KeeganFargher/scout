#!/bin/bash
# E29 on the bench corpus (rootless): builds the v6 bench index raw and lz4 at 12 CPUs, as
# every bench index was built, then interleaved A/Bs (scout-bench, so rg parity is checked
# on every run) of e26 on the v5 index against e29 raw — the format and E28's prefetch, at
# the sizes where --content auto keeps raw — and e29 raw against e29 lz4.
set -euo pipefail
H=~/small P=$(cd "$(dirname "$0")" && pwd)
V5=$H/idx-e24-12cpu-infinity
mapfile -t ROOTS < <(python3 -c 'import json, sys; [print(r["root"]) for r in json.load(open(sys.argv[1]))["repos"]]' "$V5/catalog.json")
[ ${#ROOTS[@]} -gt 0 ] || { echo "no repos read from $V5/catalog.json" >&2; exit 2; }
# Read every bench repo once outside the scopes, as every bench index was built; rg's
# exit 1 is "no match", anything else a failed read.
rc=0; "$H/bin/rg" -c zzqqxxnevermatches "${ROOTS[@]}" > /dev/null || rc=$?
[ $rc = 1 ] || { echo "rg exited $rc warming the corpus" >&2; exit 2; }
for c in raw lz4; do
  I=$H/idx-e29-$c
  rm -rf "$I"
  "$H/bin-e29/scout" --index-dir "$I" add "${ROOTS[@]}" > /dev/null
  "$P/prof.sh" 12 infinity "$H/bin-e29/scout" --index-dir "$I" index --content "$c" 2>&1 | grep -E "done in|content"
  du -sh "$I"
done
"$P/ab.sh" e29-format "$V5" e26="$H/bin-e26" e29raw="$H/bin-e29@$H/idx-e29-raw" 4 10 "1:2G" "4:4G" "12:infinity"
"$P/ab.sh" e29-codec "$H/idx-e29-raw" e29raw="$H/bin-e29" e29lz4="$H/bin-e29@$H/idx-e29-lz4" 4 10 "1:2G" "4:4G" "12:infinity"
