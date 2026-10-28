#!/bin/bash
# E43 (every block lz4 shrinks stored compressed, small ones too) on the EBS-like box
# (root): builds chromium + llvm-project with --content lz4 by each binary at 2 CPUs /
# 4 GiB, runs the bench parity gate on the new one, then serve-ab.sh with each arm on its
# own index and cold-io.sh on each index at gp3's IOPS cap (one pass, base first).
# Usage: run-e43.sh <base-bin-dir> <new-bin-dir> <out-dir>
set -euo pipefail
[ "$#" = 3 ] || { echo "expected base-bin-dir new-bin-dir out-dir" >&2; exit 2; }
A=$1 B=$2 OUT=$3 P=$(cd "$(dirname "$0")" && pwd)
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
for arm in base:$A small:$B; do
  n=${arm%%:*} bin=${arm#*:} I=/mnt/ebs/idx-e43-$n
  rm -rf "$I"
  su keegan -c "'$bin/scout' --index-dir '$I' add /mnt/ebs/big/chromium /mnt/ebs/big/llvm-project" > /dev/null
  sync; echo 3 > /proc/sys/vm/drop_caches
  bash "$P/ebs-run.sh" 2 4G 125 3000 "$bin/scout" --index-dir "$I" index --content lz4 > "$OUT/index-$n.log" 2>&1
  echo "$n: $(grep -E 'done in' "$OUT/index-$n.log" | tail -1) size=$(du -sm "$I/shards" | cut -f1) MiB" | tee -a "$OUT/index.txt"
done
su keegan -c "bash '$P/parity-e30.sh' '$B' /home/keegan/small/parity-e43" > "$OUT/parity.log" 2>&1
grep -h "parity" /home/keegan/small/parity-e43/*.out | grep -v "^|" | tee "$OUT/parity.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A@/mnt/ebs/idx-e43-base" "small=$B@/mnt/ebs/idx-e43-small" /mnt/ebs/idx-e43-base 4G /tmp/q18.tsv 2 3 1
for n in base small; do
  bash "$P/cold-io.sh" "$([ $n = base ] && echo "$A" || echo "$B")" "/mnt/ebs/idx-e43-$n" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-$n.txt" > /dev/null
done
