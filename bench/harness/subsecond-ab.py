"""Separate-cgroup A/B for two index formats, with streamed exact-output parity.

run BEFORE_EXE BEFORE_INDEX AFTER_EXE AFTER_INDEX QUERIES NEW_OUT ROUNDS RUNS
batch EXE INDEX QUERIES ROUND RUNS

Each batch evicts only its own index before warming it inside a fresh 2 CPU / 4 GiB
cgroup. Two indexes must not compete for that memory: a deployed server holds one.
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


def queries(path):
    rows = []
    for line in Path(path).read_text().splitlines():
        if line and not line.startswith("#"):
            name, flags, pattern = line.split("\t")
            rows.append((name, shlex.split(flags), pattern))
    if not rows or len({row[0] for row in rows}) != len(rows):
        raise ValueError("query names must be nonempty and unique")
    return rows


def batch(exe, index, tsv, round_text, runs_text):
    round_index, runs = int(round_text), int(runs_text)
    if runs < 1:
        raise ValueError("runs must be positive")
    for file in pagecache.files(index):
        with open(file, "rb") as source:
            os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    resident, total = pagecache.residency(index)
    if resident > 2:
        raise RuntimeError(f"index not evicted: {resident:.1f} MiB")
    rows = queries(tsv)
    random.Random(20261027 + round_index).shuffle(rows)
    result = {"index": index, "resident_mib_before_warmup": resident,
              "index_mib": total, "round": round_index, "queries": []}
    for name, flags, pattern in rows:
        command = [exe, "--index-dir", index, "search", "--freshness", "off", "--threads", "2", "-n", *flags, "--", pattern]
        output = None
        if round_index == 0:
            digest, size = hashlib.sha256(), 0
            with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
                while block := child.stdout.read(1 << 20):
                    digest.update(block)
                    size += len(block)
                error = child.stderr.read()
                code = child.wait()
            if code not in (0, 1):
                raise RuntimeError(error.decode(errors="replace"))
            output = {"bytes": size, "sha256": digest.hexdigest(), "exit": code}
        else:
            warmup = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=180)
            if warmup.returncode not in (0, 1):
                raise RuntimeError(warmup.stderr.decode(errors="replace"))
        samples = []
        for sample in range(runs):
            usage = resource.getrusage(resource.RUSAGE_CHILDREN)
            start = time.perf_counter()
            child = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=180)
            elapsed = (time.perf_counter() - start) * 1000
            finished = resource.getrusage(resource.RUSAGE_CHILDREN)
            if child.returncode not in (0, 1):
                raise RuntimeError(child.stderr.decode(errors="replace"))
            samples.append({"round": round_index, "sample": sample, "wall_ms": elapsed,
                "cpu_ms": ((finished.ru_utime + finished.ru_stime) - (usage.ru_utime + usage.ru_stime)) * 1000,
                "read_bytes": (finished.ru_inblock - usage.ru_inblock) * 512})
        result["queries"].append({"name": name, "flags": flags, "pattern": pattern, "output": output, "samples": samples})
        print(f"round {round_index + 1} {name} {statistics.median(s['wall_ms'] for s in samples):.1f} ms", file=sys.stderr, flush=True)
    print(json.dumps(result))


def run(before, before_index, after, after_index, tsv, output, rounds_text, runs_text):
    if os.geteuid() != 0:
        raise ValueError("run as root for fresh cgroups")
    rounds, runs = int(rounds_text), int(runs_text)
    if rounds < 1 or runs < 1:
        raise ValueError("rounds and runs must be positive")
    directory = Path(output)
    directory.mkdir(parents=True, exist_ok=False)
    workload = directory / "queries.tsv"
    workload.write_bytes(Path(tsv).read_bytes())
    harness = Path(__file__).resolve()
    arms = {"before": (before, before_index), "after": (after, after_index)}
    data = {"rounds": rounds, "runs": runs, "arms": {}, "queries": []}
    for arm, (exe, index) in arms.items():
        data["arms"][arm] = {"exe": exe, "index": index,
            "binary_sha256": hashlib.sha256(Path(exe).read_bytes()).hexdigest(),
            "catalog_sha256": hashlib.sha256((Path(index) / "catalog.json").read_bytes()).hexdigest()}
    batches = {arm: [] for arm in arms}
    for r in range(rounds):
        order = list(arms) if r % 2 == 0 else list(reversed(arms))
        for arm in order:
            exe, index = arms[arm]
            path = directory / f"{r + 1}-{arm}.json"
            with path.open("w") as report, (directory / f"{r + 1}-{arm}.log").open("w") as log:
                subprocess.run(["bash", str(harness.parent / "ebs-run.sh"), "2", "4G", "125", "3000",
                    "python3", str(harness), "batch", exe, index, str(workload), str(r), str(runs)],
                    stdout=report, stderr=log, check=True, timeout=1200)
            batches[arm].append(json.loads(path.read_text()))
            print(f"round {r + 1}/{rounds}: {arm} complete", flush=True)
        for qa in batches["before"][0]["queries"]:
            qb = next(q for q in batches["after"][0]["queries"] if q["name"] == qa["name"])
            if qa["output"] != qb["output"]:
                raise RuntimeError(f"OUTPUT PARITY FAIL {qa['name']}: {qa['output']} / {qb['output']}")
    for name, flags, pattern in queries(workload):
        row = {"name": name, "flags": flags, "pattern": pattern, "samples": {}, "median_ms": {}}
        for arm in arms:
            row["samples"][arm] = [sample for b in batches[arm] for q in b["queries"] if q["name"] == name for sample in q["samples"]]
            row["median_ms"][arm] = statistics.median(s["wall_ms"] for s in row["samples"][arm])
        row["output"] = next(q["output"] for q in batches["after"][0]["queries"] if q["name"] == name)
        row["ratio"] = row["median_ms"]["after"] / row["median_ms"]["before"]
        row["rounds_won"] = sum(statistics.median(s["wall_ms"] for s in row["samples"]["after"] if s["round"] == r)
            < statistics.median(s["wall_ms"] for s in row["samples"]["before"] if s["round"] == r) for r in range(rounds))
        data["queries"].append(row)
    data["geomean_ratio"] = math.exp(statistics.mean(math.log(q["ratio"]) for q in data["queries"]))
    data["subsecond_medians"] = sum(q["median_ms"]["after"] < 1000 for q in data["queries"])
    (directory / "summary.json").write_text(json.dumps(data, indent=2) + "\n")
    print("| query | before ms | after ms | after/before | rounds won |")
    print("|---|---:|---:|---:|---:|")
    for q in data["queries"]:
        print(f"| {q['name']} | {q['median_ms']['before']:.1f} | {q['median_ms']['after']:.1f} | {q['ratio']:.3f} | {q['rounds_won']}/{rounds} |")
    print(f"subsecond medians: {data['subsecond_medians']}/{len(data['queries'])}; geomean {data['geomean_ratio']:.3f}", flush=True)


if __name__ == "__main__":
    if len(sys.argv) == 7 and sys.argv[1] == "batch":
        batch(*sys.argv[2:])
    elif len(sys.argv) == 10 and sys.argv[1] == "run":
        run(*sys.argv[2:])
    else:
        raise SystemExit(__doc__)
