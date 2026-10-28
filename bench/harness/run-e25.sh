#!/bin/bash
# E25: indexing time and memory of the read-ahead indexer against base on the small-server
# profiles: each build a fresh index of the bench repos in a scratch directory, arms
# alternating, 5 builds each at 1 CPU / 2 GiB, 4 CPUs / 4 GiB and 12 CPUs. As in E24's
# indexab, CPU time (user + sys) is the measure of cost: wall time varies by seconds with the
# disk's writeback. Wall time is still the number E25 set out to cut, so the summary shows
# both. The repos come from the v3 index's catalog, in its order, as in run-e24.sh.
#   run-e25.sh <base-bin-dir> <e25-bin-dir> [profile...]   (profile = cpus:mem; default all three)
set -euo pipefail
[ $# -ge 2 ] || { sed -n 2,8p "$0" >&2; exit 2; }
base=$1 new=$2; shift 2
profiles=("$@")
[ $# -gt 0 ] || profiles=(1:2G 4:4G 12:infinity)
H=~/small; P=$(cd "$(dirname "$0")" && pwd)
V3=$H/idx-e20-12cpu-infinity
R=$H/results/e25-indexab I=$H/idx-e25-indexab
for B in "$base" "$new"; do
  [ -x "$B/scout" ] || { echo "$B lacks scout" >&2; exit 2; }
done
[ -d "$V3" ] || { echo "missing $V3" >&2; exit 2; }
if [ -e "$R" ]; then echo "$R exists: its results would pool with these; move it away first" >&2; exit 2; fi
mkdir -p "$R"

mapfile -t ROOTS < <(python3 -c 'import json, sys; [print(r["root"]) for r in json.load(open(sys.argv[1]))["repos"]]' "$V3/catalog.json")
[ ${#ROOTS[@]} -gt 0 ] || { echo "no repos read from $V3/catalog.json" >&2; exit 2; }

# Reads every bench repo once, so no build pays for the first read of the corpus.
rc=0
"$H/bin/rg" -c zzqqxxnevermatches "${ROOTS[@]}" > /dev/null || rc=$?
[ $rc = 1 ] || { echo "rg exited $rc warming the corpus" >&2; exit 2; }

for prof in "${profiles[@]}"; do
  c=${prof%%:*} m=${prof##*:}
  for r in 1 2 3 4 5; do
    if [ $((r % 2)) = 1 ]; then order=("base=$base" "e25=$new"); else order=("e25=$new" "base=$base"); fi
    for arm in "${order[@]}"; do
      name=${arm%%=*} B=${arm#*=}
      rm -rf "$I"
      "$B/scout" --index-dir "$I" add "${ROOTS[@]}" > /dev/null
      # The previous build's index was deleted and synced, so no writeback overlaps this one.
      sync
      "$P/prof.sh" "$c" "$m" /usr/bin/time -f "index_wall_s=%e user_s=%U sys_s=%S maxrss_kib=%M" "$B/scout" --index-dir "$I" index \
        > /dev/null 2> "$R/$name-${c}cpu-$r.err" || { echo "$name build failed: $R/$name-${c}cpu-$r.err" >&2; exit 1; }
      du -sb "$I/shards" | awk '{print "index_bytes=" $1}' >> "$R/$name-${c}cpu-$r.err"
      # Shard bytes per repo, without the generation in their names: the arms must write the
      # same shards.
      (cd "$I/shards" && for f in $(ls | grep -v '^snapshots' | sort); do
        # repo.generation.n.backend: drop the generation from the right, so a repo name
        # with dots keeps them.
        echo "$(echo "$f" | sed -E 's/\.[0-9a-f]+\.([0-9]+)\.[^.]+$/.\1/') $(sha256sum "$f" | cut -c1-16)"
      done) > "$R/$name-${c}cpu-$r.sha"
      rm -rf "$I"
      sync
    done
  done
done

python3 - "$R" << 'EOF' | tee "$R/summary.txt"
import glob, os, re, statistics, sys
runs = {}  # (profile, arm) -> [{field: value}]
for f in sorted(glob.glob(os.path.join(sys.argv[1], "*.err"))):
    arm, prof, _ = os.path.basename(f)[:-4].split("-")
    text = open(f).read()
    fields = "index_wall_s|user_s|sys_s|maxrss_kib|cgroup_peak_mib|oom_kills|index_bytes"
    run = {k: float(v) for k, v in re.findall(rf"({fields})=(\d+(?:\.\d+)?)", text)}
    if len(run) != 7 or run["oom_kills"]:
        sys.exit(f"{f}: incomplete or OOM-killed build: {run}")
    run["cpu_s"] = run["user_s"] + run["sys_s"]
    runs.setdefault((prof, arm), []).append(run)
# Both arms must write the same shards in every build. Shard caps follow the profile's
# memory, so builds are compared within a profile only.
for prof in sorted({p for p, _ in runs}):
    got = {open(f).read() for f in glob.glob(os.path.join(sys.argv[1], f"*-{prof}-*.sha"))}
    if len(got) != 1:
        sys.exit(f"{prof}: SHARDS DIFFER ({len(got)} variants); the arms must write the same shards")
    print(f"{prof}: shards identical across arms and builds")
each = lambda rs, k: ", ".join(f"{r[k]:.2f}" for r in rs)
for prof in sorted({p for p, _ in runs}, key=lambda p: int(p[:-3])):
    print(f"## {prof}: arm | builds | CPU s, user + sys (median; each) | wall s (median; each)"
          " | max RSS MiB (median) | cgroup peak MiB (median) | index bytes")
    for arm in ("base", "e25"):
        rs = runs[(prof, arm)]
        print(f"{arm} | {len(rs)}"
              f" | {statistics.median(r['cpu_s'] for r in rs):.2f}; {each(rs, 'cpu_s')}"
              f" | {statistics.median(r['index_wall_s'] for r in rs):.2f}; {each(rs, 'index_wall_s')}"
              f" | {statistics.median(r['maxrss_kib'] for r in rs) / 1024:.0f}"
              f" | {statistics.median(r['cgroup_peak_mib'] for r in rs):.0f}"
              f" | {', '.join(sorted({str(int(r['index_bytes'])) for r in rs}))}")
EOF
