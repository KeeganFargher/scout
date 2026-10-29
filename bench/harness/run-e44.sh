#!/bin/bash
# E44 (a per-shard lz4 dictionary for docs of one block) on the EBS-like box (root): builds
# chromium + llvm-project with --content lz4 by the new binary at 2 CPUs / 4 GiB, runs the
# bench parity gate on it, then serve-ab.sh with each arm on its own index and cold-io.sh on
# each index at gp3's IOPS cap (one pass, base first). The base index is E43's, written by
# the E42 writer, which E43 left unchanged. E43's serve runs had already synced a tiny repo
# into that index (serve-ebs.sh), so the arms do not start from equal indexes.
# Usage: run-e44.sh <base-bin-dir> <new-bin-dir> <base-index> <out-dir>
set -euo pipefail
[ "$#" = 4 ] || { echo "expected base-bin-dir new-bin-dir base-index out-dir" >&2; exit 2; }
A=$1 B=$2 BI=$3 OUT=$4 P=$(cd "$(dirname "$0")" && pwd) NI=/mnt/ebs/idx-e44-dict
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 2; }
[ -d "$BI/shards" ] || { echo "$BI has no shards" >&2; exit 2; }
install -d -o 1000 -g 1000 "$OUT"
rm -rf "$NI"
su keegan -c "'$B/scout' --index-dir '$NI' add /mnt/ebs/big/chromium /mnt/ebs/big/llvm-project" > /dev/null
sync; echo 3 > /proc/sys/vm/drop_caches
bash "$P/ebs-run.sh" 2 4G 125 3000 "$B/scout" --index-dir "$NI" index --content lz4 > "$OUT/index-dict.log" 2>&1
echo "base: size=$(du -sm "$BI/shards" | cut -f1) MiB (built for E43)" | tee -a "$OUT/index.txt"
echo "dict: $(grep -E 'done in' "$OUT/index-dict.log" | tail -1) size=$(du -sm "$NI/shards" | cut -f1) MiB" | tee -a "$OUT/index.txt"
su keegan -c "bash '$P/parity-e30.sh' '$B' /home/keegan/small/parity-e44" > "$OUT/parity.log" 2>&1
grep -h "parity" /home/keegan/small/parity-e44/*.out | grep -v "^|" | tee "$OUT/parity.txt"
grep -v -P "^(short_unindexable|huge_output)\t" "$P/../queries-big.tsv" > /tmp/q18.tsv; chmod 644 /tmp/q18.tsv
bash "$P/serve-ab.sh" "$OUT/ab-4G" "base=$A@$BI" "dict=$B@$NI" "$BI" 4G /tmp/q18.tsv 2 3 1
bash "$P/cold-io.sh" "$A" "$BI" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-base.txt" > /dev/null
bash "$P/cold-io.sh" "$B" "$NI" 4G /tmp/q18.tsv 3000 "$OUT/cold-io-dict.txt" > /dev/null
