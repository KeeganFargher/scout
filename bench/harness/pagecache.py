#!/usr/bin/env python3
"""Page-cache control for honest cold benchmarks, without root.

    pagecache.py resident <dir>   resident MiB / total MiB of every file under <dir> (mincore)
    pagecache.py evict <dir>      drop every file's clean pages (posix_fadvise DONTNEED), then
                                  print residency; exits 1 if more than 1% stays resident
    pagecache.py readahead <path> the read-ahead in KiB of the disk holding <path>

DONTNEED cannot drop pages a live process still maps, so evict only while no scout process
has the index open; a running server's pages are reclaimed through its cgroup instead.
"""
import ctypes
import ctypes.util
import mmap
import os
import sys

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(ctypes.c_ubyte)]
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
PAGE = os.sysconf("SC_PAGE_SIZE")


def files(root):
    for d, _, names in os.walk(root):
        for n in names:
            p = os.path.join(d, n)
            if os.path.isfile(p) and os.path.getsize(p) > 0:
                yield p


def resident_pages(path):
    size = os.path.getsize(path)
    fd = os.open(path, os.O_RDONLY)
    try:
        # A fresh read-only mapping: mincore reports the page cache, not this mapping's faults.
        addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        if addr in (None, ctypes.c_void_p(-1).value):
            raise OSError(ctypes.get_errno(), f"mmap {path}")
        try:
            n = (size + PAGE - 1) // PAGE
            vec = (ctypes.c_ubyte * n)()
            if libc.mincore(addr, size, vec) != 0:
                raise OSError(ctypes.get_errno(), f"mincore {path}")
            return sum(b & 1 for b in vec), n
        finally:
            libc.munmap(addr, size)
    finally:
        os.close(fd)


def read_ahead_kb(path):
    """A cold mmap fault reads a window this size around the faulting page, so a cold number
    means nothing without it (E24: 8 MiB on the WSL dev disk, 128 KiB on a typical cloud VM)."""
    dev = os.stat(path).st_dev
    sysdir = f"/sys/dev/block/{os.major(dev)}:{os.minor(dev)}"
    # A partition has no queue of its own: its disk's directory is the parent.
    if os.path.exists(f"{sysdir}/partition"):
        sysdir += "/.."
    return int(open(f"{sysdir}/queue/read_ahead_kb").read())


def residency(root):
    res = tot = 0
    for p in files(root):
        r, n = resident_pages(p)
        res += r
        tot += n
    if tot == 0:
        raise SystemExit(f"no files under {root}")
    return res * PAGE / 2**20, tot * PAGE / 2**20


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("resident", "evict", "readahead"):
        raise SystemExit(__doc__)
    cmd, root = sys.argv[1], sys.argv[2]
    if cmd == "readahead":
        print(read_ahead_kb(root))
        return
    if cmd == "evict":
        for p in files(root):
            fd = os.open(p, os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)
    res, tot = residency(root)
    print(f"resident_mib={res:.1f} total_mib={tot:.1f}")
    if cmd == "evict" and res > tot * 0.01:
        print("eviction failed: a process still maps these files?", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
