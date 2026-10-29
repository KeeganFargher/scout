"""Posting list sizes across an index (native v9): how many lists, and how many bytes,
fall at or under each size, next to the tables (docs..postings) serve copies to the heap."""
import bisect
import os
import struct
import sys

root = sys.argv[1]
edges = [8, 16, 32, 64, 128, 256, 512, 1024, 4096, 1 << 62]
n = [0] * len(edges)
b = [0] * len(edges)
tables = 0
total_post = 0
shards = 0
for dp, _, fs in os.walk(root):
    for f in fs:
        p = os.path.join(dp, f)
        with open(p, "rb") as fh:
            head = fh.read(112)
            if len(head) < 112 or head[:8] != b"SCOUTNI9":
                continue
            tri = struct.unpack_from("<I", head, 16)[0]
            offs = struct.unpack_from("<10Q", head, 32)
            docs_off, vals_off, post_off = offs[1], offs[6], offs[9]
            size = os.fstat(fh.fileno()).st_size
            fh.seek(vals_off)
            raw = fh.read(tri * 12)
        starts = [o for _, o in struct.iter_unpack("<IQ", raw)]
        starts.append(size - post_off)
        for i in range(tri):
            ln = starts[i + 1] - starts[i]
            k = bisect.bisect_left(edges, ln)
            n[k] += 1
            b[k] += ln
        tables += post_off - docs_off
        total_post += size - post_off
        shards += 1
tot = sum(n)
print(f"shards {shards}, tables {tables / 2**20:.0f} MiB, postings {total_post / 2**20:.0f} MiB, lists {tot}")
cn, cb = 0, 0
for e, k, v in zip(edges, n, b):
    cn += k
    cb += v
    print(f"<= {e if e < 1 << 60 else 'inf':>6}: lists {k:>10} ({cn / tot:6.1%} cum), bytes {v / 2**20:9.1f} MiB (cum {cb / 2**20:9.1f})")
