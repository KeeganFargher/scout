#!/bin/bash
# Cross-format timing: each arm warms its own index in a fresh 2 CPU / 4 GiB cgroup.
# Usage: run-subsecond-ab.sh <before-exe> <before-index> <after-exe> <after-index> <new-out>
set -euo pipefail
[ "$#" = 5 ] || { echo "expected before-exe before-index after-exe after-index new-out" >&2; exit 2; }
P=$(cd "$(dirname "$0")" && pwd)
python3 "$P/subsecond-ab.py" run "$1" "$2" "$3" "$4" "$P/../queries-lazy.tsv" "$5" 5 2
