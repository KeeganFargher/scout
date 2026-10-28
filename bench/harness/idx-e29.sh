#!/bin/bash
# E29: index chromium + llvm-project into /mnt/ebs/idx-e29 inside the 2 CPU / 4 GiB box with
# the default --content auto, which must pick lz4 here (5.2 GiB of content > 4 GiB / 2).
# io is left unthrottled: this measures the index, not the build.
set -euo pipefail
B=/home/keegan/small/bin-e29/scout
I=/mnt/ebs/idx-e29
rm -rf "$I"
mkdir -p "$I"
chown 1000:1000 "$I"
H="$(dirname "$0")"
bash "$H/ebs-run.sh" 2 4G max max "$B" --index-dir "$I" add /mnt/ebs/big/chromium /mnt/ebs/big/llvm-project
/usr/bin/time -f "%e s %M KiB" bash "$H/ebs-run.sh" 2 4G max max "$B" --index-dir "$I" index
du -sh "$I"
ls "$I/shards" | wc -l
