"""Root controller for E35: a persistent real HTTP server, cold index per request.

bounded-http.py <scout> <250-repo-index> <new-out> <existing-sync.toml> <rounds>
Uses ebs-run.sh 2 4G 125 3000; never changes the production corpus or existing baselines.
"""
import errno
import hashlib
import json
import os
from pathlib import Path
import queue
import random
import re
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

import pagecache


def evict(index, cgroup=None):
    if cgroup is not None:
        try:
            (cgroup / "memory.reclaim").write_text("16G")
        except OSError as error:
            if error.errno != errno.EAGAIN:
                raise
    for path in pagecache.files(index):
        with open(path, "rb") as source:
            os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    resident, _ = pagecache.residency(index)
    if resident > 2:
        raise RuntimeError(f"index not cold: {resident:.3f} MiB resident")
    return resident


def read_bytes(pid):
    io = dict(line.split(": ") for line in Path(f"/proc/{pid}/io").read_text().splitlines())
    return int(io["read_bytes"])


def query(base, case):
    url = base + "/search?" + urllib.parse.urlencode(case["params"])
    started = time.perf_counter()
    with urllib.request.urlopen(url, timeout=10) as response:
        body = response.read()
        headers = dict((key.lower(), value) for key, value in response.headers.items())
        if response.status != 200:
            raise RuntimeError(f"HTTP {response.status}")
    wall = (time.perf_counter() - started) * 1000
    if "x-scout-complete" not in headers:
        raise RuntimeError("bounded response has no completion metadata")
    result = {"wall_ms": wall, "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest(),
              "complete": headers["x-scout-complete"] == "true", "stop": headers["x-scout-stop-reason"],
              "matched_lines": int(headers["x-scout-returned-matches"])}
    if case["params"].get("c"):
        result["count"] = json.loads(body)["count"]
        if (result["count"]["kind"] == "exact") != result["complete"]:
            raise RuntimeError("count completion and exactness disagree")
    elif len(body) > int(case["params"].get("max_bytes", 65536)) or result["matched_lines"] > 100:
        raise RuntimeError("response exceeded its result/byte budget")
    return result, body


def main():
    if len(sys.argv) != 6 or os.geteuid() != 0:
        raise SystemExit(__doc__)
    exe, index, out, config = map(Path, sys.argv[1:5])
    rounds = int(sys.argv[5])
    if rounds < 1 or not exe.is_file() or not config.is_file() or not (index / "fixture.json").is_file():
        raise ValueError("positive rounds, executable, existing sync config and completed stress fixture required")
    out.mkdir(parents=True, exist_ok=False)
    before = evict(index)
    if pagecache.read_ahead_kb(index) != 128:
        raise RuntimeError("expected 128 KiB read-ahead")
    harness = Path(__file__).resolve().parent
    command = ["bash", str(harness / "ebs-run.sh"), "2", "4G", "125", "3000", str(exe), "--index-dir", str(index),
               "serve", "--config", str(config), "--threads", "2", "--http-workers", "2", "--listen", "127.0.0.1:0"]
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    messages = queue.Queue()
    log = (out / "server.log").open("w")
    def drain():
        for line in process.stderr:
            log.write(line); log.flush(); messages.put(line)
    reader = threading.Thread(target=drain, daemon=True); reader.start()
    pid = None
    try:
        startup = time.perf_counter()
        while True:
            line = messages.get(timeout=120)
            match = re.search(r"listening on (http://\S+)", line)
            if match:
                base = match[1]; break
        pids = []
        for item in Path("/proc").iterdir():
            if item.name.isdigit():
                try:
                    args = (item / "cmdline").read_bytes().split(b"\0")
                except (FileNotFoundError, ProcessLookupError):
                    continue
                if args and args[0] == os.fsencode(exe) and b"serve" in args and os.fsencode(index) in args:
                    pids.append(int(item.name))
        if len(pids) != 1:
            raise RuntimeError(f"expected one benchmark server, found {pids}")
        pid = pids[0]
        rel = Path(f"/proc/{pid}/cgroup").read_text().strip().split("::", 1)[1]
        cgroup = Path("/sys/fs/cgroup" + rel)
        if int((cgroup / "memory.max").read_text()) != 4 << 30:
            raise RuntimeError("server memory limit changed")
        # The real serve command requires sync configuration. One tiny local control
        # repo makes that configuration healthy; it is in addition to the 250 fixtures.
        deadline = time.monotonic() + 120
        while True:
            with urllib.request.urlopen(base + "/status", timeout=10) as response:
                status = json.load(response)
            if len(status["repos"]) == 251 and status.get("last_cycle") is not None:
                if status["last_cycle"].get("error") or status["last_cycle"].get("failures"):
                    raise RuntimeError(f"control sync failed: {status['last_cycle']}")
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("control sync did not finish")
            time.sleep(0.1)
        subprocess.run(["sync", "-f", str(index / "catalog.json")], check=True)
        cases = []
        for name, pattern in [("if", "if"), ("return", "return"), ("export", "export"),
                              ("todo", "TODO|FIXME|HACK"), ("date", r"\d{4}-\d{2}-\d{2}"),
                              ("symbol", "getAsCXXRecordDecl"), ("none", "zqxjvbnm_nothing_here"),
                              ("unindexable_sparse", "^[A-Za-z_]{173}$")]:
            cases.append({"name": name, "params": {"q": pattern, "n": 1, "limit": 100, "max_bytes": 65536, "timeout_ms": 1000}, "samples": []})
        cases += [{"name": "count_threshold", "params": {"q": "if", "c": 1, "limit": 100, "timeout_ms": 1000}, "samples": []},
                  {"name": "count_time_budget", "params": {"q": "if", "c": 1, "limit": 1000000000, "timeout_ms": 1000}, "samples": []},
                  {"name": "count_none", "params": {"q": "zqxjvbnm_nothing_here", "c": 1, "limit": 1000000000, "timeout_ms": 1000}, "samples": []}]
        data = {"complete": False, "exe": str(exe), "binary_sha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
                "fixture": json.loads((index / "fixture.json").read_text()), "served_repos": 251,
                "initial_resident_mib": before, "startup_seconds": time.perf_counter() - startup,
                "scope": "persistent HTTP server; index reclaimed before each cold request; heap filters stay live; 2 CPU / 4 GiB / 125 MB/s / 3000 IOPS",
                "rounds": rounds, "queries": cases}
        def save():
            (out / "progress.json").write_text(json.dumps(data, indent=2) + "\n")
        for case in cases:
            query(base, case)
            case["warm"], _ = query(base, case)
        for r in range(rounds):
            order = cases.copy(); random.Random(20261028 + r).shuffle(order)
            for case in order:
                resident = evict(index, cgroup)
                reads = read_bytes(pid)
                result, body = query(base, case)
                result.update({"round": r, "resident_mib_before": resident, "read_bytes": read_bytes(pid) - reads})
                case["samples"].append(result)
                save()
                print(f"round {r+1}/{rounds} {case['name']}: {result['wall_ms']:.1f} ms, {result['stop']}, {result['matched_lines']} lines, {result['read_bytes']/2**20:.1f} MiB read", flush=True)
        for case in cases:
            case["median_ms"] = statistics.median(s["wall_ms"] for s in case["samples"])
        data["memory_peak_mib"] = int((cgroup / "memory.peak").read_text()) / 2**20
        data["complete"] = True; save()
        (out / "summary.json").write_text(json.dumps(data, indent=2) + "\n")
    finally:
        if pid is not None:
            try: os.kill(pid, 15)
            except ProcessLookupError: pass
        else:
            process.terminate()
        process.wait(timeout=15); reader.join(timeout=5); log.close()


if __name__ == "__main__":
    main()
