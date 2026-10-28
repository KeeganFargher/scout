# Small-server harness

The Linux scripts behind the interleaved A/Bs (E16 on) and the cold/serve measurements
(E21 on). They run under WSL2 Ubuntu or any Linux with cgroup v2 and a systemd user
session, without root.

| script                                                      | what it does                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `prof.sh <cpus> <mem> <cmd...>`                             | runs a command in a transient systemd scope: `CPUQuota`, `MemoryMax` with no swap, pinned to CPUs `0..cpus-1`; prints the scope's peak memory and OOM kills                                                                                                                                                                                                                                                                                               |
| `ab.sh`                                                     | interleaved A/B of two builds with `scout-bench` per profile (rounds alternate which arm goes first); an arm can name its own index (`arm=bin@index`, for a format change); `abagg.py` summarises it                                                                                                                                                                                                                                                      |
| `stats-ab.sh`                                               | where an A/B's gap lives, for the queries it read slower: `scout search --stats` per query and profile, pairs in alternating order, with the output piped as scout-bench reads it and the whole process timed around it; `statsabsum.py` summarises candidates, postings, verification, the search's total and the process's wall time                                                                                                                    |
| `qbench.py`                                                 | times the bench queries one request at a time against `scout serve` or the CLI, warm or **cold**; cold evicts the index before every request and fails unless `mincore` shows at most 2 MiB of it resident and the request then takes a major fault. Records each request's major faults and MiB read from disk, and the index disk's read-ahead (see below; the run fails if it changes)                                                                 |
| `pagecache.py`                                              | `mincore` residency, `posix_fadvise` eviction and the read-ahead of a path's disk, for `qbench.py` and the shell scripts                                                                                                                                                                                                                                                                                                                                  |
| `run-e21.sh`                                                | E21: CLI and serve, warm and cold, per profile, then the 1-CPU memory cliff; `e21sum.py` summarises it, including serve-vs-CLI result parity                                                                                                                                                                                                                                                                                                              |
| `serve-coldab.sh`                                           | interleaved A/B of two builds as `scout serve`, cold then warm, per profile; arms as in `ab.sh`; `coldabsum.py` summarises it and fails if its runs had different read-ahead (its parity line compares the two arms with each other; parity with rg is `scout-bench`'s)                                                                                                                                                                                   |
| `run-e24.sh`                                                | E24: builds the v5 bench index and checks it (`v3v5same.py`, `filtercheck.py`), an indexing time and memory A/B against v3, then the CLI warm, serve cold/warm, memory-cliff, and (at a cloud disk's 128 KiB read-ahead) serve cold and CLI cold A/Bs against v3, and the CLI's map-vs-heap filter A/B. Arms that search different indexes are first warmed from the script's own cgroup (`warmpass.py`), so no profile scope is charged for either index |
| `run-e25.sh <base> <e25>`                                   | E25: indexing A/B of two builds at 1 CPU / 2 GiB, 4 CPUs / 4 GiB and 12 CPUs, 5 alternating builds each; CPU and wall time, peak memory, and a hash of every shard so both arms must write the same ones                                                                                                                                                                                                                                                  |
| `scout_only.py <scout> <index> <tsv> <runs>`                | scout alone, no rg: 1 warm-up and `runs` timed runs per query, each a fresh process; median, p90, max, output lines and `--stats` counts. For corpora where rg takes minutes a query (E25 ran it on Windows)                                                                                                                                                                                                                                              |
| `ab_scout.py <index> <tsv> <rounds> <runs> <A=exe> <B=exe>` | interleaved wall-time A/B of two scout builds, scout alone, output read through a pipe; alternating arm order per round; per query the median per arm, the ratio and rounds won (E26)                                                                                                                                                                                                                                                                     |
| `pipe_ceiling.py run`                                       | how fast a pipe carries a saved output into a Python `capture_output` reader, by write size: the floor for output-heavy queries (E26). Times every `*.out` in the directory it is given (outputs saved by redirecting `scout search`)                                                                                                                                                                                                                     |
| `warmpass.py`                                               | runs every bench query once per arm (`bin@index`), untimed, so the pages they need are cached and charged to the caller's cgroup                                                                                                                                                                                                                                                                                                                          |
| `v3v5same.py`                                               | E24: checks a v5 index holds byte-identical shards to the v3 one but for the trigram filters, and that every shard's filter passes all of its own keys                                                                                                                                                                                                                                                                                                    |
| `filtercheck.py`                                            | E24: probes a v5 index's trigram filters per literal bench query with an independent copy of the hash, and checks `scout search --stats`'s `shards_filtered` against it                                                                                                                                                                                                                                                                                   |
| `shardfmt.py`                                               | the v3 and v5 native shard layouts, and an independent copy of the filter hash and trigram packing, for the E24 scripts                                                                                                                                                                                                                                                                                                                                   |
| `bloomoracle.py`, `bloomsim.py`                             | E24's design runs on the v3 index: shards an exact or a Bloom filter would let through per literal query, by expected and by simulated false-positive rate                                                                                                                                                                                                                                                                                                |
| `ebs-setup.sh [delay_ms] [size]`                            | root: the EBS-like volume `/mnt/ebs` (see below). Idempotent; rerun to change the read delay                                                                                                                                                                                                                                                                                                                                                              |
| `ebs-run.sh <cpus> <mem> <MB/s\|max> <iops\|max> <cmd...>`  | root: runs a command as uid 1000 in a fresh cgroup with `cpu.max`, `memory.max` (no swap) and `io.max` on the volume; prints peak memory, OOM kills and io.stat                                                                                                                                                                                                                                                                                           |
| `run-ebs.sh`                                                | root: `qbench.py` CLI warm then cold inside `ebs-run.sh` at gp3's baseline (125 MB/s, 3000 IOPS); `ebssum.py` summarises it (p50, p90, MiB read, major faults, queries under 1 s)                                                                                                                                                                                                                                                                         |
| `ab-ebs.sh`                                                 | root: interleaved A/B of two builds over `run-ebs.sh`, arms as in `ab.sh`; `ebsab.py` pools the reps, checks both arms printed the same bytes per query and prints ratios and geomeans (E28, E29)                                                                                                                                                                                                                                                         |
| `idx-e29.sh`                                                | root: E29's chromium + llvm-project index on the volume, built at 2 CPUs / 4 GiB with `--content auto`                                                                                                                                                                                                                                                                                                                                                    |
| `run-e29.sh`                                                | E29 on the bench corpus: v6 indexes raw and lz4 at 12 CPUs, then `ab.sh` of e26 (v5) against e29 raw, and e29 raw against lz4, at 1, 4 and 12 CPUs                                                                                                                                                                                                                                                                                                        |
| `coldmap.py <index-dir> <cmd...>`                           | evicts the index, runs the command, then maps the shard pages it left resident (`mincore`) to their format sections: which sections a cold query reads, in MiB (E30)                                                                                                                                                                                                                                                                                      |
| `run-e30.sh`, `run-e30-1g.sh`                               | root: `ab-ebs.sh` of two read-advice builds on one index at 2 CPUs / 4 and 2 GiB; at 1 GiB, no `MADV_RANDOM` against it on postings only and on dictionary + postings; every arm keeps the per-list `MADV_WILLNEED` (E30)                                                                                                                                                                                                                                 |
| `parity-e30.sh <bin-dir> <out-dir>`                         | the rg parity gate for a build on the bench corpus: v9 indexes raw and lz4 at 12 CPUs, `scout-bench` on each at 1 CPU / 2 GiB and 12 CPUs (E30)                                                                                                                                                                                                                                                                                                           |
| `phases.py <scout> <index> <queries> <runs> <out.json>`     | a warm CLI query's wall time against its `--stats` phases, per query; wall minus `total_ms` is process start plus shard release (E36)                                                                                                                                                                                                                                                                                                                     |
| `sections.py <index-dir>`                                   | MiB per v9 shard section summed over an index: what holding sections on the heap would cost (E36)                                                                                                                                                                                                                                                                                                                                                         |
| `serve-ebs.sh`, `serve-ab.sh`, `serveab.py`                 | root: `scout serve` in an `ebs-run.sh` cgroup, started on an evicted index (time to ready recorded), `qbench.py` warm then cold from CPUs 8-11; interleaved A/B of two builds (each on its own index with `name=bin@index`) pooled by `serveab.py`, which fails on any output difference. The server's sync adds a tiny repo, so give it an index copy (E36)                                                                                              |
| `run-e38.sh`, `probe-e38.sh`                                | root: E38, builds chromium + llvm-project `--content lz4` with each binary at 2 CPUs / 4 GiB, the parity gate on the new one, then `serve-ab.sh` with an index per arm; the probes behind it (`codec-probe`, `perf` profiles, `-w`/`-i`/`-S` against `-F`)                                                                                                                                                                                                |
| `run-e31.sh`                                                | root: `ab-ebs.sh` of the tree built with the count hint off (`COUNTED_PER_AND` above any AND) against on, same index, at 2 CPUs / 4 and 1 GiB (E31, rejected and removed)                                                                                                                                                                                                                                                                                 |
| `build-e32.sh`, `run-e32.sh`                                | E31 + E32 alone: a copy of the E33 baseline snapshot built without E30's `MADV_RANDOM` (its per-list `MADV_WILLNEED` stays), then `ab-ebs.sh` of e29 on its v6 index against it on a v7 index, at 4 and 2 GiB                                                                                                                                                                                                                                             |

## Broad-query comparisons

`run-subsecond-ab.sh <before-exe> <before-index> <after-exe> <after-index> <new-out>`
runs five alternating rounds of two timed samples per query/arm. Run it as root on the
EBS-like volume. Its Python controller gives each arm a fresh 2 CPU / 4 GiB cgroup,
evicts only that index, verifies residency, and warms it before timing. This prevents
different index formats from competing for a single server's memory. Complete output
is hashed in a separate warm-up pass and must match across arms. Timed stdout goes to
`/dev/null`; every sample records CPU time and disk-read bytes.

After it completes, `run-subsecond-parity.sh <after-bin-dir> <after-index> <completed-ab-dir> <rg>`
runs six broad queries through the independent rg oracle. The correctness run retains
2 CPUs / 4 GiB but lifts the disk throttle for rg's source-file reads.

`index-subsecond.sh <scout-exe> <new-index-dir>` builds chromium + llvm-project under
the same CPU, memory and disk limits. Existing directories are refused.
`small-block-cost.py <v9-shards-dir>` reports the cache cost of storing small LZ4
blocks raw, reading metadata without decoding content.

For cold index data, run `cold-subsecond.py <accepted-warm-summary.json> <new-out> <rounds>`
inside `ebs-run.sh 2 4G 125 3000`. It checks the accepted binary/catalog fingerprints,
evicts the index before every sample, verifies cache residency and disk read-ahead,
and records disk bytes, major faults, CPU and wall time. The executable stays warm;
timed stdout goes to `/dev/null`. Separate cold output-hash passes must match the warm
results. This follow-up covers four broad line queries and two selective queries.

## Expected layout

Scripts find each other through their own directory; data lives under `~/small`:

- `~/small/queries.tsv`: a copy of [`bench/queries.tsv`](../queries.tsv).
- `~/small/corpus/`: the bench repos mirrored onto the Linux disk (rg's parity runs read it).
- `~/small/bin/rg`: the rg binary parity is checked against.
- `~/small/<bin-dir>/{scout,scout-bench}`: one directory per build under test.
- `~/small/idx-e20-12cpu-infinity/`: the v3 index the A/Bs up to E23 search (built with
  `prof.sh 12 infinity scout --index-dir ... index`). `run-e21.sh` copies it to
  `~/small/idx-e21-serve/` and creates `~/small/tiny.git` and `~/small/serve-e21.toml`,
  which `serve-coldab.sh` reuses, because `serve` adds its mirrored repo to the index.
  `run-e24.sh` builds the v5 index `~/small/idx-e24-12cpu-infinity/` from the same repos
  and profile, and its primed serve copy `~/small/idx-e24-serve/`.
- Results go to `~/small/ab/<label>/`, `~/small/results/e21/`,
  `~/small/results/e24-index/`, `~/small/results/e24-indexab/` and `~/small/results/e25-indexab/` (whose builds use the
  scratch indexes `~/small/idx-e24-indexab/` and `~/small/idx-e25-indexab/`); the ones the experiment log cites are copied into
  [`bench/results/`](../results). Every script refuses to write into an output directory
  that already exists, because its summary reads every run in it: move an old one away to
  rerun.

Cold numbers depend on the read-ahead of the disk holding the index: a cold mmap fault
reads a window that size around the faulting page. The dev box's WSL2 virtual disks have
8 MiB (`/sys/block/sdX/queue/read_ahead_kb` = 8192), where a cloud VM's disk typically has
128 KiB, so a cold fault here reads up to 64 times as much (E24). Setting it needs root and
lasts until WSL restarts: `sudo sh -c 'echo 128 > /sys/block/sdc/queue/read_ahead_kb'`.

The EBS scripts (E28 on) need root (`wsl.exe -u root`) and a running distro (WSL stops an
idle one and the volume with it: keep a `sleep` running). `ebs-setup.sh` builds a sparse
file → direct-I/O loop device → `dm-delay` → ext4 at `/mnt/ebs`, with a 128 KiB read-ahead;
`ebs-run.sh` adds gp3's throughput and IOPS caps per run, so the page cache of the index is
charged to the same cgroup as the search, as on an EC2 instance. `dm-delay` waits in 4 ms
ticks (a 1 ms delay measures 4 ms a read) and the bare device answers in ~0.3 ms where gp3
takes ~0.5–1 ms: measure at 0 ms and confirm at 4 ms. The test data is chromium and
llvm-project under `/mnt/ebs/big/`.

The serve benchmarks run the HTTP client on CPUs 8-11 (`taskset`), outside every profile's
CPU set, so they need a machine with 12 or more CPUs.

### E35 bounded search

`bounded-http.py <scout> <250-repo-index> <new-out> <existing-sync.toml> <rounds>`
runs the real HTTP server under `ebs-run.sh 2 4G 125 3000`, with two HTTP workers.
Run the controller as root. It reclaims the server cgroup's file cache, evicts index
files, verifies residency before every cold request, and records response hashes,
completion/count metadata, read bytes and memory peak. Heap filters stay live.
Unlike the earlier serve scripts, this controller does not pin its lightweight
HTTP client to CPUs 8–11. It runs outside the limited server cgroup.

Build the fixture with `cargo run --release -p scout-index-native --example
repartition -- <source-index> <new-index> 250 8`. It repartitions/cycles existing
plain text into 250 independent native LZ4 shards; it does not generate filler or
pretend to be an NX corpus. The real server also needs a healthy sync config with
one tiny local Git repo named `zz-control`; reuse the same file URL on reruns,
because that repo is appended to the fixture catalog. The controller expects 251
served repos and refuses an existing output directory.

`bounded-oracle.py <scout> <index> <HTTP-summary> <new-output.json>` runs inside the
same resource harness after the HTTP benchmark. It measures an evicted exhaustive
`-c if`, checks bounded count lower bounds against it, and checks HTTP line hashes
against exhaustive output prefixes. An empty incomplete batch is recorded without
claiming no matches. `bounded-parity.py <scout> <E34-index> <E34-summary>
<new-output.json>` verifies all 19 frozen exhaustive hashes/exit codes.

`bounded-evidence.py <E34-source> <E35-built-source> <workspace> <HTTP-summary>
<index> <new-output-dir>` checks source/binary identity and writes the incremental
source patch and hashes. [Report and accepted results](../../docs/bounded-mcp-search.md).
