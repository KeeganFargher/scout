"""E35 correctness gates plus a cold exhaustive count reference.

Run in ebs-run.sh 2 4G 125 3000:
bounded-oracle.py <scout> <index> <HTTP-summary.json> <new-output.json>
"""
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import time

import pagecache


def main():
    if len(sys.argv) != 5:
        raise SystemExit(__doc__)
    exe, index, reference, output = map(Path, sys.argv[1:])
    if output.exists():
        raise ValueError("output exists")
    http = json.loads(reference.read_text())
    if not http["complete"] or hashlib.sha256(exe.read_bytes()).hexdigest() != http["binary_sha256"]:
        raise RuntimeError("HTTP benchmark incomplete or binary changed")
    for path in pagecache.files(index):
        with open(path, "rb") as source:
            os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    resident, _ = pagecache.residency(index)
    if resident > 2:
        raise RuntimeError(f"index not cold: {resident} MiB")
    base = [str(exe), "--index-dir", str(index), "search", "--freshness", "off", "--threads", "2"]
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter()
    total, files = 0, 0
    with tempfile.TemporaryFile() as error, subprocess.Popen([*base, "-c", "--", "if"], stdout=subprocess.PIPE, stderr=error) as child:
        for line in child.stdout:
            number = line.rstrip(b"\n").rsplit(b":", 1)[-1]
            if not number.isdigit():
                child.kill()
                raise RuntimeError(f"unexpected count record: {line[:200]!r}")
            total += int(number); files += 1
        code = child.wait()
        if code != 0:
            error.seek(0)
            raise RuntimeError(error.read(8192))
    elapsed = (time.perf_counter() - started) * 1000
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    data = {"complete": False, "binary_sha256": http["binary_sha256"], "exhaustive_if": {
        "matching_lines": total, "matching_files": files, "wall_ms": elapsed,
        "cpu_ms": (after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime) * 1000,
        "read_bytes": (after.ru_inblock - before.ru_inblock) * 512,
        "resident_mib_before": resident, "scope": "cold CLI exhaustive -c plus streamed count aggregation"}, "prefix_checks": []}
    print(f"exhaustive cold if count: {total} lines in {elapsed:.1f} ms", flush=True)
    for case in http["queries"]:
        samples = [case["warm"], *case["samples"]]
        if case["params"].get("c"):
            for sample in samples:
                count = sample["count"]
                if case["params"]["q"] == "if":
                    if count["value"] > total or (count["kind"] == "exact" and count["value"] != total):
                        raise RuntimeError("budgeted count disagrees with exhaustive count")
                elif count != {"kind": "exact", "value": 0}:
                    raise RuntimeError("unexpected no-match count")
            continue
        lengths = {s["bytes"] for s in samples}
        if max(lengths) == 0 and not any(s["complete"] for s in samples):
            data["prefix_checks"].append({"query": case["name"], "empty_incomplete_prefix": True})
            continue
        with tempfile.TemporaryFile() as error, subprocess.Popen([*base, "-n", "--", case["params"]["q"]], stdout=subprocess.PIPE, stderr=error) as child:
            prefix = child.stdout.read(max(lengths))
            for sample in samples:
                if hashlib.sha256(prefix[:sample["bytes"]]).hexdigest() != sample["sha256"]:
                    child.kill()
                    raise RuntimeError(f"not an exhaustive output prefix: {case['name']}")
                if sample["complete"] and (sample["bytes"] != len(prefix) or child.stdout.read(1) != b""):
                    child.kill()
                    raise RuntimeError(f"incomplete search mislabeled complete: {case['name']}")
            child.stdout.close()
            code = child.wait(timeout=120)
            if any(s["complete"] for s in samples) and code not in (0, 1):
                error.seek(0)
                raise RuntimeError(f"exhaustive complete reference failed ({code}): {error.read(8192)!r}")
        data["prefix_checks"].append({"query": case["name"], "checks": len(samples), "passed": True})
        print(f"exhaustive prefix parity OK: {case['name']}", flush=True)
    data["complete"] = True
    with output.open("x") as destination:
        json.dump(data, destination, indent=2)
        destination.write("\n")


if __name__ == "__main__":
    main()
