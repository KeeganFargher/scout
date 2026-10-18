#!/usr/bin/env python3
"""Times the bench queries one at a time against `scout serve` or the CLI, warm or cold,
recording per request the latency, the result's hash, how much page cache the cgroup held,
and how many major faults the request took and how many bytes they read from disk.

Every report records the read-ahead of the disk holding the index, and the run fails if it
changes midway: a cold mmap fault reads a window that size around the faulting page, so a
cold number means nothing without it (E24 found 8 MiB on the WSL dev disk, where a typical
cloud VM has 128 KiB). Disk bytes read are the server's /proc/<pid>/io read_bytes, or the CLI
child's getrusage ru_inblock: WSL's user cgroups have no io controller, so no io.stat.

    qbench.py serve <base_url> <cgroup_dir> <index_dir> <queries.tsv> <mode> <rounds> <out.json>
    qbench.py cli   <bin_dir>               <index_dir> <queries.tsv> <mode> <rounds> <out.json>

The CLI runs <bin_dir>/scout; cold CLI runs evict all of <bin_dir>, so the binary is cold too.

mode warm: one untimed pass over every query, then <rounds> timed passes.
mode cold: before every timed request the index is evicted from the page cache, and the
           run fails if more than 2 MiB of it is still resident by mincore (a selective
           cold query reads ~18 MiB, so a mostly warm run cannot pass), or if the request
           then took no major fault at all, so a "cold" number can never be a cached one.
           The residue is recorded per request. (E21 ran with a 16 MiB limit and no fault
           check; every one of its cold requests took 146-294 major faults.) The check asks
           the kernel about the index files themselves: cgroup counters are not enough,
           because pages a previous process loaded stay charged to its cgroup and survive a
           reclaim of this one (found in the first smoke run). Serve: the server maps the
           index, so its cgroup is reclaimed (memory.reclaim; that also drops its binary's
           pages). CLI: nothing maps the index between runs, so the index and the binary are
           dropped with posix_fadvise.
Each pass runs the queries in a different shuffled order (fixed seeds), so no query always
follows the one that warmed the pages it needs. Every result of a query must hash the same
on every run, or the run fails.
"""
import errno
import hashlib
import json
import os
import random
import resource
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

import pagecache

FLAG_PARAM = {"-F": "F", "-i": "i", "-S": "S", "-w": "w", "-c": "c", "-l": "l"}
COLD_SLACK = 2 << 20


def load_queries(path):
    out = []
    for line in open(path, encoding="utf8"):
        if line.startswith("#") or not line.strip():
            continue
        name, flags, pattern = line.rstrip("\n").split("\t")
        out.append((name, flags.split(), pattern))
    return out


def cg_stat(cg):
    stat = dict(l.split() for l in open(f"{cg}/memory.stat"))
    return int(stat["file"]), int(stat["pgmajfault"])


def procs_read_bytes(cg):
    # Charged at bio submission to the task that faulted, read-around included; summed over
    # the cgroup's processes (one, the server), each over all of its threads.
    total = 0
    for pid in open(f"{cg}/cgroup.procs").read().split():
        io = dict(l.split(": ") for l in open(f"/proc/{pid}/io").read().splitlines())
        total += int(io["read_bytes"])
    return total


def children_read_bytes():
    # The same counter for waited-for children, in 512-byte units.
    return resource.getrusage(resource.RUSAGE_CHILDREN).ru_inblock * 512


def assert_cold(index_dir):
    res, _ = pagecache.residency(index_dir)
    if res * 2**20 > COLD_SLACK:
        raise SystemExit(f"cold run not cold: {res:.1f} MiB of {index_dir} still resident")
    return res


def evict_files(*roots):
    for root in roots:
        for p in pagecache.files(root):
            fd = os.open(p, os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)


def reclaim(cg):
    try:
        with open(f"{cg}/memory.reclaim", "w") as f:
            f.write("64G")
    except OSError as e:
        # EAGAIN: it could not reclaim all 64G asked for, which is expected; the residue
        # check below decides whether enough went.
        if e.errno != errno.EAGAIN:
            raise


def serve_runner(base):
    def run(flags, pattern):
        params = {FLAG_PARAM[f]: "1" for f in flags}
        params["n"] = "1"
        params["q"] = pattern
        t = time.perf_counter()
        try:
            with urllib.request.urlopen(f"{base}/search?{urllib.parse.urlencode(params)}", timeout=900) as r:
                body, code = r.read(), r.status
        except urllib.error.HTTPError as e:
            body, code = e.read(), e.code
        ms = (time.perf_counter() - t) * 1000
        if code != 200:
            raise SystemExit(f"HTTP {code} for {pattern!r}: {body[:300]!r}")
        return ms, body
    return run


def cli_runner(scout, index_dir):
    def run(flags, pattern):
        cmd = [scout, "--index-dir", index_dir, "search", "-n", "--freshness", "off", *flags, "--", pattern]
        t = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True)
        ms = (time.perf_counter() - t) * 1000
        # rg's convention: 1 means no match, which literal_none expects.
        if p.returncode not in (0, 1):
            raise SystemExit(f"scout exited {p.returncode} for {pattern!r}: {p.stderr[:300]!r}")
        return ms, p.stdout
    return run


def own_cgroup():
    rel = open("/proc/self/cgroup").read().strip().split("::", 1)[1]
    return "/sys/fs/cgroup" + rel


def main():
    kind = sys.argv[1] if len(sys.argv) > 1 else None
    if kind == "serve" and len(sys.argv) == 9:
        base, cg, index_dir, qpath, mode, rounds, out = sys.argv[2:]
        run = serve_runner(base)
        make_cold = lambda: reclaim(cg)
        disk_read = lambda: procs_read_bytes(cg)
    elif kind == "cli" and len(sys.argv) == 8:
        bin_dir, index_dir, qpath, mode, rounds, out = sys.argv[2:]
        run, cg = cli_runner(os.path.join(bin_dir, "scout"), index_dir), own_cgroup()
        make_cold = lambda: evict_files(index_dir, bin_dir)
        disk_read = children_read_bytes
    else:
        raise SystemExit(__doc__)
    if mode not in ("warm", "cold"):
        raise SystemExit(f"mode must be warm or cold, got {mode}")
    rounds = int(rounds)
    ra = pagecache.read_ahead_kb(index_dir)
    queries = load_queries(qpath)
    if mode == "warm":
        for _, flags, pattern in queries:
            run(flags, pattern)
    samples = {name: [] for name, _, _ in queries}
    hashes = {}
    for r in range(rounds):
        order = queries[:]
        random.Random(1000 + r).shuffle(order)
        for name, flags, pattern in order:
            resident = None
            if mode == "cold":
                make_cold()
                resident = assert_cold(index_dir)
            file_before, maj_before = cg_stat(cg)
            read_before = disk_read()
            ms, body = run(flags, pattern)
            read_after = disk_read()
            _, maj_after = cg_stat(cg)
            if mode == "cold" and maj_after == maj_before:
                raise SystemExit(f"{name}: a cold request took no major fault, so it read cached pages")
            h = hashlib.sha256(body).hexdigest()
            if hashes.setdefault(name, h) != h:
                raise SystemExit(f"{name}: result changed between runs")
            samples[name].append({"ms": ms, "file_mib": file_before / 2**20, "majfaults": maj_after - maj_before,
                                  "read_mib": (read_after - read_before) / 2**20,
                                  "resident_mib": resident, "bytes": len(body)})
    if pagecache.read_ahead_kb(index_dir) != ra:
        raise SystemExit(f"read_ahead_kb changed during the run (was {ra}): its numbers mix two disks")
    report = {"kind": kind, "mode": mode, "rounds": rounds, "read_ahead_kb": ra, "queries": []}
    for name, _, _ in queries:
        s = samples[name]
        ms = sorted(x["ms"] for x in s)
        report["queries"].append({
            "name": name, "sha256": hashes[name], "bytes": s[0]["bytes"],
            "p50_ms": statistics.median(ms), "p90_ms": ms[min(len(ms) - 1, int(len(ms) * 0.9))], "max_ms": ms[-1],
            "majfaults_median": statistics.median(x["majfaults"] for x in s),
            "read_mib_median": statistics.median(x["read_mib"] for x in s),
            "file_mib_before_median": statistics.median(x["file_mib"] for x in s),
            "runs": s,
        })
    p50s = [q["p50_ms"] for q in report["queries"]]
    report["geomean_p50_ms"] = statistics.geometric_mean(p50s)
    json.dump(report, open(out, "w"), indent=1)
    print(f"{kind} {mode}: geomean p50 {report['geomean_p50_ms']:.1f} ms (read_ahead_kb={ra})")


main()
