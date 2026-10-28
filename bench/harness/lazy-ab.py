"""Bounded-memory, interleaved warm CLI A/B plus exact byte-output parity.

Usage: lazy-ab.py INDEX QUERIES BEFORE AFTER OUT ROUNDS RUNS
Run inside ebs-run.sh so both arms share the same CPU, memory and I/O limits.
Output hashes are streamed separately from timing; timed output goes to /dev/null.
"""
import hashlib
import json
import math
import os
from pathlib import Path
import random
import resource
import shlex
import statistics
import subprocess
import sys
import time

import pagecache


def main():
    if len(sys.argv) != 8:
        raise SystemExit(__doc__)
    index, tsv, before, after, output = sys.argv[1:6]
    rounds, runs = map(int, sys.argv[6:])
    if rounds < 1 or runs < 1:
        raise SystemExit("rounds and runs must be positive")
    path = Path(output)
    if path.exists():
        raise SystemExit(f"refusing to overwrite {path}")
    queries = []
    for line in Path(tsv).read_text().splitlines():
        if line and not line.startswith("#"):
            name, flags, pattern = line.split("\t")
            queries.append((name, shlex.split(flags), pattern))
    if not queries or len({q[0] for q in queries}) != len(queries):
        raise SystemExit("queries must be nonempty and uniquely named")
    arms = {"before": before, "after": after}
    data = {"index": index, "rounds": rounds, "runs": runs,
            "arms": {name: {"path": exe, "sha256": hashlib.sha256(Path(exe).read_bytes()).hexdigest()}
                     for name, exe in arms.items()},
            "catalog_sha256": hashlib.sha256((Path(index) / "catalog.json").read_bytes()).hexdigest(),
            "queries": []}

    # The warm-up must populate pages from this limited cgroup, not inherit an index
    # charged to the indexing process's former cgroup. Evict only this test index.
    for file in pagecache.files(index):
        with open(file, "rb") as source:
            os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    resident, _ = pagecache.residency(index)
    if resident > 2:
        raise RuntimeError(f"index still resident before warm-up: {resident:.1f} MiB")
    data["resident_mib_before_warmup"] = resident

    def command(exe, flags, pattern):
        return [exe, "--index-dir", index, "search", "--freshness", "off", "--threads", "2", "-n", *flags, "--", pattern]

    def digest(cmd):
        # The largest broad output is ~900 MB. Holding it would change the memory
        # pressure we are trying to measure and could mask a decoder memory regression.
        h, size = hashlib.sha256(), 0
        with subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
            while block := child.stdout.read(1 << 20):
                h.update(block)
                size += len(block)
            stderr = child.stderr.read()
            code = child.wait(timeout=180)
        if code not in (0, 1):
            raise RuntimeError(f"{cmd}: {code}: {stderr.decode(errors='replace')}")
        return {"sha256": h.hexdigest(), "bytes": size, "exit": code}

    for name, flags, pattern in queries:
        hashes = {arm: digest(command(exe, flags, pattern)) for arm, exe in arms.items()}
        if hashes["before"] != hashes["after"]:
            raise RuntimeError(f"OUTPUT PARITY FAIL {name}: {hashes}")
        data["queries"].append({"name": name, "flags": flags, "pattern": pattern,
                                "output": hashes["after"], "samples": {arm: [] for arm in arms}})
        print(f"parity {name}: {hashes['after']['bytes']} bytes identical", flush=True)

    # Both complete output passes above warm the exact pages the timed workload needs.
    for round_index in range(rounds):
        order = list(arms) if round_index % 2 == 0 else list(reversed(arms))
        shuffled = list(data["queries"])
        random.Random(20261027 + round_index).shuffle(shuffled)
        for q in shuffled:
            for arm in order:
                for sample in range(runs):
                    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
                    started = time.perf_counter()
                    child = subprocess.run(command(arms[arm], q["flags"], q["pattern"]),
                                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=180)
                    elapsed = (time.perf_counter() - started) * 1000
                    finished = resource.getrusage(resource.RUSAGE_CHILDREN)
                    if child.returncode not in (0, 1):
                        raise RuntimeError(child.stderr.decode(errors="replace"))
                    q["samples"][arm].append({"round": round_index, "sample": sample, "wall_ms": elapsed,
                        "cpu_ms": ((finished.ru_utime + finished.ru_stime) - (usage.ru_utime + usage.ru_stime)) * 1000,
                        "read_bytes": (finished.ru_inblock - usage.ru_inblock) * 512})
        print(f"round {round_index + 1}/{rounds} complete", flush=True)
    for q in data["queries"]:
        q["median_ms"] = {arm: statistics.median(s["wall_ms"] for s in samples) for arm, samples in q["samples"].items()}
        q["ratio"] = q["median_ms"]["after"] / q["median_ms"]["before"]
        q["rounds_won"] = sum(
            statistics.median(s["wall_ms"] for s in q["samples"]["after"] if s["round"] == r)
            < statistics.median(s["wall_ms"] for s in q["samples"]["before"] if s["round"] == r)
            for r in range(rounds))
    data["geomean_ratio"] = math.exp(statistics.mean(math.log(q["ratio"]) for q in data["queries"]))
    path.write_text(json.dumps(data, indent=2) + "\n")
    print("| query | before ms | after ms | after/before | rounds won |")
    print("|---|---:|---:|---:|---:|")
    for q in data["queries"]:
        print(f"| {q['name']} | {q['median_ms']['before']:.1f} | {q['median_ms']['after']:.1f} | {q['ratio']:.3f} | {q['rounds_won']}/{rounds} |")
    print(f"geomean after/before: {data['geomean_ratio']:.3f}", flush=True)


if __name__ == "__main__":
    main()
