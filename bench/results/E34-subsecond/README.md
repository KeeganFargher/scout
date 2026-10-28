# E34 evidence

The accepted comparison is `ab-final/summary.json`: E33 v7 versus the final v9 hybrid
index, five alternating rounds of two samples per arm/query, 19 queries at 2 CPUs /
4 GiB. All 380 timed samples report zero disk-read bytes; all 19 output hashes, byte
counts and exit codes match. Each case is faster in every paired round. All 19 after
medians are below one second. Full `if` is 957.0 ms median, range 930.2–1,046.6 ms;
one of its ten samples exceeds a second. This was the only after sample over a second
out of 190.

`ab-final/rg-parity.json` and `.log` contain the separate six-case rg gate: ALL OK.
It runs after timing, at 2 CPUs / 4 GiB, with disk caps lifted for source-file reads.
Its timing data is not part of the controlled comparison.

`cold/summary.json` and `cold.log` are the cold-index follow-up on the final binary:
three runs per case, all 18 starting from verified zero index residency. At the same
2 CPU / 4 GiB and 125 MB/s / 3,000 IOPS limits, full `if` is 14.58 s median,
`return` 12.18 s, license lines 6.59 s and date lines 3.55 s. Selective symbol and
no-match medians are 700 / 515 ms. All six separate cold-output checks match the warm
hashes, byte counts and exit codes. The executable stays warm, disk read-ahead is
128 KiB, and the device has no extra injected delay. Peak charged memory is 1,811 MiB,
no OOM kills.

`cold-original/summary.json` and `cold-original.log` add the original task-start build
(before E33 and E34) under the same limits. `cold-original-reference.json` selects
that original executable and v7 catalog using fingerprints from `E33-lazy/warm.json`;
its `after` slot means the build under test, explicitly the original build here.
`original-comparison.json` combines the original/final warm and cold medians and keeps
the source report paths. All 19 final warm medians are lower. Cold date / return lines
improve 75.3% / 49.2%, but full `if` / license medians are 1.4% / 2.8% higher and the
no-match median is 11.5% higher. Selective-query ranges overlap across the three-run
sweeps. These are separate sweeps, not interleaved pairs. All six original cold-output
checks match the final outputs. Of the 36 timed cold samples, 35 have zero starting
index residency; one original date sample has 128 KiB, below the unchanged 2 MiB guard.

`source-evidence.json` fingerprints the changed compilation inputs against the frozen
E33 source, with line endings normalized, and verifies the candidate snapshot matches
the workspace. `implementation.patch` isolates E34 from the pre-existing working-tree
changes. Full final Windows/Linux test logs and release Clippy output are retained.

`index-hybrid.log` is the final v9 hybrid index build: 2 CPUs / 4 GiB, 125 MB/s /
3,000 IOPS, 271.9 seconds, no OOM kill. Other compiles ran during that build, so its
time is not a controlled indexing comparison. Its peak includes reclaimable cache.
The native file layout is v9 for both `index-v9.log` and `index-hybrid.log`; the latter
changes only the writer's raw-storage cutoff for blocks below 4 KiB.

Earlier exploratory evidence remains separate:

- `text-probe.*`: direct plain-text formatting on the old v7 layout.
- `scan-*.log`: metadata, decoder and byte-search probes. The ASCII date regex lost
  a Unicode occurrence and was not adopted.
- `index.log`, `probe-v8.*`: the early shape-summary prototype, before tagged pairs.
- `index-v9.log`, `probe-v9.*`: tagged pairs and numeric shape summaries.
- `probe-path-reuse.*`, `probe-recycle.*`, `probe-prefetch.*`: intermediate allocation
  and prefetch changes. Full `if` output still exceeded one second.
- `probe-short.*`, `probe-inline.*`: literal-loop and inlining variants without a
  useful improvement; both variants were reverted.
- `ab/`, `ab.log`: interrupted early A/B. It is incomplete and is not the final result.
- `probe-hybrid.log`: the eviction guard refused 29.7 MiB of dirty resident pages
  immediately after indexing. No samples were taken. After writeback completed,
  `probe-hybrid-clean.*` started from zero residency and passed the probe.

The report and reproduction commands are in `docs/subsecond-search-benchmark.md`.
The A/B timings apply to warm CLI searches with freshness off and fully formatted
stdout directed to `/dev/null`. The separate `cold/` follow-up evicts the index before
each query. Network transfer and HTTP load are outside both results. The new index is
21.6% larger, and previous indexes require rebuilding.
