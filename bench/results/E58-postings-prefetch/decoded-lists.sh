#!/bin/bash
# Sizes of the posting lists each query decodes (throwaway logging build), bucketed:
# lists and their bytes at or under 8, 64, 512, 4096 bytes and over.
set -uo pipefail
B=/home/keegan/small/bin-listlog
I=/mnt/ebs/idx-e43-base
Q="/mnt/c/Users/User/source/test space/scout/bench/queries-big.tsv"
grep -v -e '^#' -e '^$' "$Q" | while IFS= read -r line; do
  name=${line%%$'\t'*} rest=${line#*$'\t'}
  flags=${rest%%$'\t'*} pat=${rest#*$'\t'}
  case $name in short_unindexable | huge_output) continue ;; esac
  # shellcheck disable=SC2086
  "$B/scout" --index-dir "$I" search --freshness off --threads 2 $flags -- "$pat" </dev/null 2>&1 >/dev/null | { grep '^LIST ' || true; } | awk -v n="$name" '
    { l = $2; t++; tb += l
      if (l <= 8) { a++; ab += l } else if (l <= 64) { c++; cb += l } else if (l <= 512) { d++; db += l }
      else if (l <= 4096) { e++; eb += l } else { f++; fb += l } }
    END { printf "%-18s lists %5d  <=8 %5d  <=64 %5d  <=512 %5d  <=4K %5d (%5.1f MiB)  >4K %4d (%6.1f MiB)\n",
          n, t, a, c, d, e, eb / 1048576, f, fb / 1048576 }'
done
