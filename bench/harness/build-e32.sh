#!/bin/bash
# E31 + E32 measured alone: build the E33 baseline snapshot (the working tree with E30-E32)
# with E30's MADV_RANDOM taken out. E30's per-list MADV_WILLNEED in `postings` stays, so
# an A/B against E29 holds the count hint, the candidate-unit reads and that WILLNEED,
# which only acts on pages not yet resident. The snapshot is left untouched: this edits
# a copy.
# Usage (as the user): build-e32.sh <snapshot-dir> <out-bin-dir>
set -euo pipefail
[ "$#" = 2 ] || { echo "expected snapshot-dir out-bin-dir" >&2; exit 2; }
snap=$1 out=$2
[ ! -e "$out" ] || { echo "$out exists" >&2; exit 2; }
export PATH="$HOME/.local/zigshim:$HOME/.cargo/bin:$HOME/.local/bin:$HOME/.local/zig:/usr/local/bin:/usr/bin:/bin"
export CC=cc AR=ar CARGO_TARGET_DIR="$HOME/scout-target-e32"
src=$(mktemp -d)
cp -r "$snap"/. "$src"
lib="$src/crates/scout-index-native/src/lib.rs"
# Exactly one advice call: a snapshot that differs fails here instead of measuring
# something else.
[ "$(grep -c 'shard.advise_random(' "$lib")" = 1 ] || { echo "expected one advise_random call" >&2; exit 1; }
sed -i '/shard.advise_random(/d' "$lib"
(cd "$src" && cargo build --release -p scout-cli -p scout-bench 2>&1 | tail -1)
mkdir -p "$out"
cp "$CARGO_TARGET_DIR/release/scout" "$CARGO_TARGET_DIR/release/scout-bench" "$out"/
rm -rf "$src"
