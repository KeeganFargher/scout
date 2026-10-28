#!/usr/bin/env python3
"""Where a cold query's reads land, by shard section (native format v9).

    coldmap.py <index-dir> <cmd...>

Evicts the index, runs <cmd> (a scout search, e.g. through ebs-run.sh), then maps every
resident page of every shard to the section it falls in: header, content, docs, blocks,
paths, units, keys, vals, fence, filter, postings. A page that straddles two sections counts
for the first. Prints MiB resident per section, summed over shards, and the section totals,
so a cold query's bytes read can be split into lookup data and content.
"""
import os
import struct
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pagecache  # noqa: E402

SECTIONS = ["content", "docs", "blocks", "paths", "units", "keys", "vals", "fence", "filter", "postings"]
MAGIC = b"SCOUTNI9"


def bounds(path):
    with open(path, "rb") as f:
        head = f.read(112)
    if head[:8] != MAGIC:
        raise SystemExit(f"{path}: not a v9 shard")
    offs = list(struct.unpack_from("<10Q", head, 32))
    ends = offs[1:] + [os.path.getsize(path)]
    return [("header", 0, offs[0])] + list(zip(SECTIONS, offs, ends))


def resident_vec(path):
    """mincore of `path` as a list of 0/1 per page."""
    size = os.path.getsize(path)
    fd = os.open(path, os.O_RDONLY)
    try:
        addr = pagecache.libc.mmap(None, size, 1, 1, fd, 0)  # PROT_READ, MAP_SHARED
        if addr in (None, pagecache.ctypes.c_void_p(-1).value):
            raise OSError(pagecache.ctypes.get_errno(), f"mmap {path}")
        try:
            n = (size + pagecache.PAGE - 1) // pagecache.PAGE
            vec = (pagecache.ctypes.c_ubyte * n)()
            if pagecache.libc.mincore(addr, size, vec) != 0:
                raise OSError(pagecache.ctypes.get_errno(), f"mincore {path}")
            return [b & 1 for b in vec]
        finally:
            pagecache.libc.munmap(addr, size)
    finally:
        os.close(fd)


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    idx, cmd = sys.argv[1], sys.argv[2:]
    shards = [p for p in pagecache.files(os.path.join(idx, "shards")) if p.endswith(".native")]
    if not shards:
        raise SystemExit(f"no shards under {idx}/shards")
    for p in pagecache.files(idx):
        fd = os.open(p, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)
    res, _ = pagecache.residency(idx)
    if res > 2:
        raise SystemExit(f"{res:.1f} MiB still resident after evicting: a process maps the index?")
    run = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if run.returncode not in (0, 1):
        raise SystemExit(f"command failed ({run.returncode}): {run.stderr}")
    page = pagecache.PAGE
    hit, size = {}, {}
    for p in shards:
        vec = resident_vec(p)
        for name, start, end in bounds(p):
            size[name] = size.get(name, 0) + end - start
            first, last = start // page, (end + page - 1) // page
            hit[name] = hit.get(name, 0) + sum(vec[first:last]) * page
            # The next section starts on this page: count it once, here.
            if end % page:
                vec[last - 1] = 0
    total = sum(hit.values())
    print(f"{len(shards)} shards, {total / 2**20:.1f} MiB resident after the query")
    print("| section | MiB read | MiB in index | share of reads |")
    print("|---|---:|---:|---:|")
    for name in ["header"] + SECTIONS:
        print(f"| {name} | {hit[name] / 2**20:.1f} | {size[name] / 2**20:.1f} | {hit[name] / max(total, 1):.0%} |")
    for line in run.stderr.splitlines():
        if line.startswith("ebs "):
            print(line)


if __name__ == "__main__":
    main()
