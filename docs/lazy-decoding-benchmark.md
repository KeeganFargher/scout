# Lazy decoding and decode-buffer reuse (E33)

Implemented and measured on 2026-10-27 against a snapshot of the working tree at the
start of this task, including its E31 count hints and E32 candidate-unit reads.
The comparison is **not against E29**.

## What changed

- Native compressed content is visited one line-aligned block at a time. Returning
  `false` stops before decoding the next payload. Raw spans are borrowed from the mmap.
- One printer sink spans the entire file. Counts, filenames and `-m` limits remain
  file-wide even when matches occur in separated candidate units. `-c` and `-m` can now
  use candidate-unit reads as well as normal line output.
- Initialized decode buffers retain their length between reads. Only growth initializes
  new bytes; every returned slice is fully overwritten and length-checked. Shorter
  results cannot expose the retained tail.
- Binary/BOM documents and patterns with haystack anchors use whole-document matching.
  Ranking still scores whole documents, with the shared buffer-reuse improvement.
- The shard format remains v7. Both arms searched the **same freshly built index**.

The read-ahead window still queues candidate content ahead of verification. This change
establishes lazy **decoding**; it does not promise that disk prefetch stops at the first
matching block.

Subsequent work: [E34 broad-query optimization](subsecond-search-benchmark.md) builds
on these changes and reaches subsecond warm medians across all 19 cases. E33's modest
gain below measures lazy decoding and buffer reuse alone.

## Benchmark

WSL2 Linux, chromium + llvm-project, 697,675 files, 5,126 MiB of indexed content and a
2,354 MiB LZ4 v7 index. Each run used two pinned CPUs, a 200% CPU quota, a 4 GiB memory
limit, no swap, and the existing EBS-like volume capped at 125 MB/s and 3,000 IOPS.

The harness evicted only this test index and verified **0 MiB resident** before warm-up,
so its pages were populated from the limited cgroup. It first streamed and compared
both arms' complete output hashes, then ran five interleaved rounds, two timed samples
per arm/query/round, alternating arm order and deterministically shuffling queries.

There were **380 timed samples**, all with **zero disk-read bytes**. Timed stdout went
to `/dev/null` to isolate CLI search/formatting work. Network transfer, HTTP queueing,
cold latency and serve-plus-sync concurrency were not measured.

| Query / output             | Before ms | After ms | After / before | Rounds faster |
| -------------------------- | --------: | -------: | -------------: | ------------: |
| `std::unique_ptr`, lines   |     552.3 |    542.8 |          0.983 |           4/5 |
| `std::unique_ptr`, files   |     522.9 |    506.5 |          0.969 |           4/5 |
| `std::unique_ptr`, counts  |     561.6 |    529.2 |          0.942 |           5/5 |
| `std::unique_ptr`, `-m 1`  |     543.9 |    503.2 |          0.925 |           5/5 |
| License sentence, lines    |    1590.3 |   1597.2 |          1.004 |           4/5 |
| License sentence, files    |    1561.9 |   1552.9 |          0.994 |           4/5 |
| License sentence, `-m 1`   |    1628.2 |   1560.6 |          0.959 |           5/5 |
| `TODO\|FIXME\|HACK`, lines |     717.1 |    697.1 |          0.972 |           5/5 |
| `TODO\|FIXME\|HACK`, files |     659.9 |    626.2 |          0.949 |           5/5 |
| Date regex, lines          |    2162.3 |   2028.9 |          0.938 |           5/5 |
| Date regex, files          |    2115.4 |   1934.6 |          0.915 |           5/5 |
| `if`, lines                |    4225.6 |   4118.9 |          0.975 |           5/5 |
| `if`, files                |    3727.0 |   3516.7 |          0.944 |           5/5 |
| `if`, `-m 1`               |    3765.6 |   3522.7 |          0.935 |           5/5 |
| `return`, lines            |    2424.6 |   2408.2 |          0.993 |           3/5 |
| `return`, files            |    2092.6 |   1999.9 |          0.956 |           5/5 |
| `return`, `-m 1`           |    2173.0 |   2025.7 |          0.932 |           5/5 |
| Selective symbol           |      27.6 |     27.1 |          0.979 |           4/5 |
| No-match literal           |      20.8 |     21.3 |          1.027 |           1/5 |

Geomean after/before **0.962**, about **3.8% less latency** across this suite.
Twelve cases improved in every round. The largest reduction was 8.5% for date-query
filenames. Common-query counts and match-limited queries improved 5.8–7.5%.

The gains are modest: full license and `return` output were effectively unchanged.
The no-match case regressed 0.6 ms / 2.7%; it has no content to decode, so this workload
does not establish a benefit there. These results measure both changes together and
do not attribute a separate percentage to buffer reuse versus lazy decoding.

The entire A/B, including output-hash warm-ups, peaked at **1,874 MiB of charged memory**
with zero OOM kills. This is one benchmark cgroup's peak, not a before/after memory
comparison or a production service memory estimate.

## Correctness

All **19 cases produced byte-identical output and exit codes** between arms. Hashing
was streamed rather than retaining output in RAM, including 811,859,623 bytes for `if`
and 428,643,537 bytes for `return`.

The separate broad-query rg gate **passed all six cases**: common lines, license
filenames, TODO counts, date lines, `if` filenames and `return -m 1`. The results are
recorded in `rg-parity.json` / `rg-parity.log` in the results directory. It uses the
existing scout-bench oracle, including its per-file reverification for ripgrep's
late-NUL reader-buffer behavior.
It retains the two-CPU / 4 GiB limits but lifts the disk throttle: the first capped
attempt took 172 seconds for rg's first query, passed that query, and was stopped.
That partial attempt is retained as `rg-parity-io-capped.log`. It is not a completed gate.

Full Rust workspace tests passed on Windows and Linux. Release Clippy passed with
all targets. New tests exercise:

- Stopping before a corrupt later LZ4 payload is decoded, for whole and selected-unit visits.
- Reusing initialized storage across large, small and empty reads without stale bytes.
- Line numbers, separated units and an unterminated last line.
- Counts and filenames once per file, repeated occurrences on one line, and `-m 0/1/2/3`.
- Raw native, LZ4 native and SQLite behavior, with one and two search threads.
- Existing whole-document handling for reader-mode files and rg-style anchor semantics.

## Reproduction and evidence

- [Raw timing samples and output hashes](../bench/results/E33-lazy/warm.json)
- [Timing table and cgroup resource report](../bench/results/E33-lazy/warm.log)
- [Independent rg parity results](../bench/results/E33-lazy/rg-parity.json)
- [Incremental implementation diff against the frozen baseline](../bench/results/E33-lazy/implementation.patch)
- [Benchmark harness](../bench/harness/lazy-ab.py) and [workload](../bench/queries-lazy.tsv)
- [rg parity workload](../bench/queries-lazy-parity.tsv)

The raw report records both binary hashes and the index catalog hash. The source
snapshots are `/home/keegan/scout-lazy-baseline-20261027` and
`/home/keegan/scout-lazy-after-20261027` in WSL. The latter's final source differs from
the working tree only in a test newline-counting helper and formatter-only changes;
the production behavior benchmarked is the implementation described here.

Run with a new output directory; the harness refuses to overwrite existing evidence:

```powershell
wsl -d Ubuntu -u root -- bash "/mnt/c/Users/User/source/test space/scout/bench/harness/run-lazy-ab.sh" /home/keegan/small/bin-lazy-before/scout /home/keegan/small/bin-lazy-after/scout /mnt/ebs/idx-lazy-v7-20261027 "/mnt/c/Users/User/source/test space/scout/bench/results/E33-lazy-repeat"
wsl -d Ubuntu -u root -- bash "/mnt/c/Users/User/source/test space/scout/bench/harness/run-lazy-parity.sh" /home/keegan/small/bin-lazy-after /mnt/ebs/idx-lazy-v7-20261027 "/mnt/c/Users/User/source/test space/scout/bench/results/E33-lazy-repeat" /home/keegan/small/bin/rg
```
