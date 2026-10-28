# Search latency opportunities at 2 CPUs / 4 GiB

Assessment: 2026-10-27. No search implementation was changed.

Subsequent implementation: [lazy decoding and buffer reuse, measured against the current
tree](lazy-decoding-benchmark.md). The assessment below records the earlier investigation.

The next wins are reducing verification/decode work and avoiding unnecessary full scans.
Adding search threads or making the posting planner more selective is unlikely to help
queries where almost every candidate is a real match.

## Evidence and limits

I inspected the current working tree, including its in-progress E31 count hints and E32
candidate-unit reads. Those are existing work, not new recommendations or measured wins
from this audit. I did not build a matching v7 index or benchmark the current tree.

Live measurements use the existing E29/v6 binary and compressed chromium + llvm-project
index: 697,675 files, 51 shards, about 2.3 GiB. They establish the workload, not the exact
latency of the current modified source.

The existing ebs-run.sh harness imposed two pinned CPUs, CPU quota 200%, memory.max=4G,
no swap, 125 MB/s and 3,000 IOPS. Each query/mode had one warm-up and three timed samples.
Output went to /dev/null; stderr captured --stats. These are warm CLI medians, excluding
HTTP queueing, network transfer and a client's consumption of output. This was a
diagnostic mode comparison, not an interleaved optimization A/B or a new rg parity run.

| Pattern                        |        Lines | Filenames (-l) |  Counts (-c) | Candidates / matched files |
| ------------------------------ | -----------: | -------------: | -----------: | -------------------------: |
| std::unique_ptr, fixed         |       581 ms |         540 ms |       549 ms |            50,646 / 50,302 |
| Common license sentence, fixed |     1,681 ms |       1,632 ms |     1,680 ms |          184,684 / 182,037 |
| TODO\|FIXME\|HACK              |       776 ms |         737 ms |       781 ms |            49,040 / 44,213 |
| `\d{4}-\d{2}-\d{2}`            |     2,352 ms |       2,331 ms |     2,367 ms |            697,675 / 5,699 |
| if                             | not measured |       3,739 ms | not measured |          697,675 / 367,730 |

All timed samples read **zero bytes from disk**. Total child CPU time was about 1.9–2.0x
wall time. The harness reported 2,617 MiB peak charged memory and zero OOM kills for
the whole run, including warm-ups. This is not a full serve-plus-sync memory estimate.
Page-cache accounting can depend on which cgroup first populated a shared page.

The common literal's candidate precision is 99.3%, and the license sentence's 98.6%:
there are almost no false candidates to remove. Their remaining cost is in reading,
decoding, matching and formatting. Conversely, the date query plans as All and only
0.82% of checked files match.

Raw samples and final stats: [results.json](../bench/results/latency-audit-20261027/results.json).
Replay script: [measure.py](../bench/results/latency-audit-20261027/measure.py).
It retains the environment-specific E29 binary/index paths and needs that existing setup:

```powershell
wsl -d Ubuntu -u root -- bash "/mnt/c/Users/User/source/test space/scout/bench/harness/ebs-run.sh" 2 4G 125 3000 python3 "/mnt/c/Users/User/source/test space/scout/bench/results/latency-audit-20261027/measure.py"
```

Binary SHA-256: 3a41fcc72b8f798caccc0338ace1181a9bfeb20fb41d3b4cce7a70c621af1a35.
Catalog SHA-256: eb94f2b66d7f071abdd29d9b4d89ec5ed8f78d3c33cd1b8190b7c1b661379003.
Kernel: 6.18.33.2-microsoft-standard-WSL2. Observed dm-0 read-ahead: 128 KiB.

## Recommended experiments, in implementation order

### 1. Reuse initialized decode buffers without clearing and zeroing them for every file

[NativeShard::content](../crates/scout-index-native/src/lib.rs) clears its buffer, resizes
it with zeroes to the entire decoded length, then overwrites that region. unit_content
also clears and zero-initializes appended output for every requested block.

Keep the initialized length where possible, grow only when necessary, and decode into
checked slices. Track the actual write cursor for unit reads. All requested output
must still be overwritten and its decoded length validated before it is returned.
This can stay in safe Rust and retain the corruption guards.

This is the smallest experiment and applies even to unindexable scans. Its gain is
unmeasured: the real decoder's cost may dwarf the avoidable initialization.

### 2. Make block decoding lazy, so -l and -m can stop before decoding the rest of a file

In [visit_docs](../crates/scout-core/src/search.rs), content or unit_content completes
before the matcher sees any bytes. The current first_match_only break skips later
matching calls, but unit_content has already decoded every selected run.

Expose a block/run visitor that verifies each run as it is decoded. Stop decoding after
the first hit for -l; maintain a file-wide remaining match budget for -m. Count mode
can aggregate runs and emit exactly once per file rather than selecting Reading::Whole.

The important remaining case is Query::All: candidate-unit reads do not help when no
unit constraints exist. A filename search for the common license can often match near
a file's beginning, yet currently reads its whole compressed content first.

Keep line numbers, final unterminated lines, binary/BOM behavior and rg match-count
semantics exact. Retain explicit whole-file handling where the format requires it.
Limit prefetch to the same bounded progress window so lazy decoding also saves reads.
This is the first substantial change I would try.

### 3. Add a sound filter for regex character classes that currently plan as All

The date pattern costs about 2.35 seconds to check almost 698k files for 5,699 hits.
[query.rs](../crates/scout-core/src/query.rs) limits class/exact-string expansion and
cannot express this as useful literal trigrams. Raising those limits globally risks
large plans and thousands of dictionary probes; it is not an established fix.

Prototype a compact, separate class-normalized ngram or shape filter, starting with
digit runs and punctuation. Extract only necessary conditions from the regex HIR,
then keep the existing matcher as the final authority. Unicode decimal digits,
case behavior, transformed encodings and cross-boundary matches require explicit
soundness coverage; an ASCII-only shortcut must not lose Unicode matches.

Measure additional index/cache bytes as well as candidate reduction. The 4 GiB box's
cache residency is a constraint on the design. This has high upside for scan-like
regexes but is a format/planner experiment, not a quick tuning change.

### 4. Give ranked queries the same work distribution as plain queries

[search_ranked](../crates/scout-core/src/ranked.rs) still sets workers to
min(threads, repos.len()) and assigns an entire repo per task. A query scoped to
chromium uses one worker even on the target two-CPU box. With two repos, unequal work
can leave one CPU idle after the smaller repo completes. Plain search already splits
verification into chunks within shards.

Share bounded chunk scheduling with ranked feature collection and merge deterministic
results before calculating the global corpus statistics. Ranking currently requests
whole documents; candidate-unit scoring needs separate proof of equivalent features.

Also move query-independent rank features to index time: content hash, line count,
longest line and static noise classification. features() currently scans matched
content again to count lines, find the longest line and hash it on each query.
The full FileFeatures collection is then cloned before sorting. Avoiding that clone
can reduce memory, but a small --top does not eliminate the exact ranking scan because
its score uses statistics over all matches.

Measure one-repo and skewed two-repo ranked searches separately. No ranked timing or
speedup was measured in this audit.

### 5. Bound doomed HTTP work earlier and measure the excluded broad cases

[run-ebs.sh](../bench/harness/run-ebs.sh) excludes if and return because their full
outputs are about 900 MB and 480 MB. The 16 MiB HTTP cap prevents returning all that
data, but does not establish how cheaply the server rejects the request.

[search_chunk](../crates/scout-core/src/search.rs) checks output limits after each
visited document/run; planning and eager decoding have already happened. Add checks
before expensive planning/decode boundaries and enforce the output budget while
printing, with a shared cancellation signal across workers. A sibling's output-limit
failure should stop new work immediately. Keep the existing all-or-error response
contract.

Add served benchmarks for if, return, long matching lines, rank=1&top=20, -l and -m1.
Record successful-request latency and 422/503 rejection latency, plus queue wait,
CPU, decoded bytes and peak memory. The HTTP deadline currently starts when the worker
handles the request, so it does not account for waiting for that worker.

If a small preview is desired, expose an explicit result-limited operation with
incompleteness metadata. Silently truncating the exact endpoint or calling the first
20 files the globally best 20 would change its contract. Streaming can improve time
to first result, but requires its own error/partial-response design.

## Avoid spending the first iteration here

- More HTTP workers: the recorded E17 2-CPU run went from 51.0 to 49.8 req/s with
  one to two workers; broad p99 worsened from 323 to 414 ms. Re-measure the larger
  corpus before changing the current one-worker default.
- Turning off compression: E29 already demonstrated that keeping the index resident
  was worth far more than its decoding cost on this corpus.
- Global mmap-advice changes: E30's 4-GiB warm geomean was 0.997, while its 2-GiB warm
  geomean regressed to 1.455. Fewer disk bytes alone do not prove lower latency.
- A larger stdout buffer: E27 already tested and rejected this.
- Another rarest-trigram heuristic as the main broad-literal fix: almost all of these
  candidates really match. E31 dictionary hints and E32 unit reads are already in progress.

## Acceptance for an implementation

First build and benchmark the current E31/E32 tree against a matching index, in its own
output directory. Then use interleaved A/Bs with the same binary workload, index codec,
CPU/memory caps and storage profile; format changes need separate matching indexes.

Track decoded bytes/blocks, candidate units, planning wall time, verification wall time,
formatting/output bytes, CPU and memory. Existing verify_ms includes several of these
costs and is summed thread time, not elapsed wall time. Without those extra counters,
the live mode comparison cannot attribute the cost specifically to lz4.

Use exact output hashes and the rg parity gate, including unit-boundary lines, UTF-16,
late NUL, -n, -l, -c and -m. Retain cold and warm runs, a serve-plus-sync memory run, and
mixed-load tail latency. No implementation speedup or current-tree parity is claimed here.

Relevant external design reference: [Zoekt's primary design documentation](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)
describes positional ngrams and staged evaluation. It informs possible designs;
the priorities above come from this repository's code and measurements.
