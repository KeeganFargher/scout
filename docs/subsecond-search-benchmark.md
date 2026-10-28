# Broad-query latency at 2 CPUs / 4 GiB (E34)

Measured on 2026-10-27 against the E33 lazy-decoding implementation, frozen before
this experiment. Both versions include the previous E31 count hints and E32
candidate-unit reads. This is a comparison against the code immediately before E34,
not E29 or the Git branch's original HEAD.

## Warm CLI results

All **19/19 warm query medians are below one second** at 2 CPUs / 4 GiB. Each case was
faster in all five paired rounds. Geomean after/before latency is **0.294**,
about **70.6% less latency** across this suite.

| Query / output             | Before ms | After ms | After / before | Rounds faster |
| -------------------------- | --------: | -------: | -------------: | ------------: |
| `std::unique_ptr`, lines   |     553.2 |    206.6 |          0.373 |           5/5 |
| `std::unique_ptr`, files   |     504.2 |    179.4 |          0.356 |           5/5 |
| `std::unique_ptr`, counts  |     538.0 |    194.9 |          0.362 |           5/5 |
| `std::unique_ptr`, `-m 1`  |     517.4 |    183.5 |          0.355 |           5/5 |
| License sentence, lines    |    1602.0 |    328.0 |          0.205 |           5/5 |
| License sentence, files    |    1552.0 |    300.5 |          0.194 |           5/5 |
| License sentence, `-m 1`   |    1561.9 |    305.1 |          0.195 |           5/5 |
| TODO / FIXME / HACK, lines |     715.6 |    373.2 |          0.522 |           5/5 |
| TODO / FIXME / HACK, files |     628.3 |    309.6 |          0.493 |           5/5 |
| Date regex, lines          |    2012.4 |    362.6 |          0.180 |           5/5 |
| Date regex, files          |    1923.9 |    282.9 |          0.147 |           5/5 |
| `if`, lines                |    4161.6 |    957.0 |          0.230 |           5/5 |
| `if`, files                |    3465.9 |    553.3 |          0.160 |           5/5 |
| `if`, `-m 1`               |    3495.0 |    578.0 |          0.165 |           5/5 |
| `return`, lines            |    2377.3 |    714.0 |          0.300 |           5/5 |
| `return`, files            |    2002.4 |    469.8 |          0.235 |           5/5 |
| `return`, `-m 1`           |    1987.2 |    475.8 |          0.239 |           5/5 |
| Selective symbol           |      26.7 |     24.7 |          0.925 |           5/5 |
| No-match literal           |      21.3 |     20.3 |          0.957 |           5/5 |

This is a median result, **not a strict one-second tail guarantee**. Full `if` output
took 930.2–1046.6 ms across ten samples; 9/10 were below one second.
The 1,046.6 ms sample was the only candidate sample above one second out of 190.
All **380 timed samples had zero recorded disk-read bytes**.

The index grows from **2,354.5 to 2,863.4 MiB (+21.6%)**. The after batches peak at
**1,935 MiB charged memory**, versus 1,870 MiB before, with zero OOM kills. Those are
batch peaks including cache and output-hash warm-ups, not a production-service memory
estimate or a guarantee that every byte of either index was cached.

## Implementation

- Indexed plain text formats paths, line numbers, lines, filenames and counts directly
  into the chunk's output buffer. Exact case-sensitive literals without word boundaries
  use `memmem`; other patterns keep the regex searcher. Binary/BOM reader-mode documents
  use rg's reader/printer. Limits and counts span every visited block of a file.
- The bounds-checked reference LZ4 decoder writes into the retained initialized scratch
  buffer. Its result must fill the requested slice exactly. Compressed blocks beyond
  its signed 32-bit length limit are rejected before allocation. The Rust block encoder
  remains in use.
- Native format v9 adds tagged two-byte postings and a 32-byte summary per document.
  The former lets `if` reject files/units without that pair. The latter filters numeric
  regexes whose ordinary gram plan is unconstrained, before decoding their candidates.
- The numeric summary normalizes ASCII letters to one symbol and ASCII digits plus all
  non-ASCII Unicode characters to another. This deliberately broad treatment keeps
  Unicode digits sound without depending on matching Unicode table versions. Only
  four-character shapes with at least two digit-like symbols consume Bloom bits.
  Bounded HIR projection derives necessary prefixes; short alternatives and unsupported
  raw-byte projections disable this filter. Reader-mode documents are always retained.
- Per-chunk path and decode storage are reused. Printed output buffers are recycled
  with at most four buffers of at most 8 MiB retained. Prefetch spans merge across gaps
  of at most 4 KiB, reducing repeated advice calls at the cost of prefetching the gaps.
- Blocks smaller than 4 KiB remain raw even with `--content lz4`, so small documents can
  borrow the mmap without a decoder call or content copy. Metadata from the first v9
  index predicted 427,398 avoided decodes for 258 MiB more stored content across this corpus.

Native and SQLite indexes from previous builds must be rebuilt with `scout index`.
No compatibility reader is provided. The format change is required for the new postings
and summaries; the small-block storage policy uses the existing raw/LZ4 block encoding.

For this corpus, `if` candidates fall from 697,675 to **377,412** files and date-regex
candidates to **56,542**. Every survivor is still checked against actual contents.

## Measurement boundary

WSL2 Linux; chromium + llvm-project; 697,675 files and 5,126 MiB of indexed content.
The before arm uses the E33 v7 index and the after arm a freshly built v9 hybrid index.
The new index was built under 2 CPUs / 4 GiB, with a 1 GiB indexing budget, no swap,
and EBS-like 125 MB/s / 3,000 IOPS disk caps. The build completed without an OOM kill.
Build time is not a controlled before/after comparison.

Each benchmark batch runs in its own fresh cgroup: CPUs 0–1, a 200% CPU quota, 4 GiB
memory, no swap, and the same disk caps. An arm evicts only its own index and verifies
at most 2 MiB resident before warming it inside that cgroup. Separate cgroups prevent
two different index layouts from competing for a single server's memory budget.
The first probe immediately after indexing correctly refused to run because 29.7 MiB
of dirty pages could not be evicted; writeback was completed before the fresh probe.
The eviction guard was retained.

Five alternating rounds, two timed samples per query/arm/round, deterministically
shuffled query order. Fresh CLI processes use `--freshness off --threads 2 -n` and
query-specific flags. Timed stdout goes to `/dev/null`; output hashing happens in
separate untimed passes with bounded buffers. This measures warm CLI search and
formatting. It does not measure cold latency, HTTP concurrency, network transfer or
terminal rendering. In particular, transmitting the full `if` result is additional work.

## Cold-index follow-up

Cold broad queries take **3.55–14.58 seconds** on the same 2 CPU / 4 GiB setup.
The subsecond broad-query result depends on a warm index cache under these disk limits.

Three timed runs per case, with **every index file evicted before every run** and
**0 MiB resident verified by `mincore` in all 18 samples**. Each sample also records
positive disk reads and major faults. CPU pinning/quota, memory, no swap, freshness off,
fully formatted stdout to `/dev/null`, 125 MB/s / 3,000 IOPS and 128 KiB disk read-ahead
match the warm measurement. The executable stays warm; this is cold index data, not
a full-machine boot. The WSL EBS-like volume has no additional injected I/O delay;
these are bandwidth/IOPS-limited local measurements, not measurements from an AWS VM.

| Query / output          | Warm median ms | Cold median s |  Cold range s | Cold disk read MiB |
| ----------------------- | -------------: | ------------: | ------------: | -----------------: |
| `if`, full lines        |          957.0 |        14.579 | 14.483–14.583 |             1674.1 |
| `return`, full lines    |          714.0 |        12.175 | 12.094–12.179 |             1314.4 |
| License sentence, lines |          328.0 |         6.589 |   6.519–6.644 |              790.4 |
| Date regex, lines       |          362.6 |         3.551 |   3.509–3.555 |              395.0 |
| Selective symbol        |           24.7 |         0.700 |   0.650–0.816 |               95.5 |
| No-match literal        |           20.3 |         0.515 |   0.407–0.517 |               60.4 |

Disk I/O dominates: full `if` output reads about **1.64 GiB** from the index, with
3.12 seconds of CPU time versus 14.58 seconds wall time. The run peaks at **1,811 MiB
charged memory**, including the separate parity passes, with zero OOM kills.

All **six cold-output hash/byte-count/exit-code checks match the accepted warm
results**. These checks run separately after evicting the index again; hashing is
not included in the timed searches. The original task-start comparison below adds a matching cold sweep of the
original build; these initial final-build measurements alone were not a comparison.

[Cold samples and output hashes](../bench/results/E34-subsecond/cold/summary.json),
[progress and resource log](../bench/results/E34-subsecond/cold.log), and
[cold harness](../bench/harness/cold-subsecond.py).

## Original task-start comparison

This comparison starts **before both E33 lazy decoding and E34**, using the frozen
original executable and its v7 index. The earlier warm table starts after E33 instead.
All **19/19 measured warm medians are lower** than the task-start values; the geomean
ratio is **0.284**, about **71.6% less latency**.

**Cold is mixed.** Date lines improve by about 75% and `return` lines by about 49%,
but full `if` is 1.4% slower and license lines 2.8% slower on measured cold medians.
The no-match median is also higher. The selective-query ranges overlap between the
three-run sweeps, so those differences should not be treated as decisive.

| Query / output          | Warm originally ms | Warm now ms | Cold originally s | Cold now s |
| ----------------------- | -----------------: | ----------: | ----------------: | ---------: |
| `if`, full lines        |             4225.6 |       957.0 |            14.375 |     14.579 |
| `return`, full lines    |             2424.6 |       714.0 |            23.962 |     12.175 |
| License sentence, lines |             1590.3 |       328.0 |             6.411 |      6.589 |
| Date regex, lines       |             2162.3 |       362.6 |            14.374 |      3.551 |
| Selective symbol        |               27.6 |        24.7 |             0.738 |      0.700 |
| No-match literal        |               20.8 |        20.3 |             0.462 |      0.515 |

The original and final builds each have three cold samples per case and a separate
cold output-parity pass. All six output hashes/byte counts/exit codes match across
builds. **35/36 timed cold samples start at zero index residency**; the remaining
original date sample starts with 128 KiB resident, below the unchanged 2 MiB guard.
All samples record positive disk reads and major faults. Both builds use the same CPU,
memory, disk caps and read-ahead. These are separate sweeps, not an interleaved
original/final experiment; warm medians also come from separate campaigns. Tiny
differences, especially the 0.4 ms warm no-match change, are not a strong performance claim.

The index is 21.6% larger. Cold license reads rise **685.9 → 790.4 MiB**, illustrating
the extra-I/O side of the raw-small-block/cache-space tradeoff. The original cold run
peaks at 1,794 MiB charged memory and the final run at 1,811 MiB, with zero OOM kills.
These results establish major warm gains and some cold gains, not a universal cold
latency improvement.

[Original-versus-final comparison](../bench/results/E34-subsecond/original-comparison.json),
[original cold samples](../bench/results/E34-subsecond/cold-original/summary.json),
[original cold resource/parity log](../bench/results/E34-subsecond/cold-original.log),
and [explicit original-build manifest](../bench/results/E34-subsecond/cold-original-reference.json).

## Validation

The corpus parity gate compares complete output byte counts, SHA-256 hashes and exit
codes for all 19 queries across index versions. It includes 811,859,623 bytes of `if`
output and 428,643,537 bytes of `return` output, with no truncation or result cap.
The separate rg oracle checks six broad queries after timing and lifts disk throttling
for rg's source-file reads while retaining the 2 CPU / 4 GiB limit.

**Both gates pass: 19/19 byte-identical A/B cases and 6/6 rg oracle cases.** The rg cases
are common lines, license filenames, TODO counts, Unicode date lines, `if` filenames
and `return -m 1`. Oracle timings are excluded from the latency comparison.

Full Rust workspace tests passed on Windows and Linux; release Clippy passed with all
targets. Tests cover raw native, LZ4 native and SQLite; file-wide counts and match limits;
separated candidate units; CRLF and unterminated lines; invalid UTF-8; Unicode digits;
short alternatives and byte regexes; binary/BOM readers; and corruption/decoder bounds.
The direct text formatter is compared to the stock rg printers across output, case,
word, line-number and match-limit combinations. The small-file test verifies that a
4,095-byte compressible file leaves decode storage untouched while a 4,096-byte file
still decodes correctly.

## Evidence and reproduction

- [Final A/B samples, output hashes and binary/catalog fingerprints](../bench/results/E34-subsecond/ab-final/summary.json)
- [A/B timing table](../bench/results/E34-subsecond/ab-final.log)
- [Independent rg parity](../bench/results/E34-subsecond/ab-final/rg-parity.json)
- [Incremental implementation patch](../bench/results/E34-subsecond/implementation.patch)
- [Source fingerprints against the frozen baseline](../bench/results/E34-subsecond/source-evidence.json)
- [New index build and resource report](../bench/results/E34-subsecond/index-hybrid.log)
- [Windows tests](../bench/results/E34-subsecond/windows-final-tests.log),
  [Linux tests](../bench/results/E34-subsecond/linux-final-tests.log),
  [release Clippy](../bench/results/E34-subsecond/clippy.log)

The source snapshots are `/home/keegan/scout-subsecond-base-20261027` and
`/home/keegan/scout-subsecond-after-20261027`. Workspace compilation sources were checked
against the candidate snapshot with line endings normalized. Previously modified
vendored searcher files were also checked against both snapshots. The incremental
patch isolates E34 from the working tree's existing changes.

```powershell
wsl -d Ubuntu -u root -- bash "/mnt/c/Users/User/source/test space/scout/bench/harness/run-subsecond-ab.sh" /home/keegan/small/bin-lazy-after/scout /mnt/ebs/idx-lazy-v7-20261027 /home/keegan/small/bin-subsecond-accepted/scout /mnt/ebs/idx-subsecond-v9-hybrid-20261027 "/mnt/c/Users/User/source/test space/scout/bench/results/E34-repeat"
wsl -d Ubuntu -u root -- bash "/mnt/c/Users/User/source/test space/scout/bench/harness/run-subsecond-parity.sh" /home/keegan/small/bin-subsecond-accepted /mnt/ebs/idx-subsecond-v9-hybrid-20261027 "/mnt/c/Users/User/source/test space/scout/bench/results/E34-repeat" /home/keegan/small/bin/rg
```

For the cold-index follow-up, also use a new output directory:

```powershell
wsl -d Ubuntu -u root -- bash "/mnt/c/Users/User/source/test space/scout/bench/harness/ebs-run.sh" 2 4G 125 3000 python3 "/mnt/c/Users/User/source/test space/scout/bench/harness/cold-subsecond.py" "/mnt/c/Users/User/source/test space/scout/bench/results/E34-subsecond/ab-final/summary.json" "/mnt/c/Users/User/source/test space/scout/bench/results/E34-cold-repeat" 3
```

Output directories must be new. The result directory also retains earlier probes and
an interrupted A/B that still exceeded one second for full `if` output. Those are
exploratory evidence; the final warm comparison is exclusively `ab-final/` and the
cold-index follow-up is `cold/`.
