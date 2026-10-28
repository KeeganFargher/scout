"""Cold-index follow-up to E34, with the same CLI output destination and limits.

Run inside ebs-run.sh 2 4G 125 3000:
    cold-subsecond.py <E34-warm-summary.json> <new-output-directory> <rounds>

The executable stays warm; every index file is evicted before every timed search.
Cold output parity is checked separately, without including hashing in the timings.
"""
import hashlib
import json
import os
from pathlib import Path
import random
import resource
import statistics
import subprocess
import sys
import tempfile
import threading
import time

import pagecache

NAMES = ["license_lines", "return_lines", "date_lines", "if_lines", "selective_symbol", "selective_none"]


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def evict(index, read_ahead):
    if pagecache.read_ahead_kb(index) != read_ahead:
        raise RuntimeError("disk read-ahead changed")
    for path in pagecache.files(index):
        with open(path, "rb") as source:
            os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    resident, _ = pagecache.residency(index)
    if resident > 2:
        raise RuntimeError(f"cold run not cold: {resident:.3f} MiB still resident")
    return resident


def output_hash(command):
    # Drain stderr independently of stdout, so reader warnings cannot fill a pipe.
    with tempfile.TemporaryFile() as error, subprocess.Popen(command, stdout=subprocess.PIPE, stderr=error) as child:
        timer = threading.Timer(300, child.kill)
        timer.daemon = True
        timer.start()
        try:
            checksum, size = hashlib.sha256(), 0
            while block := child.stdout.read(1 << 20):
                checksum.update(block)
                size += len(block)
            code = child.wait()
        finally:
            timer.cancel()
        if code not in (0, 1):
            error.seek(0)
            raise RuntimeError(f"hash command exited {code}: {error.read(8192)!r}")
    return {"sha256": checksum.hexdigest(), "bytes": size, "exit": code}


def main():
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    reference, directory, rounds = Path(sys.argv[1]), Path(sys.argv[2]), int(sys.argv[3])
    if rounds < 1:
        raise ValueError("rounds must be positive")
    warm = json.loads(reference.read_text())
    arm = warm["arms"]["after"]
    exe, index = arm["exe"], arm["index"]
    if digest(exe) != arm["binary_sha256"] or digest(Path(index) / "catalog.json") != arm["catalog_sha256"]:
        raise RuntimeError("binary or catalog differs from the accepted warm benchmark")
    rel = Path("/proc/self/cgroup").read_text().strip().split("::", 1)[1]
    cgroup = Path("/sys/fs/cgroup" + rel)
    memory = int((cgroup / "memory.max").read_text())
    quota, period = map(int, (cgroup / "cpu.max").read_text().split())
    cpus = sorted(os.sched_getaffinity(0))
    if memory != 4 << 30 or quota != 2 * period or cpus != [0, 1]:
        raise RuntimeError("expected 2 pinned CPUs, 200% quota and 4 GiB memory")
    if int((cgroup / "memory.swap.max").read_text()) != 0:
        raise RuntimeError("swap must be disabled")
    read_ahead = pagecache.read_ahead_kb(index)
    if read_ahead != 128:
        raise RuntimeError(f"expected 128 KiB read-ahead, got {read_ahead}")
    rows = []
    for name in NAMES:
        query = next(q for q in warm["queries"] if q["name"] == name)
        rows.append({"name": name, "flags": query["flags"], "pattern": query["pattern"],
                     "warm_median_ms": query["median_ms"]["after"], "expected_output": query["output"], "samples": []})
    directory.mkdir(parents=True, exist_ok=False)
    data = {"complete": False, "reference": str(reference), "arm": arm, "rounds": rounds,
            "cpus": cpus, "memory_bytes": memory, "readahead_kb": read_ahead,
            "cold_scope": "index evicted before every sample; executable warm; timed stdout /dev/null",
            "queries": rows}

    def save():
        (directory / "progress.json").write_text(json.dumps(data, indent=2) + "\n")

    def command(row):
        return [exe, "--index-dir", index, "search", "--freshness", "off", "--threads", "2", "-n",
                *row["flags"], "--", row["pattern"]]

    save()
    for round_index in range(rounds):
        order = rows.copy()
        random.Random(20261027 + round_index).shuffle(order)
        for row in order:
            resident = evict(index, read_ahead)
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            started = time.perf_counter()
            child = subprocess.run(command(row), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=300)
            elapsed = (time.perf_counter() - started) * 1000
            after = resource.getrusage(resource.RUSAGE_CHILDREN)
            if child.returncode != row["expected_output"]["exit"]:
                raise RuntimeError(f"search exit mismatch: {row['name']}: {child.returncode}: {child.stderr[:8192]!r}")
            read_bytes = (after.ru_inblock - before.ru_inblock) * 512
            faults = after.ru_majflt - before.ru_majflt
            if read_bytes <= 0 or faults <= 0:
                raise RuntimeError(f"cold run had no disk reads or major faults: {row['name']}")
            sample = {"round": round_index, "wall_ms": elapsed, "read_bytes": read_bytes, "major_faults": faults,
                      "cpu_ms": (after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime) * 1000,
                      "resident_mib_before": resident, "exit": child.returncode}
            row["samples"].append(sample)
            save()
            print(f"round {round_index + 1}/{rounds} {row['name']}: {elapsed:.1f} ms, {read_bytes / 2**20:.1f} MiB read, {faults} major faults", flush=True)
    for row in rows:
        resident = evict(index, read_ahead)
        output = output_hash(command(row))
        if output != row["expected_output"]:
            raise RuntimeError(f"COLD OUTPUT PARITY FAIL: {row['name']}: {output}")
        row["cold_output"] = output
        row["hash_run_resident_mib_before"] = resident
        row["cold_median_ms"] = statistics.median(s["wall_ms"] for s in row["samples"])
        row["cold_median_read_mib"] = statistics.median(s["read_bytes"] for s in row["samples"]) / 2**20
        save()
        print(f"cold output parity OK: {row['name']}", flush=True)
    if pagecache.read_ahead_kb(index) != read_ahead or digest(exe) != arm["binary_sha256"]:
        raise RuntimeError("read-ahead or binary changed during the run")
    data["complete"] = True
    save()
    (directory / "summary.json").write_text(json.dumps(data, indent=2) + "\n")
    print("| query | warm ms | cold ms | cold MiB read |")
    print("|---|---:|---:|---:|")
    for row in rows:
        print(f"| {row['name']} | {row['warm_median_ms']:.1f} | {row['cold_median_ms']:.1f} | {row['cold_median_read_mib']:.1f} |")


if __name__ == "__main__":
    main()
