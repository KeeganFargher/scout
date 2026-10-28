#!/bin/bash
# E24 (per-shard trigram filter, native format v5) against the build before it (v3).
#   run-e24.sh <base-bin-dir> <e24-bin-dir> <part>...
#   parts, run in this order: index indexab cli serve serve12 serve128 clicold cliff heapcli
# index:   builds the v5 bench index with the 12-CPU profile, the same shard split as the v3
#          index it is compared with; checks the two match but for the filters and that every
#          key passes its own filter (v3v5same.py), and the planner's gate against an
#          independent probe of the filters (filtercheck.py); copies it for serve and primes
#          the copy (its first cycle adds the mirrored repo).
# indexab: indexing time and memory against base: each build a fresh index of the same repos
#          in a scratch directory, arms alternating, 5 builds each at 1 CPU / 2 GiB and at
#          12 CPUs. Wall time varies by seconds from build to build with the disk's writeback
#          (12.0 to 17.4 s at 1 CPU for one arm), so CPU time (user + sys) is the measure.
# cli:     interleaved warm A/B of the CLI (ab.sh), 5 rounds of 20 runs, 1, 4 and 12 CPUs.
#          Both indexes are first evicted, then warmed from this script's own cgroup with the
#          corpus (equalise): page cache is charged to the cgroup that first faults it, and in
#          E24's first run the v3 index was charged to the A/B's 2 GiB scopes (base hit the
#          limit) while the freshly written v5 one stayed charged to the indexer's.
# serve:   serve cold and warm (serve-coldab.sh), 2 reps, 3 cold and 10 warm rounds, 1 and 4 CPUs.
# serve12: the same at 12 CPUs with 1 cold round, to check scale-up.
# serve128: serve cold again where a cold fault reads what it would on a cloud VM: 2 reps of 3
#          cold rounds at 1, 4 and 12 CPUs, and only 2 warm rounds, since a warm request reads
#          nothing from disk. Like clicold, it needs at most 128 KiB of read-ahead.
# clicold: the CLI cold at 1 CPU / 2 GiB, both arms alternating, 2 reps of 2 rounds. Refuses
#          to start on a disk with over 128 KiB of read-ahead: at the WSL disk's 8 MiB a cold
#          open's header, fence and marker reads bring every shard's dictionary and filter in
#          with them, so both arms read the same bytes and the A/B cannot see the filter (E24).
# cliff:   serve warm at 1 CPU under shrinking memory limits, both arms alternating.
# heapcli: warm A/B of the e24 CLI (filters probed through the map) against a build whose CLI
#          copies them to the heap (bin dir in $HEAP_BIN), on the same index, at 1 CPU only:
#          the copy runs in the parallel shard open, so more CPUs hide it rather than test it.
# Every part writes to a directory of its own and refuses to start if it exists, so two
# runs' results never pool. Cold numbers depend on the disk's read-ahead (README), which
# qbench.py records in every report.
set -euo pipefail
[ $# -ge 3 ] || { sed -n 3,4p "$0" >&2; exit 2; }
base=$1 new=$2; shift 2
parts=("$@")
H=~/small; P=$(cd "$(dirname "$0")" && pwd)
V3=$H/idx-e20-12cpu-infinity V5=$H/idx-e24-12cpu-infinity
S3=$H/idx-e21-serve S5=$H/idx-e24-serve
has() { [[ " ${parts[*]} " == *" $1 "* ]]; }

for p in "${parts[@]}"; do
  case $p in index | indexab | cli | serve | serve12 | serve128 | clicold | cliff | heapcli) ;; *) echo "unknown part $p" >&2; exit 2 ;; esac
done
for B in "$base" "$new"; do
  [ -x "$B/scout" ] && [ -x "$B/scout-bench" ] || { echo "$B lacks scout or scout-bench" >&2; exit 2; }
done
[ -d "$V3" ] && [ -d "$S3" ] || { echo "missing $V3 or $S3" >&2; exit 2; }
if has heapcli; then
  [ -n "${HEAP_BIN:-}" ] && [ -x "$HEAP_BIN/scout" ] && [ -x "$HEAP_BIN/scout-bench" ] \
    || { echo "heapcli needs HEAP_BIN, a bin dir whose CLI keeps filters on the heap" >&2; exit 2; }
fi
if has clicold || has serve128; then
  ra=$(python3 "$P/pagecache.py" readahead "$H")
  [ "$ra" -le 128 ] || { echo "clicold and serve128 need at most 128 KiB of read-ahead, as on a cloud VM; $H's disk has $ra" >&2; exit 2; }
fi

# The v3 index's repos, by root, in its order: the same corpus and the same repo names.
mapfile -t ROOTS < <(python3 -c 'import json, sys; [print(r["root"]) for r in json.load(open(sys.argv[1]))["repos"]]' "$V3/catalog.json")
[ ${#ROOTS[@]} -gt 0 ] || { echo "no repos read from $V3/catalog.json" >&2; exit 2; }

fresh() { # dir; a part's output, which must not exist yet
  if [ -e "$1" ]; then echo "$1 exists: its results would pool with these; move it away first" >&2; exit 2; fi
  mkdir -p "$1"
}

evict() { # bin index; nothing may map the index while its pages are dropped
  if pgrep -f -- "--index-dir $2" > /dev/null; then echo "a scout still has $2 open" >&2; exit 2; fi
  sync
  python3 "$P/pagecache.py" evict "$2" > /dev/null
  python3 "$P/pagecache.py" evict "$1" > /dev/null
}

warm_corpus() { # reads every bench repo once, from this script's cgroup
  local rc=0
  "$H/bin/rg" -c zzqqxxnevermatches "${ROOTS[@]}" > /dev/null || rc=$?
  # 1 is rg's "no match", the expected result; anything else is a failed read.
  [ $rc = 1 ] || { echo "rg exited $rc warming the corpus" >&2; exit 2; }
}

equalise() { # bin@index...: evicts every arm's index and binary, then caches what the bench
  # queries need from this script's cgroup, so no profile scope is charged for any of it
  local arm
  for arm in "$@"; do evict "${arm%%@*}" "${arm#*@}"; done
  warm_corpus
  python3 "$P/warmpass.py" "$H/queries.tsv" "$@"
}

port=18600
serve_start() { # bin index cpus mem log -> sets SP and CG
  local B=$1 I=$2 c=$3 m=$4 log=$5 up=0
  "$P/prof.sh" "$c" "$m" "$B/scout" --index-dir "$I" serve --config "$H/serve-e21.toml" --listen "127.0.0.1:$port" \
    --max-output-mb 4096 --search-timeout-secs 900 > "$log" 2>&1 &
  SP=$!
  for _ in $(seq 300); do
    # Wait for the first sync cycle too: its generation swap must not land mid-benchmark.
    if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
    sleep 0.2
  done
  [ $up = 1 ] || { echo "serve did not become ready ($log)" >&2; cat "$log" >&2; exit 2; }
  local spid; spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1)
  CG=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
}
serve_stop() { # bin index
  pkill -f -- "^$1/scout --index-dir $2 serve"; wait "$SP" 2>/dev/null || true
  port=$((port + 1))
}
primed() { # bin index; a first cycle that reindexed means the index lacked the mirrored repo
  if ! curl -s "http://127.0.0.1:$port/status" | python3 -c 'import json, sys; sys.exit(bool(json.load(sys.stdin)["last_cycle"]["reindexed"]))'; then
    echo "the first cycle on $2 reindexed; prime it first" >&2; serve_stop "$1" "$2"; exit 2
  fi
}

if has index; then
  R=$H/results/e24-index; fresh "$R"
  rm -rf "$V5" "$S5"
  "$new/scout" --index-dir "$V5" add "${ROOTS[@]}" > /dev/null
  # Warm the corpus outside the scope, as every bench index was built.
  warm_corpus
  "$P/prof.sh" 12 infinity /usr/bin/time -f "index_wall_s=%e maxrss_kib=%M" "$new/scout" --index-dir "$V5" index \
    > "$R/index.log" 2> "$R/index.err"
  grep -E "index_wall_s|done in" "$R/index.err" | tee "$R/index-summary.txt"
  du -sh "$V3" "$V5" | tee -a "$R/index-summary.txt"
  python3 "$P/v3v5same.py" "$V3" "$V5" | tee -a "$R/index-summary.txt"
  python3 "$P/filtercheck.py" "$new/scout" "$V5" "$H/queries.tsv" | tee "$R/filtercheck.txt"
  cp -r "$V5" "$S5"
  # Prime the serve copy: its first cycle indexes the mirrored repo, as S3's once did.
  evict "$new" "$S5"
  serve_start "$new" "$S5" 12 infinity "$R/prime.log"
  serve_stop "$new" "$S5"
  python3 -c 'import json, sys; a, b = (len(json.load(open(p))["repos"]) for p in sys.argv[1:]); print(f"serve copy: {b} repos, bench index {a}"); sys.exit(b != a + 1)' \
    "$V5/catalog.json" "$S5/catalog.json" | tee -a "$R/index-summary.txt"
fi

if has indexab; then
  R=$H/results/e24-indexab; fresh "$R"
  I=$H/idx-e24-indexab
  warm_corpus
  for prof in 1:2G 12:infinity; do
    c=${prof%%:*} m=${prof##*:}
    for r in 1 2 3 4 5; do
      if [ $((r % 2)) = 1 ]; then order=("base=$base" "e24=$new"); else order=("e24=$new" "base=$base"); fi
      for arm in "${order[@]}"; do
        name=${arm%%=*} B=${arm#*=}
        rm -rf "$I"
        "$B/scout" --index-dir "$I" add "${ROOTS[@]}" > /dev/null
        # The previous build's index was deleted and synced, so no writeback overlaps this one.
        sync
        "$P/prof.sh" "$c" "$m" /usr/bin/time -f "index_wall_s=%e user_s=%U sys_s=%S maxrss_kib=%M" "$B/scout" --index-dir "$I" index \
          > /dev/null 2> "$R/$name-${c}cpu-$r.err" || { echo "$name build failed: $R/$name-${c}cpu-$r.err" >&2; exit 1; }
        du -sb "$I" | awk '{print "index_bytes=" $1}' >> "$R/$name-${c}cpu-$r.err"
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
each = lambda rs, k: ", ".join(f"{r[k]:.2f}" for r in rs)
for prof in sorted({p for p, _ in runs}, key=lambda p: int(p[:-3])):
    print(f"## {prof}: arm | builds | CPU s, user + sys (median; each) | wall s (median; each)"
          " | max RSS MiB (median) | cgroup peak MiB (median) | index bytes")
    for arm in ("base", "e24"):
        rs = runs[(prof, arm)]
        print(f"{arm} | {len(rs)}"
              f" | {statistics.median(r['cpu_s'] for r in rs):.2f}; {each(rs, 'cpu_s')}"
              f" | {statistics.median(r['index_wall_s'] for r in rs):.2f}; {each(rs, 'index_wall_s')}"
              f" | {statistics.median(r['maxrss_kib'] for r in rs) / 1024:.0f}"
              f" | {statistics.median(r['cgroup_peak_mib'] for r in rs):.0f}"
              f" | {', '.join(sorted({str(int(r['index_bytes'])) for r in rs}))}")
EOF
fi

if has cli; then
  equalise "$base@$V3" "$new@$V5"
  "$P/ab.sh" e24 "$V3" "base=$base@$V3" "e24=$new@$V5" 5 20
fi

if has serve; then
  "$P/serve-coldab.sh" e24serve "base=$base@$S3" "e24=$new@$S5" 2 10 3 1:2G 4:4G
fi

if has serve12; then
  "$P/serve-coldab.sh" e24serve12 "base=$base@$S3" "e24=$new@$S5" 2 10 1 12:infinity
fi

if has serve128; then
  "$P/serve-coldab.sh" e24serve128 "base=$base@$S3" "e24=$new@$S5" 2 2 3 1:2G 4:4G 12:infinity
fi

if has clicold; then
  OUT=$H/ab/e24clicold; fresh "$OUT"
  for r in 1 2; do
    if [ $r = 1 ]; then order=("base $base $V3" "e24 $new $V5"); else order=("e24 $new $V5" "base $base $V3"); fi
    for spec in "${order[@]}"; do
      read -r arm B I <<< "$spec"
      evict "$B" "$I"
      "$P/prof.sh" 1 2G python3 "$P/qbench.py" cli "$B" "$I" "$H/queries.tsv" cold 2 "$OUT/cold-$arm-1cpu-$r.json"
    done
  done
  python3 "$P/coldabsum.py" "$OUT" base e24 | tee "$OUT/summary.txt"
fi

if has cliff; then
  for m in 1280M 1G 768M 512M; do
    OUT=$H/ab/e24cliff-$m; fresh "$OUT"
    for r in 1 2; do
      if [ $r = 1 ]; then order=("base $base $S3" "e24 $new $S5"); else order=("e24 $new $S5" "base $base $S3"); fi
      for spec in "${order[@]}"; do
        read -r arm B I <<< "$spec"
        evict "$B" "$I"
        serve_start "$B" "$I" 1 "$m" "$OUT/serve-$arm-$r.log"
        primed "$B" "$I"
        taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$CG" "$I" "$H/queries.tsv" warm 3 "$OUT/warm-$arm-1cpu-$r.json"
        tr '\n' ' ' < "$CG/memory.events" > "$OUT/events-$arm-$r.txt"
        serve_stop "$B" "$I"
      done
    done
    echo "== cliff $m"; python3 "$P/coldabsum.py" "$OUT" base e24 | tee "$OUT/summary.txt"
  done
fi

if has heapcli; then
  # One index for both arms; equalise still runs, so neither arm's scope is charged for it.
  equalise "$new@$V5" "$HEAP_BIN@$V5"
  "$P/ab.sh" e24heapcli "$V5" "map=$new" "heap=$HEAP_BIN" 5 20 1:2G
fi
