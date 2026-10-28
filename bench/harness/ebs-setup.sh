#!/bin/bash
# Builds an EBS-like volume in WSL2 and mounts it at /mnt/ebs (run as root: wsl.exe -u root).
#   ebs-setup.sh [read_delay_ms] [size]          defaults: 0 ms, 64G (sparse)
# A sparse file on the WSL disk, attached as a direct-I/O loop device (so the guest page
# cache sees each page once, through the volume, not twice), under a dm-delay target that
# adds read_delay_ms to every read, with ext4 on top. gp3's per-read latency is about 0.5-1
# ms and the WSL disk's own a fraction of that, so without the delay every random read
# would be far cheaper than on EBS. Bandwidth and IOPS caps are not set here: they are
# per cgroup (io.max), see ebs-run.sh. Read-ahead is set to 128 KiB, the default a cloud
# VM's volume gets; a cold mmap fault reads that much around the faulting page, so cold
# numbers depend on it (E24 found 8 MiB on the plain WSL disk).
# dm-delay counts in timer ticks: on this kernel any delay from 1 to 4 ms comes out as ~4 ms
# a read (measured at queue depth 1), and 0 leaves the WSL disk's ~0.28 ms. gp3 sits between,
# so results that matter are checked at both.
# Idempotent: an existing, mounted volume keeps its data and only gets the delay asked for.
set -euo pipefail
delay=${1:-0} size=${2:-64G}
img=/var/lib/ebs.img name=ebs mnt=/mnt/ebs
[ "$(id -u)" = 0 ] || { echo "run as root (wsl.exe -u root)" >&2; exit 1; }
if mountpoint -q "$mnt"; then
  loop=$(losetup -j "$img" | cut -d: -f1)
  dmsetup suspend "$name"
  dmsetup reload "$name" --table "0 $(blockdev --getsz "$loop") delay $loop 0 $delay $loop 0 0"
  dmsetup resume "$name"
else
  modprobe dm-delay
  [ -f "$img" ] || truncate -s "$size" "$img"
  loop=$(losetup -j "$img" | cut -d: -f1)
  [ -n "$loop" ] || loop=$(losetup --direct-io=on --show -f "$img")
  dmsetup info "$name" >/dev/null 2>&1 ||
    dmsetup create "$name" --table "0 $(blockdev --getsz "$loop") delay $loop 0 $delay $loop 0 0"
  blkid /dev/mapper/$name >/dev/null 2>&1 || mkfs.ext4 -q /dev/mapper/$name
  mkdir -p "$mnt"
  mount /dev/mapper/$name "$mnt"
  chown 1000:1000 "$mnt"
fi
dm=$(basename "$(readlink -f /dev/mapper/$name)")
echo 128 > /sys/block/$dm/queue/read_ahead_kb
# ebs-run.sh puts commands in cgroups that limit io, which needs the controller enabled below
# the root.
grep -qw io /sys/fs/cgroup/cgroup.subtree_control || echo +io > /sys/fs/cgroup/cgroup.subtree_control
echo "volume $mnt on $dm ($(cat /sys/block/$dm/dev)), read delay ${delay} ms, read_ahead_kb $(cat /sys/block/$dm/queue/read_ahead_kb)"
