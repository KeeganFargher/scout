#!/usr/bin/env python3
"""One untimed scout search per bench query per arm, so every index page the queries need
arrives in the page cache by scout's own mmap faults, charged to the caller's cgroup. Run
from outside every profile scope before a warm A/B whose arms search different indexes, so
neither index is charged to a profile's memory limit (E24).

    warmpass.py <queries.tsv> <bin-dir>@<index-dir>...
"""
import subprocess
import sys

qpath, *arms = sys.argv[1:]
if not arms:
    raise SystemExit(__doc__)
for line in open(qpath, encoding="utf8"):
    if line.startswith("#") or not line.strip():
        continue
    name, flags, pattern = line.rstrip("\n").split("\t")
    for arm in arms:
        b, i = arm.split("@")
        p = subprocess.run([f"{b}/scout", "--index-dir", i, "search", "-n", "--freshness", "off", *flags.split(), "--", pattern],
                           stdout=subprocess.DEVNULL)
        if p.returncode not in (0, 1):
            raise SystemExit(f"{name} on {arm}: exit {p.returncode}")
