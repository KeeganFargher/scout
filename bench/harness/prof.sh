#!/bin/bash
# Runs a command inside a small-server profile, without root:
#   prof.sh <cpus> <mem> <cmd...>     e.g. prof.sh 1 2G scout search ...
# - CPU: pinned with taskset to N real CPUs, and CPUQuota=N*100% so scout's own core count
#   (std::thread::available_parallelism reads cgroup cpu.max) agrees with the pinning.
# - Memory: MemoryMax with no swap. Page cache for the mmapped shards is charged to the same
#   cgroup, as it would compete for RAM on a real small VM.
# Prints the scope's memory.peak and OOM kill count after the command exits.
set -uo pipefail
cpus=$1 mem=$2; shift 2
mask=$(seq -s, 0 $((cpus - 1)))
unit="small-$$-$RANDOM"
systemd-run --user --scope --quiet --unit="$unit" \
  -p MemoryMax="$mem" -p MemorySwapMax=0 -p CPUQuota="$((cpus * 100))%" \
  -- taskset -c "$mask" "$@" &
pid=$!
# The scope's cgroup exists while the command runs; sample its peak just before it ends.
cg=""
for _ in $(seq 50); do
  cg=$(cat "/proc/$pid/cgroup" 2>/dev/null | sed 's/^0:://')
  [ -n "$cg" ] && [[ "$cg" == *"$unit"* ]] && break
  sleep 0.05
done
peak=0 ooms=0
while kill -0 "$pid" 2>/dev/null; do
  if [ -n "$cg" ] && [ -r "/sys/fs/cgroup$cg/memory.peak" ]; then
    peak=$(cat "/sys/fs/cgroup$cg/memory.peak" 2>/dev/null || echo "$peak")
    ooms=$(awk '$1=="oom_kill"{print $2}' "/sys/fs/cgroup$cg/memory.events" 2>/dev/null)
  fi
  sleep 0.2
done
wait "$pid"; rc=$?
echo "profile cpus=$cpus mem=$mem rc=$rc cgroup_peak_mib=$((peak / 1048576)) oom_kills=${ooms:-0}" >&2
exit $rc
