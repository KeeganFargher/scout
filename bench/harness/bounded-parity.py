"""E35 exhaustive-search parity against all 19 frozen E34 output hashes.

Run in ebs-run.sh 2 4G 125 3000:
bounded-parity.py <scout> <E34-index> <E34-summary.json> <new-output.json>
This is a correctness gate, not a new timing comparison.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    if len(sys.argv) != 5:
        raise SystemExit(__doc__)
    exe, index, reference, output = map(Path, sys.argv[1:])
    if output.exists():
        raise ValueError("output exists")
    before = json.loads(reference.read_text())
    catalog_hash = hashlib.sha256((index / "catalog.json").read_bytes()).hexdigest()
    if catalog_hash != before["arms"]["after"]["catalog_sha256"]:
        raise RuntimeError("E34 reference index changed")
    data = {"complete": False, "binary_sha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
            "catalog_sha256": catalog_hash, "reference": str(reference), "queries": []}
    for case in before["queries"]:
        command = [str(exe), "--index-dir", str(index), "search", "--freshness", "off",
                   "--threads", "2", "-n", *case["flags"], "--", case["pattern"]]
        digest, size = hashlib.sha256(), 0
        with tempfile.TemporaryFile() as error, subprocess.Popen(command, stdout=subprocess.PIPE, stderr=error) as child:
            while block := child.stdout.read(1 << 20):
                digest.update(block)
                size += len(block)
            code = child.wait()
            actual = {"bytes": size, "sha256": digest.hexdigest(), "exit": code}
            if actual != case["output"]:
                error.seek(0)
                raise RuntimeError(f"exhaustive parity failed: {case['name']}: {actual}, {error.read(8192)!r}")
        data["queries"].append({"name": case["name"], "output": actual, "passed": True})
        print(f"exhaustive E34 parity OK: {case['name']}", flush=True)
    data["complete"] = True
    with output.open("x") as destination:
        json.dump(data, destination, indent=2)
        destination.write("\n")


if __name__ == "__main__":
    main()
