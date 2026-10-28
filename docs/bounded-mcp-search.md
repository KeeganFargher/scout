# Bounded interactive search at 2 CPUs / 4 GiB (E35)

An explicit bounded search now stops planning and decoding when it reaches a global
match cap, output cap or processing budget. On a 250-repo, 8.12 GiB stress fixture,
cold HTTP requests returned 100 `if` lines in **6.9 ms**, `return` in **11.4 ms**,
and `export` in **34.4 ms** (three-run medians).

Exact counts still need exhaustive work. The same fixture's cold `if` count found
**8,585,236 matching lines in 26.02 seconds**. With a one-second processing budget,
the bounded count instead returned **at least 291,584**, explicitly incomplete.
It does not present a partial count as an exact total.

This implements the Rust API, CLI and HTTP path for an interactive caller. The
repository's TypeScript MCP package remains a scaffold; an MCP protocol adapter
has not been implemented.

## Contract and usage

```bash
# First matching lines, across all selected repos; defaults: 64 KiB and 1 second.
scout search --freshness off --max-results 100 -n if

# Count up to a threshold: "at least 100" if the threshold is reached.
scout search --freshness off --max-results 100 -c if

# Spend up to one second counting, with a deliberately high threshold.
scout search --freshness off --max-results 1000000000 --budget-ms 1000 -c if

# Existing exact, per-file grep counts remain exhaustive.
scout search --freshness off -c if
```

`--result-bytes` and `--budget-ms` require `--max-results`. Every limit must be
positive. Bounded search requires `--freshness off`, meaning the published index
snapshot. Strict filesystem freshness checks can themselves require a full tree
walk. No index format change or rebuild is needed for this addition to E34/v9.

Line output is a byte-for-byte prefix of the usual unranked output, ending between
complete records. Completion metadata goes to stderr:

```text
scout-bounded {"complete":false,"stop_reason":"match_limit","returned_matches":100,"elapsed_ms":6.2}
```

Bounded `-c` writes one JSON object to stdout. It counts matching **lines**, as
ordinary `-c` does, even when a line contains several occurrences:

```json
{
  "count": { "kind": "lower_bound", "value": 291584 },
  "complete": false,
  "stop_reason": "deadline"
}
```

An exhausted search returns `kind: "exact"`, `complete: true`, and
`stop_reason: "complete"`. The possible stop reasons are `complete`, `match_limit`,
`byte_limit` and `deadline`. Reaching exactly N results does not prove that more
exist, so hitting N is conservatively incomplete. A deadline with no results is
**a lower bound of zero**, not proof of no matches. A valid bounded CLI response
exits successfully, including incomplete or empty batches; ordinary CLI exit
conventions are unchanged.

HTTP exposes the same operation:

```bash
curl -i 'http://127.0.0.1:7700/search?q=if&n=1&limit=100&max_bytes=65536&timeout_ms=1000'
curl -i 'http://127.0.0.1:7700/search?q=if&c=1&limit=1000000000&timeout_ms=1000'
```

Bounded responses include `X-Scout-Complete`, `X-Scout-Stop-Reason` and
`X-Scout-Returned-Matches`. Lines remain text; bounded counts are
`application/json` with the count object above. `max_bytes` and `timeout_ms`
require `limit`; defaults are 65,536 and 1,000. The server's own output/time limits
also apply, taking the smaller budget. Count metadata needs at least 256 bytes.
Invalid parameters or patterns return HTTP 400; genuine I/O/index failures remain
errors rather than successful partial responses.

`-m` / HTTP `m` still limits matches **per file**. The bounded global limit applies
in addition. Ranking and files-with-matches output cannot be combined with bounded
mode. Results follow stable traversal order; they are not the globally best N
ranked files. There is no continuation cursor in this version. Narrow by `repo`
or refine the query when a batch is incomplete.

Rust callers use `scout_core::search::search_bounded`, `BoundedOptions`,
`BoundedReport` and the tagged `CountEstimate::{Exact, LowerBound}`. An MCP adapter
must carry the completion and count-kind fields into its tool response. Small
output alone is insufficient to infer search completion.

## Why it avoids the broad-query cliff

The exhaustive path plans every selected shard and verifies all candidates.
Bounded mode plans one shard at a time and visits native content blocks only until
the batch stops. It reuses decode scratch and path storage, with no speculative
whole-file read-ahead. A fixture test corrupts a later shard's candidate mapping:
the bounded first result succeeds without planning it, while exhaustive search
reports the corruption.

One worker keeps the prefix deterministic and avoids speculative work or a result
reordering queue. Another bounded request can use the other CPU. This experiment
measures sequential requests; concurrent-client tail latency has not been measured.

The byte cap is checked before formatting a matching line. A single 20 MB line
can therefore produce an empty `byte_limit` batch, explicitly incomplete. It is
not silently clipped and does not mean no match. A separate test counts that line
exactly. Native blocks still must be read/decompressed before matching them.

Reader-mode and whole-haystack anchor expressions preserve whole-document
semantics. For reader-mode counts, matches remain tentative until binary detection
finishes: ordinary grep suppresses a binary file's count after finding a late NUL.
An interrupted reader therefore cannot inflate a reported lower bound.

The deadline is **cooperative processing time**, checked between planning steps,
files, blocks and match callbacks. A blocking read, planning a shard, decoding a
large block or matching within one block can overshoot it. Queue wait, CLI index
open/close and network delivery are outside this core budget. The sparse scan below
took 1.008 seconds with a 1.000-second budget. This is not a hard end-to-end SLA.

## Measured workload and limits

The fixture cycles real plain-text documents from the existing Chromium/LLVM index,
repartitions them into **250 repo groups**, and writes each document independently
with native v9 LZ4. It contains **1,131,349 document visits, 8,714,175,268 content
bytes (8.12 GiB)** and a **4.88 GiB index**, larger than the memory limit. Duplication
does not receive cross-file compression. Reader-mode documents are excluded from
this stress fixture and exercised separately in the backend tests.

This is a synthetic size/repo-count stress test, **not 250 actual NX repositories**.
Language mix, file order, shard size and storage latency affect the result. The
separate 20 MB single-line test checks large-file behavior; these numbers do not
establish latency for every possible 10–20 MB file or pattern.

The actual `serve` command requires a sync config, so one tiny healthy local Git
control repo is appended after the 250 fixture repos: the server serves 251 entries.
Its sync uses only a local file URL. The measured process runs with:

- 2 CPU quota and affinity to CPUs 0–1; 4 GiB memory limit; no swap.
- 125,000,000 bytes/s and 3,000 IOPS storage caps; 128 KiB read-ahead.
- Direct-I/O loop → dm-delay → ext4, with zero extra injected delay. These are local
  EBS-like throughput/IOPS limits, not a claim of measured EC2 gp3 latency.
- A persistent HTTP server, two configured HTTP workers and two exhaustive-search
  threads. Bounded searches use one worker internally.

Before each cold request, server cgroup reclaim plus file advice evicts index pages;
`mincore` verifies **0 MiB resident in all 33 cold samples**. Heap filters/fences
loaded when opening the server stay live. Thus an unseen query still benefits
from shared metadata; this is cold index file cache in a running service, not a
machine boot or new server process. Startup through the local sync readiness check
was separately **1.32 seconds**. Peak server cgroup memory was **667 MiB**, zero OOM
kills; the exhaustive oracle peaked at **3,235 MiB**, also zero OOM kills.

## Final results

Every line query requests at most 100 matching lines, 64 KiB, and 1,000 ms. Cold
medians are three requests per case, shuffled between rounds. Warm figures are a
single request after priming each case, useful for orientation, not tail estimates.
Cold read volume is the same across the three samples of each case.

| Query / mode                  | Cold median ms | Warm ms | Result                              | Cold MiB read |
| ----------------------------- | -------------: | ------: | ----------------------------------- | ------------: |
| `if` lines                    |            6.9 |     0.9 | 100, match limit                    |           0.8 |
| `return` lines                |           11.4 |     1.1 | 100, match limit                    |           1.4 |
| `export` lines                |           34.4 |     1.5 | 100, match limit                    |           5.5 |
| `TODO\|FIXME\|HACK` lines     |           17.9 |     1.2 | 100, match limit                    |           2.6 |
| `\d{4}-\d{2}-\d{2}` lines     |          119.7 |    11.5 | 100, match limit                    |          13.4 |
| `getAsCXXRecordDecl` lines    |          455.8 |    12.9 | 100, match limit                    |          54.5 |
| Absent literal                |          152.8 |     1.0 | Complete, zero                      |          19.3 |
| `^[A-Za-z_]{173}$` lines      |        1,008.1 | 1,016.3 | Deadline, zero observed, incomplete |         119.4 |
| `if` count capped at 100      |            6.6 |     0.8 | Lower bound 100                     |           0.8 |
| `if` count, one-second budget |        1,006.5 | 1,000.8 | Cold lower bound 291,584            |         119.4 |
| Absent literal count          |          156.2 |     1.1 | Exact zero                          |          19.3 |

The independent cold exhaustive `if -c` reference took **26,017 ms**, counted
**8,585,236 lines across 638,795 files**, and read **2.93 GiB**. A threshold/lower
bound is a different answer from this exact total; the latency numbers must not be
treated as an exact-count speedup.

## Correctness and reproducibility

- All **19 exhaustive outputs** match E34's frozen byte counts, SHA-256 hashes and
  exit codes on the original 5 GiB index after the planner helper refactor.
- All seven completed/nonempty HTTP line cases match exhaustive output prefixes
  for the warm sample and all three cold samples. The sparse scan's empty,
  incomplete prefix is recorded separately; it does not prove absence of matches.
- Every bounded `if` count is no greater than the exhaustive total. Absent counts
  are exact zero only after exhaustion.
- Raw native, LZ4 native and SQLite tests compare complete output and partial
  prefixes, including CRLF, invalid UTF-8, UTF-16, late NUL, unterminated lines,
  per-file limits, multi-block files, anchors and complete-record byte limits.
- Full Linux release workspace tests passed. Windows release bounded/HTTP/core
  tests and workspace release Clippy with `-D warnings` passed. Earlier full
  Windows release tests also passed before the final validation-only change.
- All 57 checked Rust/manifests/vendored compile inputs match the source snapshot
  used for the final binary. The nine-file incremental patch is against E34's
  accepted source snapshot, so earlier working-tree changes remain identifiable.

Accepted binary SHA-256:
`0ff90c603f5c33f72af9635390453c63f31d5b69d8878017785a31490b23a4ed`.

Artifacts: [HTTP samples](../bench/results/E35-bounded/http-final/summary.json),
[HTTP resource log](../bench/results/E35-bounded/http-final/server.log),
[count/prefix oracle](../bench/results/E35-bounded/oracle-final.json),
[exhaustive parity](../bench/results/E35-bounded/exhaustive-parity.json),
[source evidence](../bench/results/E35-bounded/evidence/source-evidence.json),
[incremental patch](../bench/results/E35-bounded/evidence/implementation.patch).
The earlier `http/` and `oracle.json` files are exploratory runs; acceptance uses
the `*-final` artifacts and binary hash above.

The fixture builder is `cargo run --release -p scout-index-native --example
repartition -- <source-index> <new-index> 250 8`. It refuses an existing destination.
For the HTTP controller, use an existing sync config containing the same local
`zz-control` repo across runs; the fixture's published catalog now includes it.
Run as root for cgroup controls:

```bash
python3 bench/harness/bounded-http.py "$SCOUT" "$INDEX" "$NEW_OUT" "$SYNC_CONFIG" 3
bash bench/harness/ebs-run.sh 2 4G 125 3000 python3 bench/harness/bounded-oracle.py \
  "$SCOUT" "$INDEX" "$NEW_OUT/summary.json" "$NEW_ORACLE_JSON"
bash bench/harness/ebs-run.sh 2 4G 125 3000 python3 bench/harness/bounded-parity.py \
  "$SCOUT" "$E34_INDEX" bench/results/E34-subsecond/ab-final/summary.json "$NEW_PARITY_JSON"
```

The measurement scripts require new output paths and fail when the cold-residency,
resource, byte-prefix, binary-hash or exact-count checks fail.
