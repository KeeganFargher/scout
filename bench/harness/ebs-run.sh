#!/bin/bash
# Runs a command as uid 1000 on an EBS-like small server (run as root: wsl.exe -u root):
#   ebs-run.sh <cpus> <mem> <read MB/s|max> <read IOPS|max> <cmd...>   e.g. ebs-run.sh 2 4G 125 3000 scout ...
# A fresh cgroup under the root with cpu.max and taskset for <cpus>, memory.max <mem> and no
# swap (the page cache of the index is charged to it, as on a real VM), and io.max on the
# /mnt/ebs volume of ebs-setup.sh: gp3's baseline is 125 MB/s and 3000 IOPS. Writes get the
# same caps. Prints the cgroup's memory.peak, OOM kills and io.stat for the volume after.
set -uo pipefail
cpus=$1 mem=$2 mbps=$3 iops=$4; shift 4
[ "$(id -u)" = 0 ] || { echo "run as root (wsl.exe -u root)" >&2; exit 1; }
mountpoint -q /mnt/ebs || { echo "/mnt/ebs is not mounted: run ebs-setup.sh" >&2; exit 1; }
dev=$(cat "/sys/block/$(basename "$(readlink -f /dev/mapper/ebs)")/dev")
cg=/sys/fs/cgroup/ebs-$$-$RANDOM
mkdir "$cg"
trap 'rmdir "$cg" 2>/dev/null' EXIT
echo "$((cpus * 100000)) 100000" > "$cg/cpu.max"
echo "$mem" > "$cg/memory.max"
echo 0 > "$cg/memory.swap.max"
# "max" lifts a cap (an index build measures the index, not the volume).
bps=max
[ "$mbps" = max ] || bps=$((mbps * 1000000))
echo "$dev rbps=$bps wbps=$bps riops=$iops wiops=$iops" > "$cg/io.max"
mask=$(seq -s, 0 $((cpus - 1)))
# The child joins the cgroup before it execs, so nothing it starts runs outside the limits.
bash -c 'echo $$ > "$1/cgroup.procs"; shift; exec setpriv --reuid=1000 --regid=1000 --init-groups taskset -c "$@"' \
  _ "$cg" "$mask" "$@"
rc=$?
peak=$(cat "$cg/memory.peak")
ooms=$(awk '$1=="oom_kill"{print $2}' "$cg/memory.events")
io=$(grep "^$dev " "$cg/io.stat" | cut -d' ' -f2-)
echo "ebs cpus=$cpus mem=$mem mbps=$mbps iops=$iops rc=$rc cgroup_peak_mib=$((peak / 1048576)) oom_kills=$ooms io: ${io:-none}" >&2
exit $rc
