#!/bin/bash
# E21: serve-mode vs CLI, warm and cold, per profile; then the 1-CPU memory cliff (serve).
#   run-e21.sh <bin-dir> <warm-rounds> <cold-rounds> [part...]   parts: modes cliff (default both)
# Index: a private copy of the E19 (v3) 12-CPU index, because serve adds its mirrored repo to
# the index it serves and the A/B indexes must stay untouched. Results go to
# ~/small/results/e21, which must not exist yet: e21sum.py summarises everything in it, so an
# earlier run's files would mix with this one's.
set -euo pipefail
B=$1 WR=$2 CR=$3; shift 3
parts=("$@"); [ ${#parts[@]} -eq 0 ] && parts=(modes cliff)
H=~/small; P=$(cd "$(dirname "$0")" && pwd); OUT=$H/results/e21
if [ -e "$OUT" ]; then echo "$OUT exists: its results would mix with these; move it away first" >&2; exit 2; fi
mkdir -p "$OUT"
I=$H/idx-e21-serve
if [ ! -d "$I" ]; then cp -r "$H/idx-e20-12cpu-infinity" "$I"; fi
grep -v '^#' "$H/queries.tsv" > /dev/null  # fail early if the query file is missing

if [ ! -d "$H/tiny.git" ]; then
  git init --quiet --bare --initial-branch=main "$H/tiny.git"
  t=$(mktemp -d); git -C "$t" init --quiet --initial-branch=main
  echo "tiny" > "$t/README"; git -C "$t" add README
  git -C "$t" -c user.name=bench -c user.email=bench@localhost commit --quiet -m tiny
  git -C "$t" push --quiet "$H/tiny.git" main; rm -rf "$t"
fi
mkdir -p "$H/mirrors"
cat > "$H/serve-e21.toml" <<EOF
mirror_dir = "$H/mirrors"
interval_secs = 86400
[[repo]]
url = "$H/tiny.git"
EOF

evict() { # nothing may map the index while its pages are dropped
  if pgrep -f -- "--index-dir $I" > /dev/null; then echo "a scout still has $I open" >&2; exit 2; fi
  sync  # dirty pages (a fresh copy, serve's own writes) cannot be dropped until written
  python3 "$P/pagecache.py" evict "$I" > /dev/null
  python3 "$P/pagecache.py" evict "$B" > /dev/null
}

port=18200
serve_start() { # cpus mem tag -> sets SP and CG
  local c=$1 m=$2 tag=$3
  "$P/prof.sh" "$c" "$m" "$B/scout" --index-dir "$I" serve --config "$H/serve-e21.toml" --listen "127.0.0.1:$port" \
    --max-output-mb 4096 --search-timeout-secs 900 > "$OUT/serve-$tag.log" 2>&1 &
  SP=$!
  local up=0
  for _ in $(seq 300); do
    # Wait for the first sync cycle too: its generation swap must not land mid-benchmark.
    if curl -s "http://127.0.0.1:$port/status" | grep -q '"last_cycle": {'; then up=1; break; fi
    sleep 0.2
  done
  [ $up = 1 ] || { echo "serve did not become ready on $tag" >&2; cat "$OUT/serve-$tag.log" >&2; exit 2; }
  local spid; spid=$(pgrep -f -- "^$B/scout --index-dir $I serve" | head -1)
  CG=/sys/fs/cgroup$(sed 's/^0:://' "/proc/$spid/cgroup")
}
serve_stop() {
  pkill -f -- "^$B/scout --index-dir $I serve"; wait "$SP" 2>/dev/null || true
  grep "profile" "$OUT/serve-$1.log" || true
  port=$((port + 1))
}

# How fast a cold read is here: the WSL disk may be cached by Windows, which a cloud disk is not.
evict
t0=$(date +%s.%N); cat "$I"/shards/* > /dev/null; t1=$(date +%s.%N)
python3 -c "import sys; mib=int(sys.argv[3])/2**20; s=float(sys.argv[2])-float(sys.argv[1]); print(f'cold sequential read: {mib:.0f} MiB in {s:.2f} s = {mib/s:.0f} MiB/s')" "$t0" "$t1" "$(cat "$I"/shards/* | wc -c)" | tee "$OUT/disk.txt"

if [[ " ${parts[*]} " == *" modes "* ]]; then
  for prof in 1:2G 4:4G 12:infinity; do
    c=${prof%%:*} m=${prof##*:}; tag="${c}cpu"
    echo "== $tag"
    evict
    "$P/prof.sh" "$c" "$m" python3 "$P/qbench.py" cli "$B" "$I" "$H/queries.tsv" warm "$WR" "$OUT/cli-warm-$tag.json"
    "$P/prof.sh" "$c" "$m" python3 "$P/qbench.py" cli "$B" "$I" "$H/queries.tsv" cold "$CR" "$OUT/cli-cold-$tag.json"
    evict
    serve_start "$c" "$m" "$tag"
    taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$CG" "$I" "$H/queries.tsv" warm "$WR" "$OUT/serve-warm-$tag.json"
    taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$CG" "$I" "$H/queries.tsv" cold "$CR" "$OUT/serve-cold-$tag.json"
    serve_stop "$tag"
  done
fi

if [[ " ${parts[*]} " == *" cliff "* ]]; then
  for m in 2G 1536M 1280M 1G 768M 512M; do
    tag="cliff-1cpu-$m"
    echo "== $tag"
    evict
    serve_start 1 "$m" "$tag"
    if ! taskset -c 8-11 python3 "$P/qbench.py" serve "http://127.0.0.1:$port" "$CG" "$I" "$H/queries.tsv" warm "${CLIFF_ROUNDS:-5}" "$OUT/serve-warm-$tag.json"; then
      echo "$tag: run failed (see serve log)"
    fi
    cat "$CG/memory.events" 2>/dev/null | tr '\n' ' ' > "$OUT/events-$tag.txt" || true
    serve_stop "$tag"
  done
fi
python3 "$P/e21sum.py" "$OUT" | tee "$OUT/summary.txt"
