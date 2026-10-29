# Experiment log

Corpus: 137 repos under `C:\Users\User\source` (54,258 files, 853 MiB indexed content).
Machine: Ryzen 5 5500 (12 threads), 32 GB RAM, NVMe, Windows 11.
Bench: `scout-bench` with [`bench/queries.tsv`](../bench/queries.tsv). Every query runs as a fresh
process: 1 warm-up, then median of 5 (scout) and median of 2 (rg). From E7 on: 100 scout runs
per query, reporting median, p90 and p99, and 1 rg run. Four `selective_*` queries (0–2
matching files in the whole corpus) were added at E7, so geomeans from E7 on are not
comparable with earlier rows.
**Gate:** each query's `(path, line)` set must equal `rg`'s. A failing gate rejects the change.
Raw results are in [`bench/results/`](../bench/results). The interleaved A/Bs from E16 on
are in `bench/results/E<n>-ab/` (per-round scout-bench JSON, plus `summary.txt` where the
harness wrote one); E13's scaling runs and E17's serve load runs were not kept. The Linux
scripts and the data layout they expect are in [`bench/harness/`](../bench/harness).

Timings on Windows are noisy (±15% run to run), so only clear wins are kept.

| #   | Change                                                                                                                                                                                                                                                                                                                       | Strict geomean                                        | Off geomean | Verdict                                                                                                                                                                                                                                                                                                                                                                                                                    |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| E0  | Baseline: per-repo shards, trigram AND/OR, strict freshness via full ignore walk                                                                                                                                                                                                                                             | 3.7x                                                  | —           | baseline                                                                                                                                                                                                                                                                                                                                                                                                                   |
| E1  | Trigram-index the bytes before the first NUL of binary files (they used to be always-candidates)                                                                                                                                                                                                                             | 4.0x                                                  | —           | **kept** (candidates for `literal_rare` 4096 → 209)                                                                                                                                                                                                                                                                                                                                                                        |
| E2  | Directory snapshot for strict freshness (re-enumerate known dirs, skip unchanged empty dirs by mtime, evaluate ignore rules only for new names)                                                                                                                                                                              | 6.7x                                                  | 11.6x       | **kept** (strict: `literal_rare` 386 → ~220 ms)                                                                                                                                                                                                                                                                                                                                                                            |
| E3  | Read snapshots only in strict mode; no `is_dir` stat per repo on the search path                                                                                                                                                                                                                                             | 5.8x                                                  | 12.4x       | **kept** (neutral within noise; removes work). Found and fixed the reader-buffer bug below.                                                                                                                                                                                                                                                                                                                                |
| E4  | Posting units: large docs indexed as line-aligned 64 KiB blocks, so a doc is a candidate only if one block holds all trigrams                                                                                                                                                                                                | 6.4x                                                  | 13.9x       | **kept** (`literal_rare` off 132 → 71 ms, candidates 209 → 109, working set 302 → 120 MiB; index +2.4%)                                                                                                                                                                                                                                                                                                                    |
| E5  | Open repos (shards + snapshot) in parallel                                                                                                                                                                                                                                                                                   | 7.3x                                                  | 16.5x       | **kept** (open 23 → 5.5 ms off, 51 → 11 ms strict; `literal_none` off 22 ms wall)                                                                                                                                                                                                                                                                                                                                          |
| —   | Frequency-ordered intersection / Roaring postings (from research)                                                                                                                                                                                                                                                            | —                                                     | —           | **not tried**: candidate evaluation over all 139 shards measured at 0.4–3.5 ms single-threaded; verification dominates                                                                                                                                                                                                                                                                                                     |
| E6  | Unmap shards from several threads before exit                                                                                                                                                                                                                                                                                | 8.3x                                                  | 21.3x       | **kept** (teardown 21 → 7 ms for `literal_rare`; `-F function` 184 → 138 ms wall)                                                                                                                                                                                                                                                                                                                                          |
| E7  | Strict freshness from the USN change journal: re-list only directories with journal records since the snapshot's cursor; the search persists refreshed snapshots                                                                                                                                                             | 21.4x (scan mode: 9.2x)                               | 30.4x       | **kept** (selective queries strict p50 ~155 → 37 ms, p99 180–256 → 47–50 ms; strict is now off + ~18 ms)                                                                                                                                                                                                                                                                                                                   |
| E8a | Snapshots encoded with bincode instead of JSON                                                                                                                                                                                                                                                                               | 21.1x                                                 | —           | **kept** (on equal storage, single-thread p50 99.5 → 84.9 ms, 12 threads 35.2 → 32.2 ms; listing thread time 100 → 80 ms; snapshots 5.3 → 2.2 MiB). The bench table did not move because of drift, see below                                                                                                                                                                                                               |
| E9  | One snapshot pack file instead of one file per repo; journal cursors persisted every 256 KiB of lag instead of 4 MiB                                                                                                                                                                                                         | 22.8x                                                 | —           | **kept** (paired A/B, `SCOUTNI2`: 1 thread p50 120 → 101 ms, p99 170 → 141; 12 threads p50 41.4 → 38.1, p99 52.4 → 49.0)                                                                                                                                                                                                                                                                                                   |
| E10 | Unchanged-repo fast path: a listing digest in the index and in a snapshot header decoded alone; a repo the journal proves untouched is searched like `off`                                                                                                                                                                   | 29.0x                                                 | 29.4x       | **kept** (paired A/B: 1 thread p50 97.6 → 64.9 ms; 12 threads p50 38.3 → 28.4, p99 44.8 → 33.3). Loading the pack on its own thread, overlapping the shard opens: 12 threads p50 28.4 → 26.5 ms, open 8.0 → 5.8 ms (the same as off)                                                                                                                                                                                       |
| E11 | Per-search limits (output cap, deadline; checked after every document, skipped when unbounded), the hosted server and generational shards. Not meant as a speed change                                                                                                                                                       | 24.4x (a non-interleaved run, `E11-plain-quiet.json`) | —           | **regression, fixed in E12** (interleaved A/B against E10: `literal_none` and the selective queries +4.6–6.9 ms p50 in both modes, 1.10–1.35x, slower in 47 of 50 rounds; parity all OK). The first, non-interleaved check called it noise, see notes                                                                                                                                                                      |
| E12 | Physical memory from one Win32 call (`win.rs`) and `/proc/meminfo` instead of the sysinfo crate, which linked shell32, oleaut32, pdh, powrprof and psapi into every process                                                                                                                                                  | 26.9x                                                 | 27.7x       | **kept** (interleaved A/B against E11: 4.8–6.5 ms faster p50 on every small query, 50 of 50 rounds; `scout --version` 15.5 → 10.2 ms in a direct run, E10 9.3; parity all OK, all 20 queries, both modes)                                                                                                                                                                                                                  |
| E13 | Indexing budget from usable memory: Linux caps `MemTotal` by every cgroup memory limit (v2 `memory.max`, v1 `memory.limit_in_bytes`) up to the root (containers, systemd units). A correctness fix, not a speed change                                                                                                       | —                                                     | —           | **kept** (budget in a 2 GiB cgroup 4096 → 512 MiB, in 4 GiB 4096 → 1024. Smaller shard caps: 137 → 139–143 shards, search p50 geomean 1.02–1.03x slower, indexing peak RSS 139 → 130, 234 → 183, 341 → 273 MiB at 1/2/4 CPUs)                                                                                                                                                                                              |
| E14 | Parity gate: a diff file holding a NUL is re-checked against rg searching it alone. A gate fix, not a speed change                                                                                                                                                                                                           | —                                                     | —           | **kept** (the gate failed on Linux for `short_unindexable`, 46 lines of `sherlock-nul.txt` that rg printed alone and not among 137 roots; green on Windows presumably by scheduling luck)                                                                                                                                                                                                                                  |
| E15 | Shard content as per-document lz4 blocks (raw when lz4 does not shrink it), decompressed per candidate                                                                                                                                                                                                                       | —                                                     | —           | **rejected** (1 CPU / 2 GiB: index 1.1 GB → 410 MB and indexing 16.2 → 13.3 s, but search p50 geomean 2.03x slower; `literal_rare` 19 → 61 ms, `rare_regex` 27 → 118, selective queries on small files unchanged)                                                                                                                                                                                                          |
| E16 | Verification split into ~4 MiB chunks of candidates across and within repos (was: one repo per task), planned in parallel first                                                                                                                                                                                              | —                                                     | —           | **kept** (interleaved A/B, same index, 5 rounds: p50 geomean 0.993 at 1 CPU, 0.978 at 4, 0.985 at 12; CPU-bound scans 5–14% faster at 4–12 CPUs in 5/5 rounds; `case_insensitive` +5–9% and 12-CPU selective queries +1–9% slower; parity all OK, and strict parity on Windows)                                                                                                                                            |
| E17 | `serve`'s default HTTP workers `clamp(cores / 2, 1, 4)` (was `clamp(cores / 4, 1, 8)`): 4–5 cores 1 → 2, 6–7 cores 1 → 3, 8–11 cores 2 → 4, 12–15 cores 3 → 4, 16–31 cores 4–7 → 4, 32+ cores 8 → 4                                                                                                                          | —                                                     | —           | **kept** (4 CPUs, mixed closed-loop load: 76.6 → 83.9 req/s at 4 clients, 69.9 → 85.5 at 16; 16 clients p99 selective 473 → 334 ms, broad 493 → 382, ranked 508 → 365. 8 CPUs 2 → 4: 101.9 → 103.5 req/s, p99 within ±8%. 1–3 CPUs unchanged; 6–7 and 12+ cores not measured)                                                                                                                                              |
| E18 | Bound E16's reorder buffer (it buffered every chunk ahead of a slow one: all of a CLI search's output). A: at most 4 chunks per worker ahead of the printer. B: at most 64 MiB of unprinted output                                                                                                                           | —                                                     | —           | **A rejected** (geomean 1.041 at 4 CPUs, 1.042 at 12; `date_regex` 1.215 and 1.254, 0/5 rounds). **B kept** (geomean 0.983 at 4 CPUs, 0.996 at 12; no query slower in all 5 rounds, worst `short_unindexable` 1.061 at 12 CPUs (faster in 1/5); per-query peak RSS at 4 CPUs below E13's on every query)                                                                                                                   |
| E19 | Fenced dictionary (format v3): trigram keys in their own u32 array, values (count, offset) beside it, and a fence of every 128th key; a lookup binary-searches the fence, then one 512-byte key block (was: binary search over 16-byte entries, ~16 probes over ~10 pages per lookup)                                        | —                                                     | —           | **kept** (interleaved A/B, 5 rounds of 20 runs: geomean 0.707 at 1 CPU, 0.777 at 4, 0.820 at 12; selective queries 0.56–0.63 at 1 CPU; only `case_insensitive` at 12 CPUs slower, 1.055 (2/5); index +0.02%; parity all OK)                                                                                                                                                                                                |
| E20 | Positional masks (format v4): each posting carries a byte with bit p mod 8 set for every offset p of the trigram in the unit; a literal's trigrams must agree on a start (masks rotated by their index, ANDed non-zero)                                                                                                      | —                                                     | —           | **rejected** (interleaved A/B vs E19, 5 rounds of 20 runs: geomean 0.998 at 1 CPU, 0.981 at 4, 0.962 at 12; at 1 CPU `selective_*` 1.087–1.127, `ident_regex` 1.168, `case_insensitive` 1.158, all 0/5; index +6.5%, indexing at 1 CPU 16.6 → 20.8 s; parity all OK)                                                                                                                                                       |
| E21 | Measurement, no code change: honest cold vs warm latency, CLI vs `serve`, and the page-cache cliff. Cold = the index evicted from the page cache before every request and checked evicted with `mincore`, so a cold number can never be a cached one                                                                         | —                                                     | —           | **measured** (geomean p50 of 20 queries, ms. Warm CLI / cold CLI / warm serve / cold serve: 1 CPU 71.9 / 2232 / 36.9 / 2047; 4 CPUs 49.2 / 1720 / 22.1 / 1527; 12 CPUs 51.9 / 1725 / 23.6 / 1522. Serve at 1 CPU, warm: 36–40 ms down to a 1280 MiB limit, 215 at 1 GiB, 1040 at 768 MiB, 1594 at 512 MiB; parity all OK)                                                                                                  |
| E22 | Count-first AND planning: every trigram's unit count is read from the dictionary first (one count of 0 empties the AND before anything is decoded), then lists are decoded rarest first. A: only the 2 rarest. B: a decode budget, stop once decoding the next list costs more than verifying the candidates it could remove | —                                                     | —           | **A rejected** (geomean 1.054 at 1 CPU, 1.042 at 4, 1.038 at 12; `literal_rare` 1.610 / 1.464 / 1.374, 0/5). **B kept** (geomean 0.954 at 1 CPU, 0.969 at 4, 0.977 at 12; `selective_long` 0.775, `selective_vendored` 0.789, `selective_one` 0.866 at 1 CPU, 5/5 (`selective_common_trigrams` 0.957); `case_insensitive` 1.138 at 1 CPU (0/5) with identical candidates and in-process timings, see notes; parity all OK) |
| E23 | Each shard's marker doc lists (`ALWAYS_SEARCH`, `READER_MODE`) decoded once at open and held on the heap, with the native fence copied out of the map (was: two dictionary lookups, postings decodes and unit maps per shard on every query)                                                                                 | —                                                     | —           | **kept, small** (served warm: `selective_one` / `selective_long` 1.5 → 1.3 ms at 1 CPU and 1.8 → 1.6 at 4, about 0.2 ms a query; pooled geomean 0.934 / 0.976 / 1.003 at 1 / 4 / 12 CPUs, partly noise, see notes. CLI 0.997 / 1.012 / 1.007. Served cold: median major faults per request 3–4% lower, latency 1.007 / 1.024 / 0.994; parity all OK)                                                                       |
| E25 | Read-ahead indexing: files are read, classified and trigram-split on `walk_threads` threads ahead of the shard writer, which takes them in path order (bounded window, an eighth of a shard), so shards are byte-identical to a serial build's. One CPU reads inline. Found indexing chromium + llvm-project                 | —                                                     | —           | **kept** (Windows, chromium + llvm-project, 697,675 files, 5.1 GiB: 4088 s cold, base on a warm cache over 1526 s and unfinished, e25 944 / 998 s; llvm-project alone 1420 → 302 s warm. Linux, 137 repos, CPU s / wall s median: 1 CPU (inline) 7.12 → 7.25 / 10.84 → 11.10, 4 CPUs 8.24 → 8.73 / 5.18 → 4.77, 12 CPUs 9.85 → 11.79 / 4.83 → 3.78; peak RSS equal or lower; shards identical in every build)              |
| E26 | Shards, not repos, released in parallel after a search: one queue of every opened shard that `threads` threads drain (was one repo per thread, E6)                                                                                                                                                                           | —                                                     | —           | **kept** (chromium + llvm-project, scout alone, 5/5 rounds: `nomatch_unicode_regex` 933 → 654 ms, `date_regex` 1,167 → 878, `literal_long` 477 → 364; output byte-identical; 21 of 23 queries now under 1 s)                                                                                                                                                                                                               |
| E27 | CLI stdout buffer 64 KiB → 1 MiB                                                                                                                                                                                                                                                                                             | —                                                     | —           | **rejected** (no change, 0.992–1.026: chunks are written whole and bypass the buffer)                                                                                                                                                                                                                                                                                                                                      |
| E28 | EBS-like harness (2 CPUs / 4 GiB, gp3 caps); verifiers queue `MADV_WILLNEED` over their candidates' content in ~4 MiB blocks ahead of verifying                                                                                                                                                                              | —                                                     | —           | **kept** (chromium + llvm-project, 5.8 GiB index on 4 GiB: cold geomean 0.696 with ~40% fewer bytes read; steady state 0.940, noisy)                                                                                                                                                                                                                                                                                       |
| E29 | Format v6: content in line-aligned blocks, each lz4 where smaller; `--content auto` picks lz4 when content > half of usable memory                                                                                                                                                                                           | —                                                     | —           | **kept** (EBS box: index 5.8 → 2.3 GiB, steady state 0.567 with no disk reads, `date_regex` 20.9 → 2.3 s, cold 0.746; bench corpus raw v6 + E28 vs e26: 0.813 / 0.851 / 0.899 at 1 / 4 / 12 CPUs; lz4 on an index that fits memory 1.5–1.9x slower, so `auto` keeps raw there; parity green)                                                                                                                               |
| E30 | Posting lists mapped `MADV_RANDOM` and each asks for its exact range (`MADV_WILLNEED`). A: the dictionary too. B: postings only                                                                                                                                                                                              | —                                                     | —           | **A rejected** (1 GiB warm 1.094, 2 GiB on v6 1.455: dictionary misses become 4 KiB reads; cold 0.80–0.87). **B kept** (v9, 1 GiB: warm 0.893, cold 0.914; 4 GiB warm 1.002, cold 0.917; outputs identical)                                                                                                                                                                                                                |
| E31 | A repo's shards share trigram counts; a long AND counts only the 6 the shard before ranked rarest                                                                                                                                                                                                                            | —                                                     | —           | **rejected, removed** (cold 0.993 at 4 GiB, 1.013 at 1 GiB, same bytes read; `literal_none` warm 10 → 20 ms from 4 more candidates)                                                                                                                                                                                                                                                                                        |
| E32 | Format v7: blocks carry their line offset; a large candidate is read and searched only at its candidate units (vendored grep-searcher patched to start line numbers at N)                                                                                                                                                    | —                                                     | —           | **kept** (with E31 and E30's WILLNEED, vs e29: 4 GiB warm 0.915, cold 0.956, 8,714 → 7,866 MiB read cold, broad queries slower cold; 2 GiB warm 0.954, cold 0.980; outputs identical)                                                                                                                                                                                                                                      |
| E36 | `serve` keeps every shard's dictionary (keys, values) on the heap with its filter                                                                                                                                                                                                                                            | —                                                     | —           | **kept** (`serve` at 2 CPUs / 4 GiB: cold 0.778, `selective_long` 522 → 232 ms, no-match 242 → 79; warm 0.994; startup on an evicted index +0.6 s; ~105 MiB heap, 4% of the index)                                                                                                                                                                                                                                         |
| E37 | `serve` keeps every table (docs, blocks, paths, units, dictionary, filter) on the heap, within an eighth of memory per generation                                                                                                                                                                                            | —                                                     | —           | **B kept** (`serve` at 2 CPUs / 4 GiB vs E36: cold 0.784, faster on 18/18, `literal_none` 83 → 40 ms, `selective_long` 210 → 133; warm 0.984; startup +0.85 s; ~211 MiB heap, 8% of the index). A (content `MADV_RANDOM`) rejected: same bytes, 2x the IOs                                                                                                                                                                 |
| E38 | Content blocks encoded with lz4 HC level 6 instead of lz4_flex's fast encoder (same block format, same reader). Probe: why not rewrite `-i`/`-w`/`-S` queries                                                                                                                                                                | —                                                     | —           | **kept** (`serve` at 2 CPUs / 4 GiB: warm 0.970, cold 0.964, index 2,864 → 2,580 MiB, build 304 → 295 s; outputs identical). Query rewrites not tried: `-w`, `-i`, `-S` cost 7–23 ms over `-F`, and the matcher beats Aho-Corasick                                                                                                                                                                                         |
| E39 | `--rank` verifies the plain search's chunks on every thread (was one thread per repo), with the shape filter, and scores each file in one pass                                                                                                                                                                               | —                                                     | —           | **kept** (`--rank --top 20`, warm, 2 CPUs: geomean 0.609, `date_regex` 1,937 → 514 ms, `literal_long` 1,542 → 732; output identical on all 18 queries and the 79 golden queries' features)                                                                                                                                                                                                                                 |
| E40 | Simulation, no scout change: `--rank --top N` that stops early, either after a cap of matches in a path-only order, or exactly, once the top N beat every unvisited file's path-only score bound                                                                                                                             | —                                                     | —           | **rejected** (a cap of 20–200 costs 0.24–0.34 nDCG@10 on the broad golden queries; the exact stop at N = 10 still decodes 65–96% of the matched bytes on 12 of the 16 chromium + llvm queries with matches)                                                                                                                                                                                                                |
| E41 | `--rank` per-file work: features moved instead of cloned, one hit buffer per chunk, line count stopped once `classify` is settled, head lowercased on the stack; variant A also classified paths without allocating                                                                                                          | —                                                     | —           | **rejected** (output identical; A: geomean 0.988 / 0.993 at 9 / 21 runs but +6% instructions on `literal_long`; B: −1–2% instructions, cycles within ±2% noise on all 5 broad queries)                                                                                                                                                                                                                                     |
| E42 | `prefetch` joins candidates' content ranges across gaps of up to 32 KiB (was 4 KiB), so a cold query issues fewer, larger reads                                                                                                                                                                                              | —                                                     | —           | **kept** (serve on the EBS-like box, 2 reps: cold geomean **0.731**, `alternation` 10.0 → 5.8 s, `word` 6.6 → 3.5 s; warm 0.978; `literal_long` cold +4%)                                                                                                                                                                                                                                                                  |
| E43 | Writer compresses every content block lz4 shrinks, small ones too (was: blocks under 4 KiB stored raw)                                                                                                                                                                                                                       | —                                                     | —           | **rejected** (fails the rule set before the run: cold geomean 0.959 against ≤ 0.95; index 2,580 → 2,305 MiB, `literal_long` cold 0.842 but warm 1.107, over 10% slower in both reps; warm geomean 1.018)                                                                                                                                                                                                                   |
| E44 | A 64 KiB lz4 dictionary per shard, trained on its first 8 MiB, for every doc of one block at any size (format v10)                                                                                                                                                                                                           | —                                                     | —           | **rejected** (fails all three clauses of the rule set before the run: cold geomean 0.971, warm 1.066, `literal_long` and `files_mode` over 10% slower warm in both reps, `anchored` marginally; index 2,580 → 2,165 MiB)                                                                                                                                                                                                   |
| E45 | `serve` reads the served index into the page cache in the background after the first cycle, 4 MiB at a time, pausing while a search runs                                                                                                                                                                                     | —                                                     | —           | **rejected** (restart A/B: queries sent at ready 1.141 against ≤ 1.00 in both reps, `llvm_regex` 1.86; 45 s after ready 0.264; steady warm 1.024 against ≤ 1.02)                                                                                                                                                                                                                                                           |
| E46 | E45 reading 256 KiB at a time                                                                                                                                                                                                                                                                                                | —                                                     | —           | **rejected** (queries at ready 0.986 pooled but 1.105 in one rep, `literal_none` 3.4x in both; 45 s after ready 0.286)                                                                                                                                                                                                                                                                                                     |
| E47 | E46, reading only after no search ran for 100 ms; on by default (`--warm-mb`)                                                                                                                                                                                                                                                | —                                                     | —           | **superseded by E49** (queries at ready 0.958; 45 s after ready 0.261; its confirmation E47b failed steady warm 1.037 in a new spaced scenario, which a rerun put at 1.006)                                                                                                                                                                                                                                                |
| E48 | E47 waiting for idle on a condvar instead of polling every 5 ms; shards read forward, opened lazily; a new generation skips shards already read                                                                                                                                                                              | —                                                     | —           | **rejected** (at ready 1.020, one rep 1.066; steady 1.029 and 1.027, the latter with the walk finished: the fixed clauses sat inside the noise. Forward reads doubled the walk's reads, 20,285 against 11,526)                                                                                                                                                                                                             |
| E49 | E48 reading backward again; judged against an A/A run's noise, 4 reps                                                                                                                                                                                                                                                        | —                                                     | —           | **kept** (every clause inside the A/A's noise: at ready 1.013 against ≤ 1.067, steady 0.989–1.001; 45 s after ready 0.253, the pass 12.0 → 2.3 s; 300 ms apart 0.738)                                                                                                                                                                                                                                                      |
| E50 | `prefetch` joins the shortest gaps first, only gaps under a read's worth of bytes (41.7 KB at gp3's 125 MB/s ÷ 3000), and stops once a block's reads cost no more than its bytes (was: every gap up to 32 KiB)                                                                                                               | —                                                     | —           | **rejected** (cold 0.959, 0.952 and 0.968 per rep, `alternation` 0.877 and `literal_common` 0.860; but `llvm_regex` 1.44 and 2.03 and `smart_case` 1.21 and 1.20 slower in both reps, against no query over 10% slower in both; warm 0.999)                                                                                                                                                                                |
| E51 | `serve` sends every body with Content-Length instead of tiny_http's chunked transfer encoding (8 KiB frames for bodies of 32 KiB or more)                                                                                                                                                                                    | —                                                     | —           | **reverted** (warm 0.979, 0.977 and 0.982 per rep; cold 0.990; but an A/A of one binary put its warm reps at 0.954 and 1.022, so the gain is within noise)                                                                                                                                                                                                                                                                 |
| E52 | `prefetch` joins by E50's balance for a query whose content bytes cost 1.5x its reads (estimated for the whole query while planning), by E42's 32 KiB gaps otherwise                                                                                                                                                         | —                                                     | —           | **kept** (cold 0.949, 0.947 and 0.950 per rep; held out: `small_vector` 0.775, `const_auto_ref` 0.901; no query over 5% slower cold; warm 1.022, the estimate costs 4–7% of a broad warm search, E53's target)                                                                                                                                                                                                             |
| E53 | Planning and read-ahead look a candidate's size up with a new `Shard::size`, not `meta`, which checked the path's UTF-8 on every lookup                                                                                                                                                                                      | —                                                     | —           | **rejected** (warm search alone 0.965 and 0.957, `crbug_regex` and `files_mode` ~6% faster; but cold 1.068, `llvm_regex` 1.38 and `selective_long` 1.37 slower in both reps: path pages were first read during verification, behind its read-ahead)                                                                                                                                                                        |
| E54 | E53, plus `prefetch` queuing each block's paths after its content                                                                                                                                                                                                                                                            | —                                                     | —           | **rejected** (warm search alone 0.979 and 0.966; cold 1.085, worse than E53: `llvm_regex` 1.50, `selective_symbol` 1.38, `smart_case` 1.23, the broad queries 3–8% slower; cold-io: 4–21% more reads)                                                                                                                                                                                                                      |
| E55 | E53's `Shard::size` after planning only (`want`, `queue_block`); `chunk_repo` keeps `meta`, whose path read faults the paths section in while planning                                                                                                                                                                       | —                                                     | —           | **rejected** (warm search alone 0.986 and 0.979 against ≤ 0.98 in both passes; cold 0.978 with identical reads, no query slower)                                                                                                                                                                                                                                                                                           |

Bench memory columns (from E4): _peak WS_ includes touched mmap pages, which are shared and
evictable; _peak private_ is the commit the process really needs, and is the number that has
to fit a 2 GB machine. Private peaks are 5–60 MiB, except queries with very large output
(`short_unindexable`, ~500k lines: ~290 MiB), because each repo's output is buffered until
its turn in the ordered output.

## Notes

### E7: change-journal freshness

- Design: [design.md, _Strict freshness_](design.md#strict-freshness). Snapshot v3 stores directory
  file ids, the root's ancestor ids and a journal cursor. A search reads the journal once per
  volume, from the oldest cursor, and keeps "latest USN per parent dir" and "latest
  rename/delete USN per id". A directory is re-listed iff it has a child record at or after
  the snapshot's cursor.
- Correctness: `random_changes_always_match_full_walk` (4 seeds × 60 rounds of random
  creates, deletes, renames, kind changes and ignore edits, both modes) checks that every
  refreshed listing equals a full walk. Two end-to-end tests check that the CLI persists
  and reuses snapshots in both modes.
- Off-mode parity failed in the E7 run on `docs/design.md` lines only, because I edited that
  file (it is in the corpus) during the run. Strict mode reads changed files from disk, so
  its gate stayed green. Lesson: no corpus edits during off-mode runs.
- Known limits (as git's index): same size and mtime but different content; hard links
  modified through another path.
- What strict still costs over off, at 12 threads (`SCOUTNI2`): open +5 ms (137 snapshot
  reads), listing ≈ 100 ms of thread time = decode 40 + journal plan 43 (mostly waiting on
  the one shared journal read, ~2.5 ms wall) + building the file list 17. Refresh is ~0.

### E8a: binary snapshots, and the storage-location effect

- The first E8a bench looked like a wash, and a paired A/B looked like a regression (+10 ms
  single-threaded). The cause was where the index lived, not the encoding. The same binary
  on the same files opens them in 43.5 ms under `AppDataLocal…` and 20 ms under
  `AppDataLocalTemp…` (single thread, 276 files: 139 shards + 137 snapshots). A fresh copy
  under `AppDataLocal` is as slow as the bench dir, so it is the location, not the files.
  Both are on the same NTFS volume with the same attributes. This fits a filter driver
  (Defender) scanning each open in one location and not the other, about 85 µs per open.
  Exclusions are not readable without admin.
- With both indexes in the same directory, interleaved, 40 runs each: bincode single-thread
  p50 84.9 vs JSON 99.5 ms; at 12 threads 32.2 vs 35.2 ms.
- Consequence: **the number of files a search opens is a first-order cost** wherever
  on-access scanning applies. A strict search opens 276 files; see E9.

### Correctness fixes found by the parity gate (before E0)

- rg searches the bytes _before_ the first NUL of a binary file, then prints a warning. Binary
  files are therefore stored up to and including the first NUL and searched with reader
  semantics (`search_reader`), which reproduces the warning text exactly.
- rg sniffs BOMs: UTF-16 is transcoded and a UTF-8 BOM is stripped. These files are searched
  through the reader path. UTF-16 files are always candidates, because raw-byte trigrams say
  nothing about the decoded text.
- Files rg skips (e.g. images with an early NUL) are still recorded as docs. Otherwise strict
  freshness sees them as "new" and re-reads ~3.9k files from disk on every query.

### E2: where strict freshness time goes

- Raw Windows cost, single thread: `readdir` ≈ 72 µs per directory, `stat` ≈ 40 µs. The corpus
  has 10,852 indexed directories, so a full enumeration is about 780 ms of CPU.
- Parallelism helps until about 6 effective cores. Total throughput is about 54 dirs/ms no
  matter how threads are split across repos.
- Going below this needs an OS change journal (USN; its unprivileged read works, see E5) or a
  watcher daemon. See the research notes.

### E3: reader output depended on earlier docs (correctness bug, fixed)

- The off-mode parity gate failed on `sherlock-nul.txt` after a reindex. Single-threaded
  runs failed too, but only when certain repos were searched first.
- Cause: grep-searcher's line buffer grows on long lines and never shrinks. rg stops
  _without output_ when the first buffer fill already contains the NUL. A reused searcher
  whose buffer had grown read the whole late-NUL file in one fill and printed nothing.
  A fresh rg process (64 KiB buffer) prints the lines before the NUL.
- Fix: every reader-mode doc is searched with a clone of a searcher whose line buffer was
  never used (slice searches never touch it). Regression test:
  `reader_output_does_not_depend_on_previous_docs`.

### E4: why posting units

- Off-mode `literal_rare` spent ~45 ms of 130 ms wall time _after_ the search: unmapping
  ~300 MiB of touched shard pages at exit. Leaking the mappings did not help; the OS pays
  the same at process teardown. So the real problem was touching the pages at all.
- Its 206 candidates totalled 268 MiB: CSV dumps, `typescript.js`, 5 MB Lua tables. They
  contain every trigram of `uselayouteffect` somewhere, but never on one line.
- A simulation over the bench index picked the block size:

  | query                 | doc-level |  16 KiB |  64 KiB | 256 KiB |
  | --------------------- | --------: | ------: | ------: | ------: |
  | `useLayoutEffect`     |   268 MiB |  64 MiB |  86 MiB | 175 MiB |
  | `class \w+Controller` |   471 MiB | 220 MiB | 372 MiB | 438 MiB |
  | posting entries       |     1.00x |   2.06x |   1.50x |   1.20x |

  Queries whose candidates are mostly real matches barely change, as expected.

- Sound because the matcher is line-oriented: a match lies on one line, a unit never
  splits a line, so a matching line's trigrams are all in one unit. Whole docs are still
  verified, so output (including line numbers) is unchanged.

### E5 and the strict-mode floor

- Directory enumeration API microbenchmark (44,850 dirs, warm, single thread):
  `std::fs::read_dir` 88 µs/dir, `FindFirstFileExW` + large fetch 87 µs/dir,
  `CreateFileW` + `GetFileInformationByHandleEx(FileFullDirectoryInfo)` 79 µs/dir.
  The cost is in the kernel and filter drivers (Defender), not the API, so switching
  APIs is not worth it.
- `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` works without admin when the volume is opened with
  `FILE_TRAVERSE`: 328k records (25 MiB) read in 140 ms. But the default 32 MiB journal
  covers only ~35 minutes on this machine, so using it needs a checkpoint that every search
  advances. See the research notes.

### E6: process teardown

- After E4, `literal_rare` still took 51 ms wall against 19 ms in-process plus 8 ms spawn.
- Dropping the 139 shard maps took 21 ms (≈0.17 ms per MiB of touched pages). Leaving it
  to process exit costs the same, because the kernel unmaps serially. Dropping chunks of
  repos from 12 threads overlaps the work: 7 ms.

### E9: snapshot pack

- A strict search used to open 276 files (139 shards + 137 snapshots). The pack makes that 140. It also makes advancing every repo's journal cursor one write instead of 137, so the
  persist threshold dropped from 4 MiB to 256 KiB of journal. The journal read is on every
  strict search's critical path at ~5.6 ms per MiB, so at 4 MiB it could reach ~20 ms
  before a cursor advanced.
- Concurrent writers: last writer wins. A lost update is sound, because each snapshot is
  valid at its own cursor; the next search just replays more journal.
- The bench machine was loaded during this phase (a game and a music player using ~2.5
  cores; rg itself got 20% slower), so absolute bench tables from E9 on are noisy. Every
  keep/revert decision from E8a on is from interleaved A/B runs (40–100 pairs), which
  cancel that drift.

### E10: unchanged-repo fast path

- The index stores an order-independent 128-bit digest of the listing it indexed (xxh3 per
  entry, summed). It is stored only when no file failed to read; otherwise the index does
  not hold exactly that listing. Snapshots carry the digest of their own listing in a
  header that decodes alone: digest, root id, ancestor ids, cursor, watch list, and all
  directory ids.
- A repo is unchanged when four things hold: the header digest equals the index's; the
  watch list is unchanged; the journal has no record under any of the repo's directory ids
  and none moving its root or ancestors; and the cursor does not need advancing. If the
  cursor needs advancing, the slow path runs, so `list` can persist the new cursor.
- Unchanged repos skip the indexed-path map, the full snapshot decode and the file-list
  build. After this, strict mode costs about 4 ms over off for a selective query (12
  threads): about 3 ms of journal read and header decode, the rest noise.
- Tests: the randomized refresh test now asks `unchanged()` before every refresh, and
  whenever it says yes, a full walk must have the claimed digest (4 seeds × 60 rounds ×
  both modes). The end-to-end test checks the fast path fires when idle, stops after a
  same-size rewrite deep in the tree, stays off while the snapshot differs from the index,
  and fires again after a reindex.
- Record runs (100 runs per query, loaded machine): strict and off are within noise for
  every query. Selective queries: strict p50 26.8–29.0 ms, p99 33.6–44.2 ms; off p50
  22.0–24.8, p99 30.6–37.6. Before E7 (scan) the same queries were p50 ~155, p99 180–256.
- Where a selective strict query's ~27 ms goes now (12 threads): process spawn and exit ~9
  ms (the `--version` floor), in-process ~11 ms (open 5.8, search ~5), teardown and output
  the rest.

### E11: limits, server, generational shards (regression found)

- First check (wrong): a quiet bench run (`E11-plain-quiet.json`, the 24.4x in the table)
  compared with E10's record table from another day. It showed every heavy query equal or
  faster (`short_unindexable` 339 → 304 ms) and the selective strict queries 1–2 ms
  slower, which was put down to noise. An earlier selective "regression" of 7–16 ms had a
  different cause: the scout repo is in the corpus, and the session's edits made it dirty
  (`--stats`: `dirty_files=37`), so strict re-read those files on every search. Lesson,
  as with E7: reindex the scout repo before a strict run when it has been edited, and
  write bench output outside it.
- Interleaved A/B (the method every decision from E8a on uses; `bench/results/E12-ab.txt`):
  E10's binary on an index it built, against E11's, 5 rounds × 20 runs, alternating
  order, the scout repo freshly indexed in both. `literal_none` and the selective queries
  were +4.6–6.9 ms p50 in both strict and off (off `selective_one` 19.8 → 26.7 ms), slower
  in 47 of 50 rounds. Parity all OK in both modes.
- Cause: process start, not the search. One `--stats` run each showed the same in-process
  time (open ~6 ms, total ~8 ms). Interleaved `scout --version` (10 rounds × 30 spawns):
  E10 9.3 ms, E11 15.6 ms, slower in 10 of 10 rounds. Counting imported DLLs past the CRT
  (`api-ms-win-crt-*`), E10 has 6 and E11 has 14. `sysinfo`, added only to read physical
  memory for the indexing budget, brought in shell32, oleaut32, pdh, powrprof and psapi;
  ureq and tiny_http brought bcrypt, advapi32 and ws2_32. The loader maps all of them, and
  their dependencies, at every process start, including every search. Binary size is not
  the mechanism: E12 is 8.98 MB against E11's 9.03 MB, and starts 5.3 ms faster.

### E12: physical memory without sysinfo

- `scout_source_fs::physical_memory`: `GlobalMemoryStatusEx` in `win.rs`, next to the
  other FFI, and `MemTotal` from `/proc/meminfo` elsewhere (Linux, where the server runs,
  needs no FFI). Other Unixes, macOS included, get the open error and must pass
  `--memory-mb`. Before, a `sysconf` call covered every Unix, but it was `unsafe` outside
  `win.rs`, against CLAUDE.md.
- Imports past the CRT: 9. E10's 6 plus bcrypt, advapi32 and ws2_32, which ureq and
  tiny_http need for sync and the server.
- A/B against E11 rebuilt as this tree with only sysinfo restored (same import table as
  the original E11), same index, same protocol: 4.8–6.5 ms faster p50 on `literal_none`
  and every selective query, in strict and off, 50 of 50 rounds. Against E10: off p50
  0.98–1.04x, strict 1.01–1.04x.
- Process start: `scout --version` 10.3 ms, against E10's 9.3 (slower in 10 of 10
  rounds). That 1.0 ms is the part of E11 left. It matches the three network DLLs, but
  that attribution is not isolated: the code needs them, so there is no build without
  them. A separate server binary would remove it. On Linux the binary links no such
  libraries.
- Record runs (100 runs per query, scout repo freshly indexed, output outside it): strict
  26.9x, off 27.7x, parity all OK on all 20 queries in both modes. Selective queries
  (except `selective_common_trigrams`): strict p50 25.4–27.7 ms, p99 33.3–37.0; off p50
  21.8–24.3, p99 29.4–35.0.

### E13: budget from the cgroup limit

- Found by the small-server harness (WSL2 Ubuntu, `systemd-run --user --scope` with
  `MemoryMax`, `MemorySwapMax=0` and `CPUQuota`, plus `taskset`). Inside the scope
  `/proc/meminfo` shows the host's 29 GiB, so `index` planned a 4096 MiB budget in a
  2 GiB cgroup. A container on a small VM gets the same wrong number.
- `scout_source_fs::usable_memory` (was `physical_memory`) now takes the smallest limit
  on the path from the process's cgroup up to the root of the hierarchy holding the
  memory controller: v1's `memory/<path>/memory.limit_in_bytes` when a v1 line names
  `memory` (v1-only and hybrid hosts), else the unified `<path>/memory.max`. `max` is no
  limit; a missing file inside an existing cgroup directory is skipped (the root has
  none). A missing cgroup directory (a container sharing the host's cgroup namespace), a
  `/..` path (nested namespace), any other read error or an unparsable value is an error
  that names `--memory-mb`, never a silent `MemTotal`. Only the v2 path was exercised on a
  real host; v1 and hybrid are covered by tests on fake trees. Windows still reads
  physical memory (no Job Object limit).
- Check (`budgetcheck.sh`, one repo, before and after): 2 GiB scope 4096 → 512 MiB, 4 GiB
  scope 4096 → 1024 MiB. Indexing the 137-repo bench corpus at 1 CPU / 2 GiB had peaked
  at 138 MiB RSS anyway, so the wrong budget had not yet caused an OOM kill; with more
  workers and larger shards it would.
- Before/after on the small-server profiles (fresh index per profile, warm corpus, 100
  runs per query, freshness off; sequential runs, not interleaved, `compare.py`):

  | profile        | shards    | index wall    | index peak RSS | search p50 geomean after/before |
  | -------------- | --------- | ------------- | -------------- | ------------------------------- |
  | 1 CPU / 2 GiB  | 137 → 139 | 14.5 → 16.2 s | 139 → 130 MiB  | 1.019                           |
  | 2 CPUs / 4 GiB | 137 → 139 | 10.8 → 11.3 s | 234 → 183 MiB  | 1.022                           |
  | 4 CPUs / 4 GiB | 137 → 143 | 7.5 → 7.5 s   | 341 → 273 MiB  | 1.029                           |

  The cost is on the cheapest queries (selective +0.3–0.6 ms p50: more shards to open).
  Kept because a budget above the machine's limit is a bug. The numbers also show the
  model is pessimistic: `plan_indexing` assumes 5x the shard cap per worker, the measured
  peak is far below that, so a calibrated model would give back bigger shards.

### E14: rg disagrees with itself on late-NUL files

- The first small-server run (Linux, 137 repos) failed parity on `short_unindexable`:
  scout printed 45 match lines and the binary warning for
  `vendor/ripgrep/tests/data/sherlock-nul.txt` (90 KB, first NUL at byte 77041); rg
  printed nothing for it.
- rg reads a file in fills of a line buffer that each search thread reuses and that only
  grows. A fresh buffer reads the first 64 KiB, finds no NUL, prints those matches, then
  quits at the next fill with the warning. A buffer grown by an earlier long-lined file
  takes the whole file in one fill, sees the NUL and prints nothing. Shown with `-j1
  --sort path`: the file alone or first, 46 lines; after a 200 KiB single-line file, 0.
  `--mmap` gives a third answer (67).
- scout already searches every reader-mode doc with a fresh searcher (`search::run`), so
  its output is rg's with a fresh buffer, deterministically; the copy of rg searching the
  file alone in a directory matched it line for line. The gate compared it against
  whatever rg's walk order happened to produce: on Windows it was presumably a fresh buffer (inferred from the passing gate, not measured there).
- `scout-bench` now re-runs rg (`-j1`, the query's flags) on a same-named lone copy of each
  file in a diff that holds a NUL byte, takes that as the expected output for the file,
  prints `re-verified N NUL file(s)` on stderr and records `reverified_nul_files` in the
  JSON. Files without a NUL are never re-checked, so a text diff still fails.

### E15: lz4 content (rejected)

- Motivation: on a small box the index competes with everything else for the page cache;
  gzip shrinks the whole bench index 4.4x, its two largest shards 8x.
- Native format "v3" (never shipped; E19's fenced dictionary later took the number): each
  doc stored as an lz4 block (lz4_flex, safe decoder) or raw when
  that is not smaller; `content()` returned an owned decompressed copy.
- 1 CPU / 2 GiB, against E13 (same budget, 139 shards each): index 1.1 GB → 410 MB,
  indexing 16.2 → 13.3 s (fewer bytes written), cgroup peak while indexing 1127 → 773 MiB.
  Search p50 geomean 2.03x slower: most queries 2.3–4.4x, the broad ones 1.3–1.4x, and
  only the selective queries whose candidates are small files unchanged. The 2 and 4 CPU
  runs were stopped; the result was not close.
- Why: candidates are often multi-megabyte files (generated data in `PathOfBuilding`,
  `battlefield 2`), and each one is decompressed whole even when a single 64 KiB unit
  holds all the trigrams. The whole 1.1 GB index already fits a 2 GiB box's page cache,
  so the decompression bought nothing here. It would pay only when the index is larger
  than RAM.
- What could work instead: compress and search per unit (block offsets plus line counts
  per block, so only candidate units are decompressed and line numbers stay right). That
  also removes the cost of searching whole large documents uncompressed, but must keep
  rg's binary, context and multiline semantics; not tried yet.

### E16: chunked verification

- Found by the scale-up profiles (8 CPUs / 16 GiB, and the whole 12-CPU VM with no memory
  limit). Total search p50 over the 20 queries: 3073 ms at 1 CPU, 2077 at 2, 1879 at 4,
  1862 at 8, 1870 at 12, while rg kept improving. Queries that print a lot were flat past
  2 CPUs, and selective queries got slower with more cores (9 → 12 ms).
- The unit of parallel work was a repo. Planned as: chunks of candidates of about 4 MiB
  (by file size), across and within repos, so the big repos split many ways. Phase 1
  plans every repo in parallel (overlay, candidates, chunk boundaries); phase 2 verifies
  chunks on `min(threads, chunks)` threads, and a reorder buffer prints chunk k after
  0..k. `walk_repo` became `plan_repo` plus visitors, so `--rank` verifies the same docs
  through the same code. A test proves the output byte-identical for 1-byte, small and
  unbounded chunks on 2–8 threads against one thread (which always verifies whole shards),
  in all three output modes, and that an output cap fails the same way however the work
  is cut.
- Interleaved A/B against E13 on one index (the 12-CPU one, 140 shards), 5 rounds of 20
  runs, median of round medians:

  | profile           | p50 geomean E16/E13 | clear wins (5/5 rounds)                                     | consistent losses                                                |
  | ----------------- | ------------------- | ----------------------------------------------------------- | ---------------------------------------------------------------- |
  | 1 CPU / 2 GiB     | 0.993               | `word` 0.945, `short_unindexable` 0.954                     | none                                                             |
  | 4 CPUs / 4 GiB    | 0.978               | `email_regex` 0.883, `count_mode` 0.878, `date_regex` 0.955 | `case_insensitive` 1.093 (1/5)                                   |
  | 12 CPUs, no limit | 0.985               | `date_regex` 0.859, `count_mode` 0.873, `email_regex` 0.908 | `selective_vendored` 1.092 (0/5), `case_insensitive` 1.054 (1/5) |

- What it did not fix: `literal_common` and `word` stay at ~475 and ~225 ms from 4 to 12
  CPUs. Two CSV files of 20 and 24 MB in `battlefield 2` print 104k and 102k of
  `literal_common`'s 349k lines, and a file is verified by one thread, as in rg. Splitting
  one file across threads would have to replay rg's per-file semantics (`-m`, binary
  detection, line numbers) over its pieces; not tried. `serve` refuses that output anyway
  (16 MiB cap, 422).
- The regressions are left: a second thread spawn plus the barrier between the phases
  costs the cheapest queries up to ~1.5 ms at 12 CPUs. Folding both phases into one pool
  (chunks queued as their repo is planned) is the next step if they matter.
- Parity: all OK in every A/B round (Linux, freshness off), and in strict mode on Windows
  (all 20 queries). Off mode on Windows failed only on files edited since that index was
  built, as it should.
- In the server the first build cost 6% of throughput at 1 CPU (interleaved `serve-ab.sh`,
  4 rounds of 30 s, 1 client: 32.2 → 30.2 req/s, selective p50 11.2 → 11.6 ms), though it
  gained 3.5% at 4 CPUs: a served search takes ~5–10 ms, so two thread spawns per search
  and a size lookup per candidate show. Fixed: with one thread both phases run on the
  calling thread, and chunks are one per shard with no size lookups. Re-run: 1 CPU 31.2 →
  32.2 req/s, selective p50 11.8 → 10.2 ms; 4 CPUs 72.3 → 74.2 req/s. The CLI A/B above was measured before this change and not re-run.

### E17: HTTP workers for the serve mix

- `serve.sh`: `scout serve` inside the profile on the E13 indexes plus one tiny mirrored
  repo; a closed-loop client (`loadtest.py`, pinned to CPUs outside the profile) sends a
  mix weighted like agent traffic: 81% selective lookups, 14% broad sweeps (the 16 MiB
  output cap answers some with 422), 5% ranked (weights 64 : 11 : 4); 60 s each at 1, 4 and 16 clients. E16
  binary. p50/p99 ms:

  | profile, workers        | clients | req/s | selective | broad      | ranked    |
  | ----------------------- | ------- | ----- | --------- | ---------- | --------- |
  | 1 CPU, 1 (default)      | 4       | 28.4  | 97 / 530  | 166 / 688  | 172 / 457 |
  | 1 CPU, 2                | 4       | 28.1  | 90 / 489  | 219 / 751  | 218 / 393 |
  | 1 CPU, 4                | 4       | 28.3  | 38 / 287  | 339 / 1210 | 364 / 422 |
  | 2 CPUs, 1 (default)     | 4       | 51.0  | 57 / 227  | 101 / 323  | 99 / 338  |
  | 2 CPUs, 2               | 4       | 49.8  | 54 / 230  | 123 / 414  | 129 / 285 |
  | 4 CPUs, 1 (old default) | 4       | 76.6  | 41 / 134  | 63 / 190   | 65 / 192  |
  | 4 CPUs, 2 (new default) | 4       | 83.9  | 35 / 119  | 66 / 194   | 75 / 194  |
  | 4 CPUs, 1 (old default) | 16      | 69.9  | 212 / 473 | 242 / 493  | 240 / 508 |
  | 4 CPUs, 2 (new default) | 16      | 85.5  | 171 / 334 | 215 / 382  | 214 / 365 |
  | 4 CPUs, 4               | 16      | 87.7  | 160 / 313 | 227 / 464  | 235 / 385 |
  | 8 CPUs, 2 (old default) | 16      | 101.9 | 149 / 253 | 172 / 293  | 179 / 274 |
  | 8 CPUs, 4 (new default) | 16      | 103.5 | 145 / 246 | 179 / 315  | 184 / 272 |
  | 8 CPUs, 8               | 16      | 103.5 | 141 / 275 | 183 / 367  | 176 / 287 |

- At 1 CPU the throughput is fixed by the CPU; more workers only trade the broad queries'
  latency for the selective ones'. Mix-weighted p50 alone would always take more workers
  (4 at 1 CPU: 96 vs 110 ms; 4 at 4 CPUs / 16 clients: 173 vs 179; 8 at 8 CPUs: 149 vs
  152), each time by making the broad sweeps' p99 worse (+76%, +21%, +17%). The rule is
  instead: step up from 1 worker while a step lowers the mix-weighted p50 and raises no
  class's p99 by more than 10%. 1 CPU: 1 → 2 raises the weighted p50 (110 → 115), stay
  at 1. 2 CPUs: 1 → 2 raises broad p99 28%, stay at 1. 4 CPUs: 1 → 2 lowers p50 9% (4
  clients) and 18% (16) with p99s within +2%; 2 → 4 raises broad p99 21%, stay at 2.
  8 CPUs: 2 → 4 lowers p50 1% with p99s within +8%; 4 → 8 raises broad p99 17%, stay at 4.
  Hence half the cores, capped at 4: 1 at 1–3 cores, 2 at 4–5, 4 from 8.
  Not measured: 6–7 cores (3) and 12+ (4; the harness's clients need CPUs 8–11, so a
  12-CPU profile would share them). The cap keeps 12+ cores at the largest value measured
  as best rather than extrapolating `cores / 2` to 6–8.
- With one client and the default workers, selective p50 / p99 was 11 / 83 ms at 1 CPU,
  7 / 43 at 2, 5 / 26 at 4 (2 workers) and 5 / 24 at 8 (4 workers); at 16 clients 1 CPU
  gave 476 / 1165 and 8 CPUs at 4 clients 28 / 86.
- The sweep ran on the first E16 build, before its single-thread path (see E16); the
  worker comparison holds within each profile, absolute numbers are a few percent low at
  1 CPU.
- Not run with the E17 build itself: the rule only picks values measured above.

### E18: bounding the reorder buffer

- Found in review: E16's printer kept every chunk that finished ahead of the next one to
  print, so one slow chunk let the other workers buffer the rest of the output. `serve`
  caps output at 16 MiB, but a CLI search without `--max-output` could hold all of it on
  a 2 GiB box.
- A: a worker waits before starting chunk k while k ≥ printed + 4 × workers. Interleaved
  A/B against E16 (`ab.sh`, 12-CPU index, 5 rounds of 20 runs): geomean 1.041 at 4 CPUs
  and 1.042 at 12; `date_regex` 1.215 and 1.254, `email_regex` 1.189 at 4 CPUs,
  `alternation` 1.199 at 12. A broad query's chunks vary a lot in cost and print little,
  so a count window stalled workers behind one slow chunk while almost nothing was
  buffered. Rejected.
- B: the bound is on bytes, verified but not printed: a worker waits before starting a
  chunk ahead of the printer while ≥ 64 MiB is buffered; the next chunk to print always
  runs. Past the cap a search holds at most one more chunk's output per worker. Same
  A/B: geomean 0.983 at 4 CPUs and 0.996 at 12; no query was slower in all 5 rounds
  (worst `short_unindexable` 1.061 at 12 CPUs, faster in 1 of 5). `literal_common` 0.895
  and `short_unindexable` 0.836 at 4 CPUs (5/5) are not explained and not claimed as
  wins. Kept.
- Per-query peak RSS at 4 CPUs / 4 GiB with B (rss phase, same corpus): below E13's on
  every query, e.g. `literal_common` 919 → 725 MiB, `short_unindexable` 1038 → 946. The
  bench scope's cgroup peak read 2057 MiB against 1030 before, presumably the page cache
  of the corpus that the bench's own rg runs read cold after the A/B, charged to that
  scope: no scout process came near it.
- With the same change: results are consumed in chunk order, errors included, so the
  error a search reports does not depend on scheduling (`--rank` too); a worker that
  panics wakes the waiting workers so the panic is re-raised instead of hanging.
- 1 CPU never takes this path (one thread verifies inline), so the small-server profile
  is unaffected.

### E19: fenced dictionary

- Profiled first: the CLI's new `--stats` fields `postings_ms` and `verify_ms` split a
  query's time at 1 CPU into opening shards (~4.5 ms), postings (4.7–9.5 ms) and
  verification (~80 µs per false-positive candidate). Within postings, dictionary lookups
  were 3–4.5 ms: about 4 µs each, because a binary search over 16-byte entries spread
  over a 92 MiB dictionary makes ~16 probes over ~10 distinct pages per lookup, cold on a
  small box.
- Format v3 splits the dictionary into keys (u32), values (u32 count, u64 offset) and a
  fence of every 128th key. A lookup touches the fence (~1.4 KiB per shard, shared by every lookup, so it stays
  hot), one 512-byte key block and one value.
- Interleaved A/B against E18 (v2 index vs v3 index, both built at 12 CPUs, 5 rounds of
  20 runs): geomean 0.707 at 1 CPU / 2 GiB, 0.777 at 4 CPUs / 4 GiB, 0.820 at 12 CPUs.
  The small box gains most, as expected from cold pages: `selective_*` 0.56–0.63,
  `ident_regex` 0.560, `literal_none` 0.627. CPU-bound scans gain least
  (`short_unindexable` 0.960 at 1 CPU). The one slower cell is `case_insensitive` at
  12 CPUs, 1.055, faster in 2 of 5 rounds: noise-level, not claimed.
- Index 1,074,475,888 bytes vs 1,074,284,132 (+0.02%), indexing time unchanged (8.7 s at
  12 CPUs). An old index is refused with a message to rebuild it.

### E20: positional masks (rejected)

- Idea (Zoekt-style positions, cut to one byte): next to each unit in a trigram's posting
  list, store a mask with bit p mod 8 set for every offset p where the trigram starts. A
  literal's i-th trigram starts at s + i, so in a unit that holds the literal, every mask
  rotated right by its index i shares bit s mod 8. Sound by construction; a property test
  over random two-letter units (removed with the revert) checked that no unit holding the
  literal was pruned.
- An offline simulation on this corpus (a scratch script, not kept) cut candidates for false-positive-heavy
  literals by 45–75% (e.g. `release_repos`, the `selective_common_trigrams` query, 129 → 69
  units, 2 hold it). In the A/B that query was 0.884 at 1 CPU (5/5): the simulated pruning is
  real, it just does not cover the cost elsewhere. It counted
  candidates only, not the cost of reading and decoding masks; that is the blind spot.
- Interleaved A/B against E19 (both indexes built at 12 CPUs, 5 rounds of 20 runs):
  geomean 0.998 at 1 CPU / 2 GiB, 0.981 at 4 CPUs / 4 GiB, 0.962 at 12 CPUs; parity all OK.
  Wins land on long literals with many false positives: `literal_long` 0.810 / 0.850 /
  0.882, `smart_case` 0.811 / 0.853 / 0.836, `literal_rare` 0.837 at 1 CPU (all 5/5).
  Losses at 1 CPU are on queries that read many short lists or few candidates, where the
  extra bytes and per-unit decode outweigh the pruning: `selective_one` 1.087,
  `selective_long` 1.103, `selective_vendored` 1.127, `ident_regex` 1.168,
  `case_insensitive` 1.158, `short_unindexable` 1.069, all slower in 5 of 5 rounds.
- Costs: index 1,074,475,888 → 1,144,562,405 bytes (+6.5%, postings about doubled);
  indexing at 1 CPU 16.6 → 20.8 s (the extractor's 16 MiB mask table per worker replaced a
  2 MiB bitset, and the sort carries the mask); peak RSS at 12 workers 456 → 658 MiB.
- Rejected: the small box is the target and it is flat there with a whole query class
  slower, plus slower indexing. Reverted to format v3; the reverted tree re-indexes
  byte-identically to the E19 index.
- What could still work: masks in a side array per list, read only for the units that
  survive the plain intersection, and only when the smallest list is long enough for
  pruning to pay. Not tried.

### E21: cold and warm, CLI and serve, and the page-cache cliff

A measurement, not a change: production is a long-running `scout serve` on a small Linux
box, and the scout-bench numbers are fresh CLI processes over a warm page cache. Harness:
[`bench/harness/run-e21.sh`](../bench/harness/run-e21.sh), timing with
[`qbench.py`](../bench/harness/qbench.py) and evicting with
[`pagecache.py`](../bench/harness/pagecache.py), summarised by `e21sum.py`; raw results in
`bench/results/E21-cold/`. (E21 ran with a 16 MiB residency limit and no major-fault
check; both were tightened afterwards, see `qbench.py`. Every E21 cold request took
146–294 major faults, and none took zero.) Binary: the committed tree (E19 format v3, E20 reverted), index
built at 12 CPUs. WSL2 Ubuntu, cgroup profiles as in E13 (CPU quota, affinity and memory
limit per scope); the HTTP client runs on CPUs outside the server's set.

- **Making cold honest.** Before every cold request the index is evicted and the run
  fails if more than 16 MiB of it is still resident by `mincore` on the index files
  themselves. The first version checked the cgroup's own file-page counter after
  `memory.reclaim` instead, and its "cold" CLI numbers equalled warm with zero major
  faults: the pages had been loaded by an earlier warm scope, stayed charged to that
  cgroup, and survived a reclaim of this one. Now: CLI runs evict with
  `posix_fadvise(DONTNEED)` (after `sync`: dirty pages of a fresh copy cannot be dropped;
  and only while no process maps the index); the server's pages, which it maps, are
  dropped by reclaiming the server's cgroup. Major faults per request (cgroup
  `pgmajfault`) are recorded as a second witness: 146–294 per cold request, 0 warm. Each
  pass shuffles the queries (fixed seeds), every result must hash the same on every run,
  and every serve and cold body must equal the warm CLI's stdout (sha256): all OK.
- **Warm.** 20 rounds after one untimed pass. `serve` is faster than the CLI on every
  selective query, because it pays no process start, shard open or thread spawn:
  `selective_one` 12.4 → 1.6 ms at 1 CPU, `literal_none` 10.5 → 1.4, `literal_rare` 27.5 →
  9.2. It is slower on queries with huge output (`short_unindexable` 1097 → 1574 ms at 1
  CPU, `literal_common` 609 → 973), because the bench asks for the full 100–260 MiB body
  (`--max-output-mb 4096`) and the server builds it in memory and sends it over HTTP;
  production keeps the 16 MiB default cap. Geomean: 1 CPU 71.9 → 36.9, 4 CPUs 49.2 → 22.1,
  12 CPUs 51.9 → 23.6. Serve at 12 CPUs equals 4 CPUs for one client at a time: more
  cores pay for concurrent clients (E17), not single selective queries.
- **Cold.** 5 rounds. Every request costs 1–4 s, at every size and for CLI and serve
  alike: `literal_none`, which reads no content at all, takes 1038 ms served at 1 CPU
  with 146 major faults. Cold cost is not content: it is about one or two random reads of
  dictionary per shard (138 shards; Linux reads 128 KiB around each faulting page, which
  covers most of a small shard's dictionary tail), taken one after another in planning, at
  about 7–14 ms each on this WSL2 disk (sequential read 460 MiB/s). More CPUs help only a
  little (2047 → 1522 ms served). So cold latency is set by the number of dependent
  random reads per query and by the disk's latency, not by CPUs or index size.
- **The cliff.** Serve at 1 CPU, warm steady state, 5 rounds, memory limit stepped down
  (the index is 1.07 GB; "page cache" is the cgroup's file pages before each request):

  | limit    | geomean p50 ms | median major faults / query | page cache MiB |
  | -------- | -------------- | --------------------------- | -------------- |
  | 2 GiB    | 38.4           | 0                           | 1049           |
  | 1536 MiB | 36.3           | 0                           | 1041           |
  | 1280 MiB | 39.6           | 0                           | 1012           |
  | 1 GiB    | 215.2          | 64                          | 963            |
  | 768 MiB  | 1040.0         | 142                         | 709            |
  | 512 MiB  | 1594.2         | 202                         | 456            |

  No OOM at any limit: below the cliff the kernel just keeps evicting index pages
  (`memory.events` max 0 → 356,519). Holding the whole index (here ~1.2x the 854 MiB of
  indexed content) keeps every query warm; with ~94% of it cached the geomean is 5.6x slower,
  and at ~70% most of the cold cost is back.

- What it changes: the README sizing rule now says "memory for the whole index, or
  cold-read latency per query", and cold-start work goes to the number of random reads per
  query (keep per-shard hot metadata in memory, prefetch at open, fewer shards), ranked in
  the cold-start research notes below.

### E22: count-first AND planning

- Before: an AND decoded every trigram list in it, then intersected shortest first. A
  literal of n bytes has n-2 trigrams, and most of them are common (`the`, `ion`, `ent`):
  long lists decoded only to intersect away almost nothing, in every shard.
- Now the dictionary's stored unit count is read for every trigram first (a lookup, no
  decode). One zero empties the AND without decoding anything; otherwise lists are
  decoded rarest first. Which ones, two arms (both on the E19 v3 index, interleaved A/B
  against the committed tree, 5 rounds of 20 runs, `bench/results/E22{a,b,c}-ab/`):
  - **A, the 2 rarest only** (zoekt's rule): geomean 1.054 / 1.042 / 1.038 at 1 / 4 / 12
    CPUs. `literal_rare` 1.610 / 1.464 / 1.374, `ident_regex` 1.260 at 1 CPU, all 0/5.
    Zoekt can stop at two because it then checks the two ngrams' distance in each doc;
    scout has no positions (E20 tried one-byte masks), so two lists let through many
    more candidate units to verify. Rejected.
  - **B, a decode budget:** after the rarest list, the next one is decoded only while
    total decoding stays under `candidates × 4096` postings (each list priced at its count
    plus 512); counts only grow along the walk, so the first list over budget ends it. A
    tiny candidate set skips the long lists; a large one decodes them, as before.
    Geomean 0.954 / 0.969 / 0.977. At 1 CPU `selective_long` 0.775, `selective_vendored`
    0.789, `selective_one` 0.866, `literal_none` 0.876, `anchored` 0.928, all 5/5.
  - **B with 16384:** 0.958 / 0.967 / 0.976, the same overall; selective queries keep less
    of the win at 1 CPU (`selective_long` / `selective_vendored` 0.834, `selective_one` 0.866).
    4096 kept.
- `case_insensitive` (`-i todo`) read 1.138 at 1 CPU under B (1.114 under 16384, 0/5 in
  both), yet B cannot change its work: the two trigrams' lists are both decoded, the
  candidates are the same 868 docs, and `--stats` over 15 interleaved runs pinned to one
  CPU gives postings 3.27 vs 3.40 ms, verify 81.97 vs 81.69, total 88.5 vs 88.2. Timed
  standalone in the same 1 CPU / 2 GiB cgroup with stdout piped (25 interleaved runs), the
  medians are 123.6 vs 124.9 ms. The bench gap is real in its context and unexplained;
  it is not planner work. In E23's CLI A/B the same query read 0.873 at 1 CPU (5/5) with
  no change to its work, so this is most likely bench or code-layout noise between builds.
- `Shard::count` is the new port method: native reads the dictionary value, SQLite counts
  the blob's varint end bytes (it stores no count). The index format is unchanged.
- E22's own cold effect was not measured: E23's serve A/B compares E22 with E23, not E22
  with the tree before it.

### E23: marker lists and fence on the heap

- `plan_repo` read both marker pseudo-trigrams' postings for every shard on every query:
  ~280 dictionary lookups, decodes and unit-to-doc maps per query, whatever the query.
  The lists are now decoded once when a shard opens (`Shard::markers`, filled by
  `Markers::load` through the shard's own postings, so both backends share it), and the
  native fence (~1.4 KiB per shard) is copied to a `Vec<u32>`.
- Hypothesis (cold-start research item 1): much less cold I/O, because the markers are the
  largest keys, at the far end of the dictionary and the postings. **Mostly falsified:**
  median major faults per cold request fell only 190.5 → 182.5 at 1 CPU, 217 → 212 at 4 and
  218.5 → 213 at 12 (3–4%; almost every query lost 2–16 faults, `literal_none` only 2), and
  cold latency did not improve. A cold query still waits on about one read-around window
  per shard for its own trigrams; removing the marker reads does not remove that.
- Serve, `bench/harness/serve-coldab.sh`: per profile 2 interleaved reps, each a server
  started on an evicted index, cold rounds, then 10 warm rounds. 1 and 4 CPUs: 3 cold
  rounds per rep (6 samples per query). 12 CPUs: a separate, later run with 1 cold round
  per rep (2 samples per query), added to check scale-up.
  - Warm geomean per rep, e22 → e23: 1 CPU 35.8 / 37.1 → 35.2 / 34.0 ms; 4 CPUs 23.4 /
    22.2 → 22.4 / 22.4 (e23 slower in rep 2); 12 CPUs 22.2 / 22.4 → 22.3 / 22.4. Pooled
    0.934 / 0.976 / 1.003.
  - The attributable part is about 0.2 ms a query: `selective_one` and `selective_long`
    1.5 → 1.3 ms at 1 CPU and 1.8 → 1.6 at 4, `literal_none` 1.4 → 1.2. The 1-CPU pooled
    geomean also carries verification-heavy queries that E23 cannot move by 100 ms
    (`date_regex` 977 → 761 ms, `short_unindexable` 0.923, `literal_common` 0.950): noise
    over 2 reps, so 0.934 overstates the effect.
  - Cold: 1.007 at 1 CPU, 1.024 at 4, 0.994 at 12. At 1 and 4 CPUs e23 was slower in both
    reps while e22 did not drift between reps (2110 / 2112 and 1582 / 1588 ms), and the
    selective queries read 1.05–1.10 at 4 CPUs, while the broad ones read 0.93–0.98. On a
    WSL2 virtual disk with few samples: consistent in direction, unexplained.
- CLI (`ab.sh`, 5 rounds of 20): 0.997 / 1.012 / 1.007. At 4 CPUs broad queries read slower,
  `alternation` 1.118 (0/5), `word` 1.077 (1/5), `case_insensitive` 1.077 (2/5),
  `literal_common` 1.043 (2/5), `short_unindexable` 1.035 (0/5); at 12 CPUs
  `selective_one` / `selective_long` / `selective_vendored` 1.018–1.027 (0/5) and
  `selective_common_trigrams` 1.011 (1/5). At 1 CPU `case_insensitive` read 0.873 (5/5),
  the reverse of E22's unexplained 1.138, with no change to its work. A fresh process
  does the same marker work either way (at open instead of in planning), so these swings
  on unchanged work look like bench or code-layout noise between builds rather than E23.
- Kept because the production path is a long-running `serve` on a small box, where it is a
  small, consistent saving on the commonest (selective) queries and flat elsewhere; with
  the markers on the heap, `RepoPlan` also stopped copying each shard's reader list per
  query (`visit_docs` reads the shard's own; measured below).
- That last step on its own (e23 → e23b, `serve-coldab.sh`, 1 and 4 CPUs, 2 reps,
  [results](../bench/results/E23b-serve/summary.txt)): warm 0.970 at 1 CPU, 1.010 at 4;
  cold 0.953 and 0.989. Major faults are identical per query at 1 CPU and within ±3 at 4.
  About half the 1-CPU warm gain is `date_regex` (988 → 747 ms), and most of the cold gain
  is `literal_common` and `short_unindexable` (0.80–0.83): verification-heavy queries that
  skipping a per-query copy of a few doc ids cannot move that much. The selective queries,
  the ones the copy could touch, read 0.92–1.00 at 1 CPU warm, 0.99–1.10 at 1 CPU cold and
  0.94–1.02 at 4 CPUs warm; 4 CPUs warm also has `count_mode` 1.092 and
  `short_unindexable` 1.075. All of that is noise over 2 reps: no measurable cost and no
  measurable win. Kept because it is less code. Parity OK.
- Session drift ([results](../bench/results/envcheck-ab/summary.txt), one `ab.sh` round
  at 1 CPU): the E19 build (`bin-e20`, the base arm of E20's A/B) ran 1.45x slower by
  geomean late in the day than in E20's A/B that morning (80.1 vs 55.1 ms), with nothing
  else busy in WSL. Per query the slowdown is 1.04–1.93x, worst on short and mid-length
  queries (`selective_long` 8.4 → 14.8 ms, `rare_regex` 1.80x) and least on the long scans
  (`literal_common` 1.04x, `short_unindexable` 1.09x), so it is neither a fixed cost per
  process nor a uniform factor. Absolute CLI times are therefore only comparable within
  one interleaved run. The dashboard's Linux progress series multiplies each
  experiment's paired ratio onto the point before it, which assumes a ratio carries
  across sessions; given the shape above, that holds only roughly. On one build, serve
  at 1 CPU measured 34.2 and 34.8 ms warm in two later runs (E23-serve, E23b-serve's e23
  arm) but 2127 and 2266 ms cold (+6.5%); it has not been measured across the morning and
  afternoon sessions.

### E24: per-shard trigram filter

- Hypothesis (cold-start research item 4): a cold query waits on about one read-around per
  shard just to learn from the dictionary that the shard lacks one of its trigrams. A
  filter held where the page cache cannot drop it answers that without a read, so a shard
  lacking a trigram costs a cold query nothing.
- Upper bound, measured before building it on the v3 bench index (139 shards, 6,056,069
  dictionary keys; `bloomoracle.py`, `bloomsim.py`): for each literal query, the shards
  holding all of its trigrams (all an exact filter lets through) and what the chosen
  filter (6 bits a key, 4 bits set per key in one u64 word) lets through:

  | query                       | trigrams | exact | filter |
  | --------------------------- | -------- | ----- | ------ |
  | `literal_none`              | 19       | 3     | 3      |
  | `selective_long`            | 32       | 7     | 7      |
  | `selective_one`             | 6        | 51    | 51     |
  | `selective_common_trigrams` | 11       | 58    | 59     |
  | `selective_vendored`        | 29       | 74    | 75     |
  | `literal_long`              | 25       | 95    | 95     |
  | `literal_rare`              | 13       | 96    | 96     |
  | `count_mode`                | 9        | 105   | 105    |
  | `literal_common`            | 6        | 128   | 128    |

  Summed over these queries, with 4 bits set per key: 4 bits a key let 632 shard-queries
  through, 5 bits 621, 6 bits 619, 8 bits 618, exact 617. 6 bits is 4.3 MiB for this index,
  0.4% of it.

- Design: native format v5 puts the filter between the fence and the postings. It is v5,
  not v4, because v4 was E20's rejected format and its bench indexes still exist; a stray
  one is refused with the rebuild message instead of being misread. The planner asks the
  filter about the whole query tree once per shard, before it counts or decodes anything
  (`may_match`: an AND passes when every child does, an OR when any branch does). A
  rejected shard is skipped, though its always-searched docs are still verified. Inside a
  shard that passes, only OR branches are checked again, and a rejected branch is dropped:
  every AND reachable without crossing an OR already passed with the tree. `serve` copies
  each shard's filter to the heap at open. The CLI probes it through the map, on the
  expectation that copying ~4 MiB on every start costs more than one query's probes (the
  map-vs-heap A/B below measures this), and because a cold start reads it together with the
  fence that every open reads anyway.
- Expectation, written before the runs: cold major faults per query fall to about the
  shards let through times 1–2, plus the always-searched docs' content: `literal_none`
  ~145 to under 15, `selective_long` to ~20, `selective_one` to ~100, `literal_common`
  unchanged. The 11 regex queries were not in the oracle: an OR rejects a shard only when
  every branch does, and match-all plans (`short_unindexable`, `date_regex`) read
  everything, so the cold geomean moves less than the selective rows. Warm within noise:
  on a rejected shard a probe replaces a dictionary lookup, on the others it adds one.
- Built and checked ([results](../bench/results/E24-index/index-summary.txt)): the v5
  bench index with the 12-CPU profile, 9.26 s at 413 MiB peak RSS (one run; the indexing
  A/B below compares build cost properly). `v3v5same.py`: 139 shards in
  137 repos, every section byte-identical to the v3 index's except the filter; all
  6,056,069 dictionary keys pass their own shard's filter; the filters total 4.33 MiB (6.00
  bits a key). End to end (`filtercheck.py`,
  [results](../bench/results/E24-index/filtercheck.txt)): for all 9 literal queries,
  `scout search --stats`'s `shards_filtered` equals an independent Python probe of the
  on-disk filters, and the shards let through are exactly the oracle's above, so no shard
  holding every trigram is rejected. The regex queries skip 0 (`short_unindexable`,
  `date_regex`) to 63 (`anchored`) of the 139 shards.
- Indexing against base (`run-e24.sh indexab`,
  [results](../bench/results/E24-indexab/summary.txt)): a fresh index of the same repos per
  build, arms alternating, 5 builds each. CPU time (user + sys) 6.64 → 6.68 s at 1 CPU /
  2 GiB and 8.76 → 8.67 s at 12 CPUs, each median inside the other arm's range of builds.
  Peak RSS 135 → 135 MiB and 464 → 450 MiB. The index grows by the filters, 4.3 MiB
  (+0.42%). Wall time is no measure here: it varied by up to 4 s between one arm's builds
  with the disk's writeback (9.98–13.89 s at 1 CPU).
- Warm CLI, first run (the v4-format build, 5 rounds of 20,
  [results](../bench/results/E24-v4-ab/summary.txt)): 0.889 / 0.927 / 0.943 at 1 / 4 / 12
  CPUs. But at 1 CPU the queries the filter cannot touch (`short_unindexable`,
  `date_regex`, 0 shards filtered) also read 0.95 (5/5). Outside the A/B the two arms ran
  those two within ±2% of each other ([results](../bench/results/E24-foliodiag)), both
  with the page cache as the A/B left it and after evicting and re-warming both indexes
  the same way. The cause was where the page cache was charged. In rounds 1–2 the base
  arm's 2 GiB scope peaked at 2048 and 1647 MiB, against 2048 and 1035 for e24: the v3
  index's pages were charged to the A/B's own scopes, while the freshly written v4 index's
  stayed charged to the indexer's. A format-change A/B therefore evicts both indexes and
  warms them, with the corpus, from one neutral cgroup first (`run-e24.sh`'s `equalise`,
  `warmpass.py`). In that run every 1-CPU scope peaked at 1030 MiB in rounds 2–5, in both
  arms (round 1: 1090 base, 1036 e24). Sampling five scopes of the final run every 0.2 s
  found up to 1016 MiB of anonymous memory and under 1 MiB of page cache in each: the
  peaks are the benchmark's own processes, not the index. That ~1 GiB is also why an
  index charged to a 2 GiB scope, as in the first run, reached the limit.
- Equal state, still the v4-format build, 1 and 12 CPUs
  ([results](../bench/results/E24-v4-eq-ab/summary.txt)): 0.977 and 0.993, the gain all
  in the selective queries.
- Warm CLI, final: the v5 build, equal state, 5 rounds of 20
  ([results](../bench/results/E24-ab/summary.txt)). Geomean 0.961 at 1 CPU, 0.973 at 4,
  0.975 at 12; parity all OK. The selective queries carry it. At 1 CPU: `selective_long`
  0.798, `selective_one` 0.892, `literal_none` 0.905, `selective_common_trigrams` 0.917,
  `anchored` 0.943 (all 5/5) and `selective_vendored` 0.934 (4/5). At 12 CPUs they read
  0.905–0.979, all 5/5 but `literal_none` (0.914) and `anchored` (0.964), 4/5 each.
  Slower: `case_insensitive` 1.043 at 4 CPUs (0/5) and 1.062 at 12 (1/5), `word` 1.049 at
  4 (2/5), and `literal_common` and `alternation` 1.022–1.023 at 12.
- The slower rows, in process (`stats-ab.sh`,
  [results](../bench/results/E24-stats/summary.txt)): `scout search --stats` with the
  output piped, as scout-bench reads it, timed around the whole process too. 15 pairs per
  query and profile, in alternating order after 3 untimed runs of each. Both builds verify
  the same candidates at every profile (868, 5,715, 10,759 and 1,374), and the probes add
  0.05–0.44 ms of thread time to postings. The 27 shards `case_insensitive` (`-i todo`)
  skips are ones whose count-first AND would have found empty without decoding anything
  (E22). The search's own total reads 0.873–1.029 and the whole process 0.914–1.022, e24
  lower in 4–9 of 15 pairs, with the interquartile ranges overlapping in every cell; so
  neither the 0.87 nor the 1.03 is a difference. `case_insensitive` at 4 CPUs, 1.043 in
  the A/B, reads 0.946 here. These rows are bench noise, as `case_insensitive` was in E22
  (1.138 with identical work) and E23 (0.873).
- Why the CLI gains warm, although the expectation said noise: every CLI search is a new
  process, so each dictionary page it touches costs a minor fault even when cached, and a
  rejected shard skips the key block, value and postings reads of every trigram.
  `selective_long` (132 of 139 shards rejected) ran 10.9 → 9.0 ms at 1 CPU.
- Cold, and a finding about this machine: the WSL2 disks here have `read_ahead_kb` 8192,
  so a cold mmap fault reads up to 8 MiB around the page (18–22 ms), where a cloud VM's
  disk has 128 KiB (side measurement: 4 KiB faults take 0.2–0.3 ms here with
  `MADV_RANDOM`). The cold-start notes below, and E21–E23's cold numbers, assumed 128 KiB.
  CLI cold at 8192 (the v4-format build, 1 CPU, 2 reps of one round,
  [results](../bench/results/E24-maskcheck-8192/summary.txt)): `literal_none` 167 → 167
  major faults and 580 → 585 MiB read, `selective_long` 166 → 166 and 578 → 582,
  `literal_common` 180 → 180 and 988 → 976. No saving at all. Opening a shard copies its
  fence and decodes its markers, and the 8 MiB around those reads covers the dictionary,
  the filter and most postings, so after open a rejected shard and an accepted one cost
  the same I/O. Latency moved 0.81–0.97 on identical I/O: on this virtual disk cold
  latency is noisy, so the I/O counters are the measure. `qbench.py` now records MiB read
  per request, and every cold report records its read-ahead.
- The pre-registered fault counts (`literal_none` ~145 to under 15) cannot hold for the
  CLI at any read-ahead: each search opens all 139 shards, and open reads the header, the
  fence and the markers. They describe `serve`, where open happens once at start.
- **Served cold, the hypothesis's test** (`serve-coldab.sh` at 8 MiB read-ahead: 2
  interleaved reps per profile, each a server started on an evicted index, then cold
  rounds (3 at 1 and 4 CPUs, 1 at 12), then 10 warm rounds;
  [results](../bench/results/E24-serve)). Geomean 2119 → 1667 ms at 1 CPU (0.787),
  1586 → 1229 at 4 (0.775), 1564 → 1212 at 12 (0.775); parity all OK. Median major faults
  per request, against the expectation written before the runs: `literal_none` 145 → 11
  (expected under 15), `selective_long` 144 → 15 (expected ~20), `selective_one` 148 → 61
  (expected ~100), `literal_common` 187 → 178 (expected unchanged). Latency followed:
  `literal_none` 0.125–0.148, `selective_long` 0.201–0.228, `selective_one` 0.605–0.645,
  `selective_common_trigrams` 0.785–0.811, `anchored` 0.827–0.846, `rare_regex`
  0.886–0.891. Match-all plans (`short_unindexable`, `date_regex`) took the same faults in
  both arms. The fault counts are what carries to a cloud disk: each fault avoided here
  saves an 8 MiB read, there a 128 KiB one, so the MiB saved (`literal_none` 402 → 53)
  and the latency do not transfer as they stand.
- Served warm, same runs: 0.990 at 1 CPU, 1.029 at 4, 0.960 at 12. Per query the swings
  run both ways and do not repeat across profiles: `selective_vendored` 0.967 / 1.131 /
  0.955, `short_unindexable` 1.005 / 1.080 / 0.878, `date_regex` 0.839 / 1.009 / 0.945.
  On a rejected shard a heap probe replaces a dictionary lookup, which is too little work
  to move a verification-bound query by 10%; this is the 2-rep noise E23 saw.
- Memory cliff (`serve` warm at 1 CPU under shrinking limits, 8 MiB read-ahead, 2 reps
  per arm, alternating; [results](../bench/results/E24-cliff)): geomean 1.045 at
  1280 MiB, 1.022 at 1 GiB, 0.812 at 768 MiB and 0.816 at 512 MiB; parity all OK, no OOM
  kills at any limit. Once the page cache cannot hold the index, the filter does what it
  should: at 768 and 512 MiB `literal_none` reads 0.040 and 0.066 (median major faults
  78 → 2 and 92 → 3) and `selective_long` 0.170 and 0.140. At 1 GiB, where E21 found
  about 94% of the index cached, the queries swing both ways, 0.18 to 3.59: what a query
  costs there depends on which of its pages are evicted at the time. At 1280 MiB, the last
  limit at which E21's served queries ran fully cached, e24 reached the limit more often
  (`memory.events` `max`: base 1415 and 1411, e24 1663 and 3940) and refaulted on
  queries that took no faults in base: `alternation` 2 → 18, `smart_case` 4 → 14, `word`
  0 → 10, `rare_regex` 1 → 10 (2.78x slower). The likely cause is the 4.33 MiB of heap
  filters, which the kernel cannot reclaim, pushing index pages out at the edge. But
  e24's two reps differ by 2.4x in `max` events, more than the arms do, so two reps do
  not establish it.
- Map or heap for the CLI's filters (1 CPU / 2 GiB, 5 rounds of 20, equal state;
  [results](../bench/results/E24-heapcli/summary.txt)): a build whose CLI copies every
  filter to the heap at open, as `serve` does, reads 1.104 against probing through the
  map. The selective queries read 1.23–1.37, all 0/5 (`selective_long` 10.1 → 13.9 ms,
  `literal_none` 9.6 → 13.2, `selective_one` 12.1 → 16.2): copying 4.33 MiB on every
  start costs about 4 ms, more than these queries' whole search. The CLI keeps the map.
  Only 1 CPU was measured: the copy runs in the parallel shard open, so more CPUs would
  spread it rather than test it.
- Expectation for a cloud disk's 128 KiB read-ahead, written before those runs
  (`run-e24.sh` `serve128` and `clicold`): a cold fault now reads at most 128 KiB, so a
  shard's dictionary no longer arrives with one read-around, and both arms take more,
  smaller faults than at 8 MiB. MiB read falls in both. Served, a rejected shard still
  costs nothing, so the fault ratios follow the share of shards let through, as at 8 MiB
  (0.08, 0.10, 0.41 and 0.95 for `literal_none`, `selective_long`, `selective_one` and
  `literal_common`), plus the always-searched docs' content. The selective queries'
  latency ratios follow their fault ratios. CLI cold: both arms pay open's header, fence
  and marker reads in all 139 shards, so the selective queries' fault ratios land between
  serve's and 1: roughly 0.3–0.7 for `literal_none`, depending on how many faults a
  shard's lookups take next to its open. But no longer 1.0, as the 8 MiB mask made them.

### E25: read-ahead indexing, found on chromium

- Setting: the first test of scout on large public repos. Shallow clones of
  `chromium/chromium` (510,212 files indexed, 2,958 MiB) and `llvm/llvm-project` (187,463
  files, 2,222 MiB), 5.1 GiB of content and a 5.7 GiB index, on the dev box (Windows 11,
  Defender real-time scanning on). The query suite is
  [`bench/queries-big.tsv`](../bench/queries-big.tsv), the same classes as the bench suite
  with patterns that occur in these trees. Results are in
  [`bench/results/E25-big/`](../bench/results/E25-big).
- What broke: indexing took 4,088 s (68 min), and the process had used 245 s of CPU at
  65 min (sampled while it ran; index-ab.txt). `index_repo` read, classified and trigram-split every file of a repo serially on its worker, and a
  worker is a repo: chromium got one thread that waited on 510k file opens one after the
  other, each paying Defender's scan. More workers do not help one huge repo.
- Change: `read_ahead` (indexer.rs) runs read + classify + trigram extraction on
  `read_threads` threads (the worker's `walk_threads`, cores / workers) and hands results
  to the writer in path order, so the writer does exactly what the serial loop did and the
  shards are byte-identical (a unit test builds a repo at 0, 1, 2, 4 and 16 threads and
  compares every writer call; on chromium + llvm-project all 14 shards hashed the same as the base build's,
  [shard-hashes.txt](../bench/results/E25-big/shard-hashes.txt)).
  Claimed-but-unwritten files may cost at most an eighth of the shard cap, at their size
  plus 4 KiB each (so runs of empty files cannot race ahead): 5–13% of the 5x-shard memory
  a worker plans for, depending on how many distinct trigrams the text has. Binary files
  are cut to their NUL prefix as soon as they are read. A panic on either side stops the
  other, so a failing build panics instead of hanging (both directions tested under a
  timeout).
- Windows, chromium + llvm-project ([index-ab.txt](../bench/results/E25-big/index-ab.txt)):
  the cold base build 4,088 s; on a warm cache, e25 944 s and 998 s, base stopped after
  1,526 s unfinished, with llvm-project alone 1,420 s against e25's 302 and 385. These
  e25 builds predate the review fixes (two condvars, the per-file cost, binary
  truncation); their shards are the same.
- Linux small-server profiles, the 137 bench repos (`run-e25.sh`, 5 alternating builds,
  [first run](../bench/results/E25-indexab-v1/summary.txt),
  [1 CPU inline](../bench/results/E25-indexab/summary.txt)): with a reader thread at every
  profile, CPU time rose 4% at 1 CPU (7.21 → 7.50 s, wall 11.65 → 14.25 s median, both
  arms' ranges overlapping), 6% at 4 CPUs and 20% at 12, while wall time fell 8% at 4 CPUs
  (5.18 → 4.77 s) and 22% at 12 (4.83 → 3.78 s). At 12 CPUs each of the 12 workers has one
  reader, which overlaps the long tail of large repos. On one CPU the handoff is pure cost,
  with no core to overlap it on and reads coming from the page cache, so one CPU now reads
  inline: 7.12 → 7.25 s CPU, 10.84 → 11.10 s wall, the same peak RSS (135 MiB). Peak RSS
  is equal or lower at every profile, and every build wrote identical shards.
- Search, same index, against rg ([search.txt](../bench/results/E25-big/search.txt), stopped
  after 8 of 20 queries: rg took 134–293 s a query, opening 697k files through Defender):
  scout 41–521 ms, 451–3,246x faster, parity OK on all 8. Scout alone, all 20 queries plus
  three that match nothing, 20 runs each
  ([scout-only.txt](../bench/results/E25-big/scout-only.txt), `bench/harness/scout_only.py`):
  selective queries (under 1,000 candidates) 34–54 ms, broader ones 84–471 ms, `return`
  (2.8M lines from 209k files) 1.4 s, and the three full scans (no indexable literal, all
  697,675 docs verified) 0.9–2.7 s: `\bxq\d{12}\b` (Unicode `\d`) 926 ms with 0 results,
  `date_regex` 1.1 s, `if` 2.7 s for 4.9M lines.
- Left open: a full scan that prints little takes up to twice the search's own `total_ms`
  as a process (926 vs 458 ms for `\bxq\d{12}\b`, 1,149 vs 681 for `date_regex`), possibly
  releasing 5 GiB of touched mapped pages at exit (E6 territory); E26 found and fixed it.
  And `read_threads` is fixed per run: once llvm-project finished, chromium kept only its
  6 of 12 readers.

### E26: release shards, not repos, in parallel (and E27, a bigger stdout buffer)

- Found: E25's "left open". After a search, `release_repos` unmapped the opened repos
  across threads, one repo per task (E6). With two repos, two threads unmapped the ~5 GiB a
  full scan touches, chromium's ~3 GiB on one of them, at E6's ~0.17 ms per MiB: about
  460 ms after the search (`date_regex` 712 ms searching, 1,170 ms as a process, median of 3,
  stdout to NUL; [teardown.txt](../bench/results/E26-big/teardown.txt)). E26 leaves about
  170 ms (644 and 816).
- Change: every opened shard goes into one queue that `threads` threads pop and drop, so the
  unit is a shard (14 here, 139 on the bench corpus) and a thread that finishes early takes
  the next one. One thread (1 CPU) drops every shard, as before.
- Interleaved A/B on chromium + llvm-project, scout alone, 5 rounds of 6 runs, output read
  through a pipe ([ab.txt](../bench/results/E26-big/ab.txt)): faster on 7 queries in 5/5
  rounds, and within noise on the two under 50 ms (`literal_none` 0.974, 4/5;
  `selective_symbol` 0.970, 5/5). `nomatch_unicode_regex` 933 → 654 ms (0.701),
  `date_regex` 1,167 → 878 (0.752), `literal_long` 477 → 364, `literal_common` 274 → 210,
  `literal_ident` 84 → 67, `short_unindexable` 2,605 → 2,305, `huge_output` 1,446 → 1,243.
  Output byte-identical to E25's on `if`, `return`, `date_regex` and `RenderFrameHostImpl`
  (sha256). Not measured on the Linux profiles: at 1 CPU the code does what it did, and on
  the bench corpus a shard is almost always a whole repo.
- Where that leaves the suite ([scout-only.txt](../bench/results/E26-big/scout-only.txt), 20
  runs): 21 of 23 queries under 1 s at the median, `date_regex` the slowest at 870 ms (p90
  892). The two over it print most of the corpus: `return` 480 MB in 1,227 ms and `if`
  903 MB in 2,225 ms through a pipe, 542 ms and ~1.0 s to NUL. A pipe into the reader the
  bench uses (Python `capture_output`) is the floor: writing the same bytes from memory
  takes 808 ms and 1,510 ms in 1 MiB writes (833 and 1,505 in 8 MiB ones), 1,168 and 2,966
  in 64 KiB writes
  ([pipe-ceiling.txt](../bench/results/E26-big/pipe-ceiling.txt)).
- E27, rejected: the CLI's stdout `BufWriter` 64 KiB → 1 MiB, since the ceiling run showed
  64 KiB writes costing far more than 1 MiB ones. No change (3 rounds of 5:
  `huge_output` 1.021, `short_unindexable` 0.992, the rest 1.001–1.026,
  [ab.txt](../bench/results/E27-big/ab.txt)): verification chunks are written whole, and a
  `write_all` larger than the buffer bypasses it, so the writes were already chunk-sized.

### E28: an EBS-like volume, and read-ahead of the content to verify

- Why: deploying to EC2 + EBS on a small instance. The index cannot be held in memory:
  chromium + llvm-project is 5.8 GiB (5.2 content, 545 MiB postings) on a 4 GiB box, and
  gp3's baseline is 125 MB/s and 3000 IOPS, with ~0.5–1 ms a read.
- Harness ([bench/harness](../bench/harness), `ebs-*.sh`, `run-ebs.sh`, `ab-ebs.sh`): a
  `dm-delay` volume in WSL2 with a 128 KiB read-ahead, and each run in a cgroup with 2 CPUs,
  `memory.max` 4 GiB (no swap; the page cache is charged to it) and gp3's `io.max`. Index
  built in the same box (52 shards, 325 s). `qbench.py` CLI, warm (steady state: every
  query in turn, 3 rounds, the index bigger than the box's memory) and cold (the index
  evicted before each query). Read delay 0 ms: `dm-delay` waits in 4 ms ticks.
- Baseline e26 ([results](../bench/results/E28-ebs/base-e26/summary.txt)): steady state
  geomean 465 ms, 14 of 18 queries under 1 s, `date_regex` 20 s reading 2.3 GiB; cold
  geomean 7.8 s, with even selective queries reading 160–490 MiB. The page faults were the
  cost: each reads a 128 KiB window around one page, one IOPS at a time.
- Change: `Shard::prefetch`. Each verifier queues `MADV_WILLNEED` over its candidates'
  stored content in ~4 MiB blocks (adjacent ranges coalesced), the next block queued when
  verification reaches the start of the one before, so the kernel reads ahead in large
  requests while the thread verifies. A doc larger than the block is its own block.
- Result, 2 interleaved reps ([results](../bench/results/E28-ebs/ab-0ms/summary.txt)):
  cold geomean 0.696, ~40% fewer bytes read, faster on 16 of 18 queries in both reps.
  Steady state 0.940 but noisy (each query's time depends on what the one before left in
  the cache): `alternation` 0.47, `smart_case` 0.37, `literal_common` 3.8x worse in both
  reps. Per round, e28 read 3.0–3.3 GiB against e26's 3.3–4.2. **Kept**, for the cold
  path; the steady state is E29's.

### E29: lz4 content blocks, chosen by index size against memory

- Why: E28 left the steady state read-bound: 4 GiB cannot cache a 5.8 GiB index, so every
  round reads gigabytes again. poc-compress measured lz4 per 64 KiB unit at 3.5x on the
  bench corpus, ~3.6–4 GB/s decode a core; E15 (lz4 per whole doc) lost 2x on an index
  that fits RAM, so the codec has to be a choice.
- Change: native format v6. A doc's content is stored as its units' spans (the
  line-aligned ~64 KiB blocks, `unit_spans`), each lz4-compressed only where that is
  smaller, with a blocks table of (stored, raw) lengths; the reader checks the block range
  and lengths against the doc row before decoding. `content` borrows the map when every
  block is raw and decodes into a per-thread buffer otherwise. `scout index --content
  auto|raw|lz4`, default `auto`: lz4 once the content of every registered repo (the
  jobs' as listed now, the rest from the catalog) is more than half of usable memory.
  `sync` and `serve` take the same flag.
- chromium + llvm-project at 2 CPUs / 4 GiB ([log](../bench/results/E28-ebs/idx-e29.log)):
  `auto` picked lz4; the index is 2.3 GiB (content 5.0 → 1.57 GiB, 3.2x) and built in
  194 s against 325 s raw (less to write).
- EBS A/B against e28, 2 interleaved reps
  ([results](../bench/results/E29-ebs/ab-0ms/summary.txt)): steady state geomean 0.567
  with 0 MiB read from disk (the whole index stays cached): `date_regex` 20.9 → 2.3 s,
  `crbug_regex` 2.2 → 0.26 s, `literal_common` 3.8 → 0.58 s. 16 of 18 queries under 1 s;
  `literal_long` 1.6 s and `date_regex` 2.3 s are not. Slower: `selective_symbol` 23 → 36
  ms, `llvm_regex` 32 → 46, `anchored` 0.67 → 0.75 s, `alternation` 0.73 → 0.79 (the decode
  of whole candidate docs). Cold geomean 0.746, about half the bytes read.
- Bench corpus, Linux profiles, 4 interleaved rounds of 10 runs, rg parity green on every
  run ([results](../bench/results/E29-bench); index 1.1 GiB raw, 421 MiB lz4). The v6
  format with E28's read-ahead, raw, against e26 on v5: geomean 0.813 at 1 CPU / 2 GiB,
  0.851 at 4 / 4 GiB, 0.899 at 12, faster on 19 of 20 queries at each (`literal_common`
  0.98–1.07, within noise). lz4 against raw on that index, which fits every profile's
  memory: 1.93x slower at 1 CPU, 1.60x at 4, 1.53x at 12, worst on selective queries that
  verify a few large docs (`literal_rare` 19 → 61 ms at 1 CPU), equal on those with no
  candidates. That is the case `auto` keeps raw: 854 MiB of content is under half of 2 GiB.
  One Windows `cargo clippy` ran during the first of these A/Bs (a minute, one run;
  rounds alternate arms).
- **Kept**: format v6 and `--content auto`. Left open: decoding only the units that can
  match, not the whole doc, which would cut lz4's cost on selective queries; line numbers
  then need each unit's first line number.
- Review fixes after the runs (bounds only, not re-measured): a block whose raw length is
  more than 255x its stored length (lz4's limit) or smaller than it is refused before a
  buffer is sized from it; a repo `auto` cannot list fails the run instead of counting as
  empty; `sync` and `serve` take `--content`, and `auto`'s memory read names it when the
  limit cannot be read.

### E30: no read-around on posting lists

- Why: an index bigger than memory (hundreds of repos on a small box) pays its cold lookup
  data on every query. `coldmap.py` (the shard pages a query left resident, by section;
  it read v6 then and reads v9 now) on E29's index at 2 CPUs / 4 GiB, IOs from
  `ebs-run.sh`'s io.stat: `selective_long` left 133 MiB resident in 1,206 IOs to verify
  24 MiB of content; keys 17.6, vals 34.1 and postings 34.5 MiB of it. That output was
  not saved; the A/B below measures the same query at 149 → 84 MiB read. Each lookup touches a
  few hundred bytes and each fault read 128 KiB around it.
- Change: `postings` asks for exactly a list's range (`MADV_WILLNEED`, lists over a page),
  and an opened shard's sections go `MADV_RANDOM`. A: both the dictionary (keys, vals) and
  the postings. B: the postings only.
- A on v6 against e29 ([results](../bench/results/E30-ebs)), 2 interleaved reps:
  `selective_long` 133 → 68 MiB resident (`coldmap.py`, not saved) but 1,206 → 2,853 IOs.
  4 GiB: cold geomean 0.853 (`llvm_regex` 0.62, `selective_symbol` 0.63), steady state
  0.997. 2 GiB: cold 0.868; the steady state misses there: **1.455**, `selective_long` 65 → 329 ms, `count_mode` 2.7x,
  `literal_none` 2.1x. Without read-around each dictionary miss is a synchronous 4 KiB
  read, where one read-around had fetched the pages the next lookups need. e29 had no
  per-list WILLNEED, so these numbers are A and it together; A's 1 GiB run below has
  WILLNEED in both arms.
- B on E34's v9 lz4 index ([results](../bench/results/E30b-ebs)), against no
  `MADV_RANDOM` (both arms keep the per-list WILLNEED), same build otherwise: 4 GiB warm
  1.002, cold 0.917 (`selective_long` 0.73, `literal_none` 0.75), faster on 17/18 cold
  queries in both reps; 2 GiB warm 0.995, cold 0.914. Since E33 the 2 GiB steady state
  reads nothing, so the bigger-than-memory case was run at **1 GiB**: B warm **0.893**
  (1,918 → 1,433 MiB read a round, faster on 14/18 in both reps), cold 0.914. A at 1 GiB: warm
  **1.094** (`selective_symbol` 2.1x, `selective_long` 1.7x, `literal_none` 1.7x), cold
  0.799 (5,812 against B's 6,377 MiB). Outputs byte-identical across arms (`ebsab.py`).
- Final tree (B, E31 removed, E32–E35): rg parity all OK on the bench corpus, v9 raw
  and lz4, at 1 CPU / 2 GiB and 12 CPUs (`parity-e30.sh`,
  [output](../bench/results/E30b-ebs/parity)).
- **A rejected, B kept.** A reads the least cold, but the steady state of an index bigger
  than memory is what a small box lives in.
- Not measured alone: the per-list `MADV_WILLNEED`. It is in both arms of every B run
  and of E31's, and in E32's arm against e29. Two reps of 3 warm and 1 cold round each,
  no p99: the cold results agree across reps (B faster on 17/18 at 4 GiB in both), the
  1 GiB warm ones less so.

### E31: count only the trigrams the shard before ranked rarest (rejected)

- Why: E22 counts every trigram of an AND in every shard to find the rarest one or two it
  decodes. A 36-byte literal is 34 dictionary lookups per shard, ~1,700 over 51 shards,
  and on a cold disk each lookup can be a read.
- Change: a repo's shards, planned in order, shared the counts seen so far. Once the hint
  knew every trigram of an AND longer than 6, a shard counted only the 6 the hint ranked
  rarest and left the rest out of the AND (a superset of candidates, so still exact).
- Same tree with the hint on against `COUNTED_PER_AND` above any AND, v9 lz4 index,
  2 interleaved reps ([results](../bench/results/E31-ebs)). Cold, the case it was for:
  geomean 0.993 at 4 GiB and 1.013 at 1 GiB, with 6,349 → 6,377 MiB read: no fewer bytes.
  Warm 4 GiB 1.018, with `literal_none` 10 → 20 ms (0/2). Warm `--stats` runs outside
  the cgroup (not saved): e29 76 candidates, verify 2–9 ms, 12–14 ms wall; the E31 + E32
  snapshot 80 candidates, verify 15–23 ms, 23–24 ms; that snapshot with the hint off 76
  candidates, 13–14 ms. Warm 1 GiB 0.927 (`selective_symbol` 0.62, `selective_long`
  0.66), with 1,455 → 1,474 MiB read a round; why it was faster there was not looked into.
- **Rejected and removed.** What it was for, fewer cold dictionary reads, did not happen,
  and a wrong pick costs candidates, which cost the no-match query twice its time. The
  1 GiB warm gain is the one result for it; two reps did not make it worth the code.
  Probably the skipped counts' pages were mostly read anyway by the lookups that remained
  (a key block holds 128 keys); not checked with `coldmap.py`.

### E32: read only a large candidate's candidate units

- Why: E29 decoded and searched a candidate whole, so a 2 MiB file with one candidate
  64 KiB unit cost 32 units of lz4 and regex. Units and content blocks are the same
  line-aligned spans for text, so a unit maps to one block.
- Change: native format v7. A block row gains the doc's line count before it, so a unit
  read alone keeps its file line numbers; the vendored grep-searcher gains
  `Searcher::set_first_line_number` (a scout patch: rg has no other way to start
  numbering at N). `Shard::unit_content` returns the stored or decoded bytes of the asked
  units as runs, each with its first line; `prefetch` takes the same wants, so read-ahead
  covers only those blocks. Planning keeps each candidate's units. Whole-file reads stay
  for docs of one unit, reader-mode (binary, BOM) docs, always-searched docs, `-c` and
  `-m` (E33 later carried counts and limits across units), ranking and walked repos.
  SQLite returns the doc as one run from line 1.
- Measured with E31 and E30's per-list WILLNEED, which were built together, so the
  numbers below include both (E31's own A/B, above, is neutral cold): a copy of the E33
  baseline snapshot without `MADV_RANDOM` (`build-e32.sh`) on a v7 lz4 index, against e29
  on its v6 index, 2 interleaved reps ([results](../bench/results/E32-ebs)). 4 GiB warm
  **0.915** with no disk reads: `smart_case` 0.72, `llvm_regex` 0.72, `rare_regex` 0.73,
  `selective_symbol` 0.75, `case_insensitive` 0.79; `literal_none` 10 → 20 ms is E31's
  (above). Cold 0.956, 8,714 → 7,866 MiB read over the 18 queries, but broad queries
  were slower cold in both reps while reading less: `ident_regex` 1.08, `case_insensitive`
  1.05, `alternation` 1.03, `anchored`, `rare_regex` (7/18 slower in both at 4 GiB, 9/18
  at 2 GiB, `alternation` 1.20 there). Unexplained; smaller or more scattered reads are a
  guess, not measured. 2 GiB warm 0.954,
  1,337 → 1,199 MiB a round (per-query ratios there, 0.33–1.98, follow what the query
  before left cached, as in E28); cold 0.980. Outputs byte-identical across arms.
- **Kept**: format v7 (since replaced by E34's v9, which keeps the line counts).

### E33: lazy content visits and initialized decode-buffer reuse

- Compared against a snapshot of the current working tree, including E31 count hints
  and E32 candidate-unit reads, on one freshly built v7 LZ4 index. No format change.
- Native content is visited in line-aligned blocks, stopping decoding on `-l`'s first
  match or a file-wide `-m` limit. One printer sink spans the file, so counts and limits
  also work across candidate units. Reader-mode files, ranking and haystack anchors
  retain whole-document matching. Decode scratch keeps initialized bytes between reads.
- 2 CPUs / 4 GiB, no swap, EBS-like 125 MB/s / 3,000 IOPS, chromium + llvm-project:
  five interleaved rounds of two samples per arm/query, 19 cases, stdout to `/dev/null`.
  All 380 timed samples had zero disk reads; index residency was verified zero before
  the hash/warm-up pass populated it from the limited cgroup.
- Geomean latency ratio **0.962**. Common counts 562 → 529 ms, common `-m 1` 544 → 503,
  date filenames 2,115 → 1,935, `if -m 1` 3,766 → 3,523. Full `if` output 4,226 → 4,119;
  full license and `return` output effectively unchanged. No-match literal 20.8 → 21.3
  ms, slower in 4/5 rounds. Twelve cases improved in all five rounds.
- Byte-identical output/exit codes on all 19 cases, including streamed hashes of the
  full 812 MB `if` and 429 MB `return` outputs. Windows and Linux workspace tests and
  release Clippy passed. New tests prove early stop avoids decoding a corrupt later
  payload and exercise buffer reuse, file-wide counts/limits and unit-boundary lines.
- Independent rg parity passed all six broad-query oracle cases at 2 CPUs / 4 GiB,
  with disk throttling lifted for the oracle after the timed A/B completed.
- [Full report](lazy-decoding-benchmark.md), [samples](../bench/results/E33-lazy/warm.json),
  [resource log](../bench/results/E33-lazy/warm.log), and the separate rg parity gate in
  `bench/results/E33-lazy/rg-parity.json`.

### E34: broad queries below one second at 2 CPUs / 4 GiB

- Compared with the E33 working-tree snapshot on chromium + llvm-project, 697,675
  files / 5,126 MiB content. Five alternating rounds, two samples per query/arm, 19
  queries. Each arm warmed its own index in a fresh 2 CPU / 4 GiB cgroup, with no swap
  and EBS-like 125 MB/s / 3,000 IOPS caps. All 380 timed samples recorded zero disk reads.
- **19/19 warm CLI medians below one second**; all cases faster in every paired round.
  Full `if` output **4,161.6 → 957.0 ms**, date lines **2,012.4 → 362.6 ms**, `return`
  lines **2,377.3 → 714.0 ms**, license lines **1,602.0 → 328.0 ms**. Geomean ratio
  **0.294**, about 70.6% less latency. Output is still fully formatted; timed stdout
  goes to `/dev/null`, freshness is off. Cold latency and HTTP transfer are not measured.
- Full `if` samples range **930.2–1,046.6 ms**; 9/10 below one second. That was the
  only candidate sample above one second out of 190, so this is not a strict tail SLA.
- Direct plain-text formatting and exact-literal byte search; bounded output-buffer
  recycling and path reuse; reference bounds-checked LZ4 decoding; native v9 tagged
  two-byte postings and conservative numeric shape summaries. Unicode digits remain
  supported. Binary/BOM readers and complete-file count/match limits retain rg semantics.
- Small blocks below 4 KiB stay raw, trading cache space for fewer decoder calls and
  content copies. The index grows **2,354.5 → 2,863.4 MiB (+21.6%)**. Maximum charged
  batch memory **1,935 MiB**, no OOM kills; the index build also completed inside 4 GiB.
  Previous native/SQLite indexes require `scout index` to rebuild.
- Exact output hashes, byte counts and exit codes match for all 19 corpus cases.
  The independent rg oracle also passes all six broad-query cases after timing.
  Windows and Linux workspace tests and release Clippy pass, including Unicode/byte
  edge cases, small raw-file borrowing, corruption and file-wide output limits.
- Cold-index follow-up on the final binary: three runs per case, index residency
  verified zero before all 18 samples, same 2 CPU / 4 GiB and disk caps, executable
  warm. Full `if` **14.58 s**, `return` **12.18 s**, license **6.59 s**, dates
  **3.55 s**; selective symbol / no-match **700 / 515 ms**. Full `if` reads 1.64 GiB
  from disk. All six separate cold-output checks match the warm outputs; no OOM kills.
  [Cold samples](../bench/results/E34-subsecond/cold/summary.json) and
  [resource log](../bench/results/E34-subsecond/cold.log). These measure cold index data
  under local bandwidth/IOPS limits, with no extra injected delay, not a full-machine
  boot. The subsequent original-build comparison below provides the missing baseline.
- Compared directly with the original task-start build (before E33 and E34), all 19
  measured warm medians are lower; geomean ratio **0.284**. Cold is mixed across the
  matching six-case, three-run sweeps: date lines **14.37 → 3.55 s**, `return` lines
  **23.96 → 12.18 s**, full `if` **14.37 → 14.58 s**, license **6.41 → 6.59 s**,
  selective symbol **738 → 700 ms**, no-match **462 → 515 ms**. Selective-query ranges
  overlap. Both builds' cold outputs match exactly; 35/36 timed samples start at zero
  index residency, one original date sample at 128 KiB, below the unchanged 2 MiB guard.
  These are separate sweeps, not interleaved original/final rounds.
  [Original comparison and source reports](../bench/results/E34-subsecond/original-comparison.json).
- [Full report](subsecond-search-benchmark.md),
  [final samples](../bench/results/E34-subsecond/ab-final/summary.json),
  [resource logs](../bench/results/E34-subsecond/ab-final), and
  [incremental patch](../bench/results/E34-subsecond/implementation.patch).

### E35: bounded interactive batches and explicit count lower bounds

- Added `search_bounded` to the shared Rust API, `--max-results` / byte/time budgets
  to the CLI, and `limit` / `max_bytes` / `timeout_ms` to HTTP. Planning stops before
  later shards and lazy block visits stop on the batch budget. The existing exact
  grep/count path retains its output; all 19 E34 exhaustive hashes/exit codes pass.
- Bounded counts return tagged `exact` or `lower_bound` values with completion and
  stop-reason metadata. Ordinary `-c` remains exhaustive. No actual MCP protocol
  adapter was added; this is the core/CLI/HTTP integration for an interactive caller.
- Repartitioned/cycled real Chromium/LLVM plain text into 250 fixture repos:
  1,131,349 documents / 8.12 GiB, 4.88 GiB native LZ4 index. This is synthetic scale
  testing, not actual NX repositories. One tiny local sync-control repo makes 251
  entries in the real server.
- Persistent HTTP, 2 CPUs / 4 GiB, no swap, 125 MB/s / 3,000 IOPS / 128 KiB read-ahead,
  zero extra injected latency. Three shuffled cold rounds, 0 MiB index residency
  before all 33 requests; heap filters remain live. Cold 100-line medians: `if`
  **6.9 ms**, `return` **11.4**, `export` **34.4**, date **119.7**, symbol **455.8**.
  An unindexable sparse scan stops at **1,008 ms**, explicitly incomplete.
- Cold `if` count: threshold 100 in **6.6 ms**; one-second budget returns lower bound
  **291,584** in **1,006.5 ms**. Exact exhaustive count is **8,585,236** in **26.02 s**.
  These are different answers, not an exact-count speedup. Deadline checks are
  cooperative; queue/transport/startup are outside the core processing budget.
- HTTP prefix/count oracles, raw/LZ4/SQLite boundary tests, full Linux release tests
  and workspace release Clippy pass. Peak served memory **667 MiB**, zero OOM kills.
- [Contract, limitations and reproduction](bounded-mcp-search.md),
  [accepted HTTP samples](../bench/results/E35-bounded/http-final/summary.json),
  [oracle](../bench/results/E35-bounded/oracle-final.json), and
  [incremental source evidence](../bench/results/E35-bounded/evidence/source-evidence.json).

### E36: the server keeps every shard's dictionary on the heap

- Why: with 4 GiB as the smallest box (2026-10-28), most of a big index lives on disk, and
  `serve` is the path whose per-query cost matters (a CLI query also pays process start
  and 26–50 ms of unmapping on broad queries, `phases.py`). Per section of a cold
  selective query (`coldmap.py`, CLI, v9): `selective_long` read 107 MiB, keys 18.2 and
  vals 34.3 of it, content 23.2; no-match 57 MiB, keys and vals 29. A lookup touches 16
  bytes; a cold one reads the 128 KiB around them. All keys, vals, fence and filter of
  chromium + llvm-project are ~105 MiB (`sections.py`).
- Change: `FilterHome` becomes `LookupHome`. With `Heap` (`serve`) an opened shard copies
  its keys and values sections to the heap along with its filter (E24) and fence (E23);
  the CLI keeps reading them through the map.
- `serve-ab.sh` (`serve-ebs.sh` per arm: the server in a 2 CPU / 4 GiB cgroup at gp3's
  caps, started on an evicted index, `qbench.py` warm 3 rounds, then cold, the index
  evicted before each request) against HEAD, 2 interleaved reps
  ([results](../bench/results/E36-serve/ab-4G)). Cold geomean **0.778**, faster on 17/18
  in both reps: `selective_long` 522 → 232 ms (84 → 43 MiB read), `literal_none` 242 →
  79, `llvm_regex` 576 → 315, `selective_symbol` 643 → 360, `literal_ident` 1,239 → 886.
  Warm 0.994 (`case_insensitive` 0.91, `smart_case` 1.09). Outputs identical. Time to
  ready on an evicted index 0.67–0.81 → 1.34–1.38 s: the open reads the dictionaries.
- **Kept.** The heap grows with the index, 16.75 bytes per key, ~4% of this one: an index
  several times memory would hold a matching share of a 4 GiB box. Not yet bounded (E37
  bounds it).

### E37: the server keeps every table on the heap

- A (rejected, a probe): content sections `MADV_RANDOM` too, since verification already
  asks for each candidate's blocks (E28). `coldmap.py`, CLI, cold, 2 CPUs / 4 GiB: content
  read 23.2 → 22.8, 32.0 → 31.5, 62.1 → 61.5 MiB on `selective_long`,
  `selective_symbol`, `literal_ident`, in 1,266 → 2,453, 1,358 → 2,548, 3,311 → 4,502
  IOs. The content a selective query reads is its candidates' own blocks, not
  read-around; without it the same bytes take twice the requests. Not A/B'd.
- B: with E36, a cold selective query still read its candidates' doc rows, paths, blocks
  and units through the map, each a 128 KiB read-around for a few bytes. Change: `Heap`
  copies every section between content and postings (docs, blocks, paths, units, keys,
  values, fence, filter; ~211 MiB on chromium + llvm-project, 8% of the index) in one
  slice; the shard reads them all through one accessor by absolute offset, so the CLI's
  mapped path and the server's heap path are the same code.
- `serve-ab.sh` against E36, 2 interleaved reps at 2 CPUs / 4 GiB
  ([results](../bench/results/E37-serve/ab-4G)). Cold geomean **0.784**, faster on 18/18
  in both reps: `literal_none` 83 → 40 ms, `selective_long` 210 → 133 (43 → 26 MiB
  read), `llvm_regex` 352 → 183, `smart_case` 871 → 480, `date_regex` 3,662 → 2,714;
  bytes read cold fell on every query. Warm 0.984. Time to ready on an evicted index
  1.37–1.52 → 2.25–2.32 s.
- Bound: the heap grows with the index, so it is capped. `serve` gives each generation a
  `TableBudget`, by default an eighth of usable memory (`--table-mb` sets it), taken
  first come, first served at open; a shard that does not fit reads its tables through
  the map, as the CLI does. Two generations usually overlap while a sync opens the next
  (more only while a search outlives a second sync), so tables usually hold at most a
  quarter of memory. 512 MiB on a 4 GiB box holds every table of an index ~6 GiB at
  this 8% (extrapolated). Against the unbounded build, 2 reps
  ([results](../bench/results/E37-serve/budget-4G)): cold 0.998 with identical bytes,
  warm 1.014 (noise: only open changed), ready 2.22–2.26 → 2.23–2.31 s. That index's
  211 MiB fits the budget, so this measures only the bookkeeping; over budget, which
  shards stay mapped depends on open order (parallel) and is covered by a unit test,
  not measured.
- Parity gate ALL OK on raw and lz4 at 1 and 12 CPUs. **B kept**, bounded.

### E38: content blocks in lz4 HC

- Why: profiled warm (`perf`, CLI, 2 CPUs, v9 lz4 index cached;
  [profiles](../bench/results/E38-ebs/probes/perf.txt)), broad queries spend most of
  their time decoding content: `LZ4_decompress_safe` is 63% of `literal_common`, 61% of
  `literal_long`, 57% of `files_mode` and `word`, 56% of `alternation` and
  `case_insensitive`, 37% of `anchored` (39% DFA), 25% of `date_regex` (26% DFA). The
  whole index's 5.4 GB decodes in 580 ms on 2 cores (`scan-cost`), ~4.6 GB/s a core, the
  top of lz4's range, so the decoder is not the lever; what it is given is.
- First, a probe of query rewrites (should `-w`, `-i`, `-S` literals take a byte search?),
  CLI median of 7, the same candidates in each pair
  ([timings](../bench/results/E38-ebs/probes/rewrites.txt)): `-w DCHECK` 174 ms against
  `-F DCHECK` 159; `-i deprecated` 168 against 145; `-S webassembly` 43 against 36. A
  rewrite could save at most those 7–23 ms, and over all content (a `scan-cost` mode, not
  kept) `(?i)webassembly` added 160 ms to the 580 ms decode where Aho-Corasick
  ASCII-insensitive added 300: the regex crate is already the faster matcher. An `-S
  webassembly` that once looked 6x slower (209 against 32 ms) was its output: 19.8 MB,
  lines up to 11 MB, written to a file. Not pursued.
- `codec-probe` (new example: re-encode 615 MiB of blocks sampled across the index, one
  core; [output](../bench/results/E38-ebs/probes/codec-probe.txt)): lz4_flex 232 MiB
  stored, decode 4.37 GB/s, encode 691 MB/s; lz4 fast 232, 4.24, 641; HC 3 200 MiB, 4.81,
  180; **HC 6 195 MiB, 4.97, 125**; HC 9 195 MiB, 5.00, 76; HC 12 194 MiB, 5.09, 27. HC
  finds longer matches, so fewer, longer copies: smaller and faster to decode.
- Change: the writer encodes blocks with `lz4::block::compress` at HC level 6 (the C
  reference library, already the decoder); `lz4_flex` is dropped. The format and reader
  are unchanged, so existing indexes read as before and only new blocks change.
- `run-e38.sh`: both binaries build chromium + llvm-project `--content lz4` at 2 CPUs / 4
  GiB: 304 → 295 s (more encoding, less to write through gp3's caps), 2,864 → 2,580 MiB.
  Parity gate ALL OK on raw and lz4 at 1 and 12 CPUs. `serve-ab.sh` (now taking an index
  per arm), 2 interleaved reps ([results](../bench/results/E38-ebs)): warm **0.970**,
  faster on 11/18 in both reps (`files_mode` 0.915, `anchored` 0.939); `selective_long`
  7.0 → 7.5 ms and `selective_symbol` 8.7 → 9.0 slower in both. Cold **0.964**, faster on
  13/18 in both reps, bytes read down 6–18% on every query but `literal_none` (8.9 both): `llvm_regex` 154 → 121 ms,
  `literal_long` 5.4 → 4.9 s, `date_regex` 2.7 → 2.5 s. Ready 2.27–2.55 → 2.23–2.28 s.
- **Kept.**

### E39: ranked search on every thread

- Why: capped, ranked results are what an agent wants from a broad query, and on
  chromium + llvm-project `--rank --top 20` cost up to 5x the plain search warm at 2 CPUs
  (`literal_long` 1,542 against 310 ms, `date_regex` 1,937 against 399; the plain times
  are from the separate [plain against ranked](../bench/results/E39-rank/plain-vs-rank-warm.txt)
  run, whose plain search this change shares). `perf`: the
  ranked pass ran a thread per repo (two repos, one of them most of the work, so one core
  did nearly all of it), verified every doc of a query with no trigrams (it planned
  without the shape filter: `date_regex` read all 697k docs instead of 56k), and walked
  every newline of every matching file twice (22% of `literal_long`), plus a searcher
  counting line numbers no one read, a lossy UTF-8 copy of each file's head, and six
  `memmem` searchers built per file.
- Change, the ranking untouched:
  - Planning and chunking move into `plan_chunks`, shared with the plain search; ranking
    verifies its chunks on every thread through `walk_part` and merges them in chunk order
    (the old walk order), so corpus statistics, ties and errors come out the same.
  - The shape filter applies: it drops only docs that cannot match, and the doc count
    behind IDF is taken before it.
  - Lines are counted in bulk, and the longest line measured only for files under 50
    lines, the only place `classify` reads it.
  - The ranked pass's searcher keeps no line numbers. Generated-file markers are searched
    in the lowercased head bytes with searchers built once: ASCII bytes and their order
    are the same as in the lossy string.
- Workers stop claiming chunks after the first error or deadline, as the plain search's
  do, and a chunk checks the deadline before its walk prefetches anything. Tests run the
  ranking at 1–8 threads and several chunk sizes, requiring the same output and order, and
  the first failing chunk's error, as the plain search's tests do.
- Exactness: `rank-ab.sh` compares both builds' `--top 200` output byte for byte on all 18
  queries-big queries (identical). `scout-rank-eval dump` of the 79 golden queries with
  both builds gives identical features files, and the eval reproduces R1–R2
  ([golden](../bench/results/E39-rank/golden.txt)). The frozen v1 index is format v2,
  which scout no longer reads, so this ran on the same 136 repos indexed again in v9
  (`scout-golden-v9`, bench/golden/README.md). Parity gate ALL OK.
- `rank-ab.sh`, warm, 2 CPUs, 7 interleaved runs, median
  ([results](../bench/results/E39-rank/top20-warm.txt)): geomean **0.609**, faster on 17
  of 18 (`literal_none` 14 = 14; `selective_long` and `selective_symbol` 0.92): `date_regex`
  1,937 → 514 ms, `crbug_regex` 397 → 173, `literal_long` 1,542 → 732, `literal_common`
  696 → 333, `word` 581 → 310. Ranked now costs 0.95–2.4x the plain search (`plain-vs-rank.sh`), not up to 5x
  ([plain against ranked](../bench/results/E39-rank/plain-vs-rank-warm.txt)).
- **Kept.**

### E40: can a ranked search stop early?

- Why: a capped, ranked result is what an agent reads, so `--rank --top N` could skip files
  that cannot reach the top N. Ranking needs every match today. E39 brought it to 0.95–2.4x
  the plain search, and the plain search's verification is the floor.
- Two ways to stop early, simulated offline before writing any search code. The
  simulations are `scout-rank-eval cap` and `visits`, run on the features that
  `--rank-features` dumps. Both treat a file's path and size as known before decoding
  (the doc table holds both).
  - Cap: score only the first M matches in an order known before decoding, then rank
    those M with their own statistics. Two orders were tried:
    - path-only noise class (`classify` with no content), then path and basename term
      hits, then path;
    - the exact stop's score bound, which weighs each term by its IDF.
  - Exact: visit files in descending order of a path-only upper bound on the score, and
    stop once the N-th distinct copy scores strictly above the next bound. Simulated at
    N = 10, the nDCG cutoff; `--top 20` can only visit more.
  - The bound counts content saturation (below k1 + 1) and a definition line (w_def) at
    their maximum, since neither is known from a path.
  - The path-only noise class has a subset of the real class's marks, so its multiplier
    bounds the real one while multipliers fall with severity. The tool checks that, and
    also checks that every score is within its bound. A wrong `--roots` is an error, not
    a wrong count.
- Cap, on the 79 golden queries
  ([results](../bench/results/E40-cap/golden-cap.txt)). The 22 broad ones (more than 200
  matched files) are the only ones a cap could speed up:
  - Every cap from 20 to 200 loses 0.24–0.34 nDCG@10 on them (0.697 → 0.35–0.46,
    p < 0.003).
  - Cap 500 still loses 0.061 (n.s.), and only a cap of 1,000 matches the full ranking.
  - The two orders give identical numbers: on these queries the IDF weighting does not
    reorder files that tie on noise class and path hits.
  - Cap 20 also drops a known-item target (and all its copies), q073.
  - On queries-big, 10 of the 16 queries with matches match more than 5,000 files, and
    `literal_long` matches 182,037. A cap that keeps quality saves nothing on them.
- The statistics a capped search cannot count are not the problem:
  - df ×2, ×5 or ×10 (what counting candidates instead of matches over-estimates) leaves
    nDCG@10 unchanged.
  - A mean size over every matched file, noise included, costs 0.018 (n.s.).
- Exact, with the true statistics:
  - On the 22 golden broad queries it visits 50% of the matched files in total. Five
    queries stop at 1–20%, and in each of them all 10 top files have a path or basename
    hit. Eight need 83–100%.
  - On chromium + llvm-project ([results](../bench/results/E40-cap/big-visits.txt)), it
    still decodes 65–96% of the matched bytes on 12 of the 16 queries-big queries with
    matches (`literal_long` 79%, `literal_common` 84%, `anchored` 88%). That saves 4–35%
    of their matched bytes.
  - `case_insensitive`, `date_regex` and `smart_case` stop near 0%: all 10 top files
    have a path or basename hit. `rare_regex` stops at 5% of its bytes because 91% of its
    matches are noise by path.
  - These are floors. The real search would also decode non-matching candidates, and in
    bound order rather than shard order, giving up the read order E30–E35 bought on a
    cold disk (not measured here).
- **Rejected.**
  - The cap, in either order, costs ranking quality at every size that would save time.
  - The exact stop saves 4–35% of the decoding on most broad queries. It would need
    candidate-ordered reads and an estimated df and mean size; those estimates measured
    harmless for nDCG@10 but would change the output.
  - What it does capture is the class whose top files all match by path, at near 0%.
    A stop that first verifies only path-matching candidates, then falls back to the full
    ranking when they do not settle the top N, would get that class. It is left for when
    that class matters, and `cap` and `visits` stay in `scout-rank-eval` to measure it.

### E41: trimming the ranked pass's per-file work

- Why: after E39, a profile of ranked `literal_long` (182k matched files, warm, 2 CPUs)
  put LZ4 decode at 28%, which the plain search pays too. The rest was spread across
  per-file work: the chunk callback 5%, xxh3 of the content 4.8%, `classify` 4%, newline
  counting 3.5%, and malloc/free about 4%.
- Change, output unchanged:
  - features moved out of the collected files instead of cloned for scoring;
  - one term-hit buffer per chunk instead of one per candidate;
  - `classify_content`, which stops counting newlines once `classify`'s two line tests
    are settled (50 lines, or a script's average line over 110 bytes; a test covers every
    count and width around both edges);
  - the head lowercased into a stack buffer for the generated-marker search.
  - Variant A also walked path segments without collecting them and compared test
    names ignoring case in place, to avoid three allocations per file.
- Exactness: the 79 golden queries' feature dumps were identical to E39's, and so was the
  `--top 200` output of all 18 queries-big queries.
- Variant A, `rank-ab.sh` warm at 2 CPUs ([9 runs](../bench/results/E41-rank/top20-warm.txt),
  [21 runs](../bench/results/E41-rank/top20-warm-21.txt)): geomean 0.988 and 0.993, with
  signs mixed per query between the two runs. Only `literal_long` was faster in both
  (656 → 651 and 656 → 637 ms).
- `perf stat -r 10` on five broad queries ([results](../bench/results/E41-rank/perf-stat.txt))
  settles it, because wall time on 2 CPUs cannot resolve 1%:
  - Variant A ran **6% more instructions** on `literal_long`. Re-splitting the path for
    each directory test and comparing test names byte by byte cost more than the
    allocations they saved.
  - Variant B (A without the path changes) ran 1–2% fewer instructions on four of the
    five queries, but cycles and task-clock stayed within ±2% of E39's on every query.
- **Rejected.** None of it is measurable: these per-file costs are cheap next to the
  decoding and matching every candidate already pays, and the profile's percentages
  overstated what removing them would save. Ranked search is now within 0.95–2.4x of the
  plain search, so the next milliseconds are in the plain search's own decode and verify.

### E42: fewer, larger cold reads

- Why: E38's cold times on the EBS-like box (2 CPUs / 4 GiB, gp3's 125 MB/s and
  3000 IOPS) were 0.1–10 s a query, against 2–400 ms warm. A probe (`cold-io.sh`: a serve
  per IOPS cap, the index evicted from its cgroup before each query, reads (bios) and
  bytes from the cgroup's `io.stat`) found most of them bound by reads, not bytes
  ([results](../bench/results/E42-coldio/cold-io-e39.txt)):
  - At 3000 IOPS, the nine queries over 1.7 s with 11–18 KiB reads took reads / 3000 to
    within 2% (`alternation`: 29,885 reads, 9.96 s; its 490 MiB alone would take 4.1 s
    at 125 MB/s). `literal_long` took 2.1x that, bound by bytes.
  - Reads averaged 11–25 KiB on every query but `literal_long` (81), `anchored` (38),
    `date_regex` (30), the two selective ones (38, 45) and `literal_none` (5).
  - Lifting the IOPS cap, same bytes, made those nine 2.1–3.3x faster (`files_mode`
    3.25x, `alternation` 2.39x), `literal_ident` and `smart_case` (23–25 KiB) 1.8x and
    1.6x, and `llvm_regex` (140 ms) 1.15x; `literal_long` did not move.
  - The candidates' content is queued with `MADV_WILLNEED` (E28), joining ranges only
    across gaps of up to 4 KiB, so on an lz4 index (E29: a few KiB per small file) most
    candidates were a read of their own.
- Change: `prefetch` joins ranges across gaps of up to `PREFETCH_GAP` = 32 KiB, in a
  pure `coalesce` with a unit test of the edges. One read saved is worth ~41 KB of transfer
  at 125 MB/s ÷ 3000, less what the gap costs a query already bound by bytes.
- Gap sweep, cold at 3000 IOPS, a single pass of the 18 queries per gap, so noisy (in it
  32 KiB's `literal_long` was +17%; in the A/B below, +4%)
  ([4](../bench/results/E42-coldio/cold-io-e39.txt),
  [16](../bench/results/E42-coldio/cold-io-gap16.txt),
  [32](../bench/results/E42-coldio/cold-io-gap32.txt),
  [64](../bench/results/E42-coldio/cold-io-gap64.txt),
  [128 KiB](../bench/results/E42-coldio/cold-io-gap128.txt)). Time against 4 KiB (geomean),
  reads, and the 18 queries' total:

  | gap     | time  | reads | total  |
  | ------- | ----- | ----- | ------ |
  | 4 KiB   | 1     | 1     | 57.0 s |
  | 16 KiB  | 0.818 | 0.684 | 38.8 s |
  | 32 KiB  | 0.776 | 0.524 | 37.7 s |
  | 64 KiB  | 0.873 | 0.406 | 44.7 s |
  | 128 KiB | 1.070 | 0.366 | 54.2 s |

  64 and 128 KiB were dropped on this pass alone: 64 KiB cut reads further but read so
  many more bytes that it was slower than 16 and 32 (and 128 KiB, slower than 4 KiB by
  geomean though not in total).

- `serve-ab.sh`, 2 interleaved reps, warm 3 rounds and cold 1, against e39 on
  `idx-e38-hc` ([32 KiB](../bench/results/E42-coldio/ab-gap32/summary.txt),
  [16 KiB](../bench/results/E42-coldio/ab-gap16/summary.txt)):
  - 32 KiB cold geomean **0.731**, faster in both reps on 13 of 18 queries:
    `alternation` 9,992 → 5,810 ms, `word` 6,569 → 3,479, `literal_common` 5,803 → 3,623,
    `files_mode` 4,633 → 2,136, `crbug_regex` 3,954 → 1,957. It read 25% more bytes over
    the pass, up to 49.5% more on a query (`files_mode`).
  - Slower cold in both reps: `literal_long` 4,899 → 5,111 (+4%) and `anchored` +2%, both
    bound by bytes, and `selective_symbol` 235 → 238 ms (+1%). In one rep, `llvm_regex`
    (+5% median) and `selective_long` (+10%), which take 120–140 ms.
  - Warm geomean 0.978, with no bytes read; time to ready unchanged.
  - 16 KiB: cold 0.776, warm 0.990, `llvm_regex` 1.34 in both reps.
  - 32 against 16 was not run head to head: each ran against its own e39 baseline. 16 KiB
    did better on the largest read-bound queries (`literal_common` 0.538 against 0.624,
    `word` 0.497 against 0.530, `alternation` 0.531 against 0.582) and on `literal_long`
    (1.017 against 1.043). 32 KiB won on geomean (0.731 against 0.776) and on total cold
    time (0.650 against 0.675 of e39's), so it is the one kept.
  - Memory ([peaks](../bench/results/E42-coldio/memory-peaks.txt)): the gaps are read on
    top of the read-ahead window, which counts content bytes only, so a block of n
    candidates queues at most (n − 1) × 32 KiB more. The server's cgroup peak went from
    1,793–1,806 MiB to 1,868–1,910 of the 4 GiB, all reclaimable page cache, with no OOM
    kill.
- Caveat: the harness's `io.max` throttles and counts every bio against the cap. Real
  gp3 merges sequential small reads into one I/O. The ranges joined here are separate
  candidates with gaps between them, which gp3 would count as separate I/Os too. But some
  of the bios behind the base's 11–25 KiB average may have been merged on gp3, so the win
  on EBS itself may be smaller than measured.
- Gates: clippy, tests (132 on Linux), parity ALL OK on all four bench profiles
  ([parity](../bench/results/E42-coldio/parity.txt)).
- **Kept.**

### E43: small blocks compressed too

- Why: after E42 the cold queries were bound by bytes, not reads. `cold-io.sh` on the E42
  build at caps "3000 max", one pass each
  ([output](../bench/results/E43-small/cold-io-e42-caps.txt)): every query took within
  8% of the same time at 3000 IOPS and with no cap but `rare_regex` (1.45),
  `literal_ident` (1.19) and `selective_symbol` (0.85), and the broad ones took about
  their bytes at 125 MB/s (`alternation`: 685 MiB, 5.77 s against 5.75 predicted). Their
  candidates nearly all match (`--stats`, warm;
  [counts](../bench/results/E43-small/candidates.txt)): `literal_long` 184,684
  candidates, 182,037 files matched; `literal_common` 50,646 and 50,302; `files_mode`
  22,456 and 22,339. A tighter filter would not cut those bytes; only storing content
  smaller would.
- `dict-probe` (new example: the blocks of 6 sampled shards, 615 MiB, re-encoded on one
  core; [output](../bench/results/E43-small/dict-probe.txt)): blocks under 4 KiB, which
  the writer kept raw, are 9.6% of the content but 30% of what is stored. Compressing
  every block lz4 shrinks: 195.3 → 166.1 MiB stored (−15%), decode 4.98 → 4.57 GB/s. A 1
  KiB floor stored 168.7 at the same decode speed, so none was kept. The probe times a
  raw block as a copy, which the reader does not pay (it borrows the map), so it
  understates what compressing small blocks costs to decode. Its dictionary arms are not
  an experiment yet: a per-shard dictionary trained on a shard's first 8 MiB stored 146.0
  (148.2 used only for single-block docs, which hold 52% of the content), decode 4.25–4.31.
- Rule set before the run: keep if cold geomean ≤ 0.95, warm geomean ≤ 1.05 and no warm
  query more than 10% slower in both reps, all from the interleaved `serve-ab.sh` reps.
- Change: the writer's `ContentCodec::Lz4` arm compressed every block, keeping it raw only
  when lz4 did not shrink it. The reader needed no change: a block whose two lengths
  differ was already lz4 at any length.
- `run-e43.sh`: both builds index chromium + llvm-project `--content lz4` at 2 CPUs / 4
  GiB ([index](../bench/results/E43-small/index.txt)): 299.6 → 295.0 s, shards 2,580 →
  2,305 MiB (−10.7%, postings included). Parity ALL OK on all four profiles.
  `serve-ab.sh`, 2 interleaved reps ([summary](../bench/results/E43-small/ab-summary.txt),
  [reps](../bench/results/E43-small/ab-4G)):
  - Cold **0.959**: `literal_long` 5,104 → 4,296 ms (0.842), `anchored` 0.837,
    `literal_common` 0.893, `files_mode` 0.914, the rest 0.925–1.006 but `literal_none`
    1.038 and `llvm_regex` 1.224, which is one rep (172 against 119 ms; the other 114
    against 115). Without `llvm_regex` the geomean would be 0.946. Bytes read fell 15–17%
    on `literal_long` and `anchored`, 11% on `literal_common`, 0–7% on the rest: most of
    what those queries read is not in blocks under 4 KiB.
  - Warm 1.018: `literal_long` 280.7 → 310.8 ms (1.107; 1.115 and 1.102 per rep),
    `anchored` 1.052, `crbug_regex` 1.059, `selective_symbol` 9.0 → 9.3 ms.
  - Ready time unchanged (2.24–2.30 s both).
  - `cold-io.sh` at 3000 IOPS, one pass per index, base then small, not interleaved
    ([base](../bench/results/E43-small/cold-io-base.txt),
    [small](../bench/results/E43-small/cold-io-small.txt)): geomean 0.949, `literal_long`
    0.841, `anchored` 0.848, `llvm_regex` 1.006. Not what the rule reads, and the one pass
    is ordered.
- **Rejected**: two of the rule's three clauses fail. Cold 0.959 misses 0.95 (by one noisy
  query), and `literal_long` is more than 10% slower warm in both reps: the warm cost
  lands on the same queries as the cold win.

### E44: a dictionary per shard

- Why: E43's `dict-probe` arm `hc6+small+shard8m+alone` (a shard's dictionary for its
  single-block docs, which hold 52.1% of the raw content) stored 148.2 MiB against 195.3
  (E42) and 166.1 (E43), decode 4.31 against 4.98 GB/s. That arm also compressed the small
  blocks of multi-block docs, as E43 did; what was built here left those raw, so it
  should store somewhat more than the probe.
- Rule set before the run (E43's): keep if cold geomean ≤ 0.95, warm geomean ≤ 1.05 and
  no warm query more than 10% slower in both reps, all from the interleaved
  `serve-ab.sh` reps.
- Change (format v10, reverted): the writer held back a shard's first 8 MiB of content,
  trained a 64 KiB dictionary on its blocks with zstd's trainer (no dictionary under 1 MiB
  or 100 blocks sampled: the trainer refuses a single giant line), and stored it after the
  content. Every doc of one block was lz4 HC 6 against it, at any size, kept when smaller;
  docs of several blocks were encoded as before. The reader decoded such a doc into a
  per-thread scratch holding the shard's dictionary right before the output (lz4's prefix
  path), copied in only when the thread last read another shard, then copied the doc out.
  `lzzzz` and `zstd` became runtime dependencies.
- `run-e44.sh`: the base arm served E43's base index, written by `bin-e42` (the E42
  writer, which E43 left unchanged; which commit built each binary is not recorded by the
  script). That index had already been served by E43's runs, and serving syncs a tiny repo
  into it (serve-ebs.sh), so the base arm started with that repo in it and the new index
  did not. The new build indexed chromium + llvm-project `--content lz4` at 2 CPUs / 4 GiB
  in 306.8 s, against 299.6 s for the base in E43 (separate sessions, not interleaved)
  ([index](../bench/results/E44-dict/index.txt)), 51 shards, 2,580 → 2,165 MiB (−16.1%,
  postings included). Parity ALL OK on all four profiles
  ([parity](../bench/results/E44-dict/parity.txt)). `serve-ab.sh`, 2 interleaved reps
  ([summary](../bench/results/E44-dict/ab-summary.txt),
  [reps](../bench/results/E44-dict/ab-4G)):
  - Cold **0.971**. The broad queries were faster: `anchored` 0.697, `literal_long` 5,091
    → 3,694 ms (0.726), `literal_common` 0.783, `rare_regex` 0.806, `word` 0.857. The
    selective ones were slower in both reps: `llvm_regex` 116 → 242 ms (2.09),
    `literal_none` 1.30, `smart_case` 1.23, `selective_symbol` 1.06, `selective_long`
    1.05, `literal_ident` 1.02. Several of them read more (`literal_none` 9.0 → 12.8 MiB,
    `smart_case` 46.2 → 60.0, `llvm_regex` 28.9 → 34.9), but so did `rare_regex` (110.2
    → 120.2), which was faster. Why they lost was not measured.
  - Warm **1.066**. Every query but `literal_none`, `smart_case` and `date_regex` was
    slower in both reps. Per rep (p50s from warm.json): `literal_long` 1.110 and 1.184,
    `files_mode` 1.105 and 1.174, `anchored` 1.100 and 1.109 (over 10% in rep 1 by
    0.03%, so marginal). Where the time went was not profiled; the change put every
    single-block doc, borrowed from the map before when raw, through a decode and a copy.
  - Ready time unchanged (2.23–2.36 s both).
  - `cold-io.sh` at 3000 IOPS, one pass per index, base then dict, not interleaved
    ([base](../bench/results/E44-dict/cold-io-base.txt),
    [dict](../bench/results/E44-dict/cold-io-dict.txt)): geomean 0.920. Not what the rule
    reads, and the one pass is ordered.
- **Rejected**: all three clauses fail (the per-query one on `literal_long` and
  `files_mode` clearly, `anchored` marginally). A 16% smaller index cut the broad cold
  queries by up to 30% but made almost every warm query slower and the selective cold ones
  slower too. E43 and E44 both traded warm time for cold bytes on this box and both lost
  the trade under this rule.

### E45–E47: reading the index in before the queries need it

- Why: after E42–E44, a cold broad query waits on its bytes at gp3's 125 MB/s, and
  storing them smaller cost warm time. A server that has just started (a deploy, a reboot)
  has nothing cached, so its first queries are cold; reading the whole index in the
  background (2.49 GiB of shards, ~21 s at 125 MB/s) would make the ones after that warm. The cold
  protocols (`serve-ab.sh`'s cold rounds, `cold-io.sh`) evict the index before every
  query, so they cannot see this: `warmup-ebs.sh` (new) drops the page cache, starts
  `serve`, and runs the 18 queries once (the "pass") right at ready, or 45 s after it,
  then qbench's warm rounds, the index's residency and the cgroup's peak memory.
  `warmup-ab.sh` interleaves warm-up off (`--warm-mb 0`) and on (3072 MiB, more than the
  index) of one binary, 2 reps per wait, on E43's base index (both arms the same copy).
  Search code is unchanged, so the parity gate was not rerun.
- Rule set before E45, kept for E46 and E47: keep if the pass at ready has geomean on/off
  ≤ 1.00 in both reps (the warm-up must not slow the queries it shares the disk with),
  the pass 45 s after ready ≤ 0.50, steady warm geomean ≤ 1.02, ready time within 5%,
  no OOM kill, and ≥ 95% of the index resident after the 45 s.
- Change: after each sync cycle `serve` starts a walk over the served generation's shard
  files (once per generation; it stops when another is served), reading them with plain
  `read`s into a scratch buffer: the page cache they fill is the one the map faults from,
  so it needs no `unsafe` and no format knowledge. Each shard is read from its end
  backward; the order does not matter, since the whole index is read. `/status` reports
  its progress. It starts after the first cycle, not at open, so it does not compete with
  the open and the cycle before ready.
- The pass's "MiB read" columns count the whole cgroup's reads, the walk's included.
- E45, 4 MiB reads, waiting only while a search runs
  ([summary](../bench/results/E45-warmup/summary.txt)):
  - At ready **1.141** (1.143 and 1.132 per rep): `literal_none` 3.15, `llvm_regex`
    1.86, `selective_symbol` 1.54. A search that started during a read waited for it
    (~32 ms at 125 MB/s).
  - 45 s after ready **0.264**: the pass took 2.4 s against 12.0 s.
  - Steady warm 1.024 after the pass at ready (the walk was still reading during qbench's
    rounds: 2,013–2,040 MiB of 2,579 resident at the end), 0.999 after the 45 s one.
  - Ready 2,211–2,509 ms off, 2,231–2,286 on; peak 1,878–1,929 MiB off, 2,472 on at ready and 3,055–3,109 after the 45 s; no
    OOM kill; the whole index resident after the 45 s.
  - **Rejected**: the at-ready and steady clauses fail.
- E46, 256 KiB reads (~2 ms each), otherwise E45
  ([summary](../bench/results/E46-warmup-256k/summary.txt)):
  - At ready **0.986** pooled, but 0.904 and **1.105** per rep; `literal_none` 18 → 62 ms
    (3.38) in both, `smart_case` 1.19, `word` 1.11.
  - 45 s after ready 0.286 (2.5 s against 12.0); steady warm 0.999 and 0.995.
  - **Rejected**: the at-ready clause fails in one rep. The walk read in the gaps between
    back-to-back searches, and every read shares the volume's throughput cap (the
    cgroup's io.max here, the volume's on EBS), so the next search waited for it.
- E47, E46 reading only after no search has run for 100 ms
  ([summary](../bench/results/E47-warmup-idle/summary.txt),
  [reps](../bench/results/E47-warmup-idle)):
  - At ready **0.958** (0.957 and 0.965 per rep): the walk had read 7–11 MiB before the
    pass and stayed out of it (1,434–1,439 MiB resident at the end, against 1,400 off),
    so this measures the server without the walk. Single queries still spread:
    `selective_symbol` 0.565 (faster in both reps) and `rare_regex` 1.261 (slower in
    both). The walk read nothing during the pass, so the spread is not its reads. The 18
    queries took 12.0 s either way.
  - 45 s after ready **0.261** (0.272 and 0.250 per rep): the pass took 2.39 and 2.29 s
    against 11.99 and 12.15. Every query was faster in both reps: `literal_common` 3,214
    → 210 ms, `literal_long` 2,257 → 299, `selective_long` 98 → 16.
  - Steady warm 1.007 (after the pass at ready) and 1.003.
  - Ready 2,223–2,262 ms off, 2,223–2,299 on. Peak memory 1,896–1,936 MiB off, 1,930–1,963 on at ready and 3,102–3,118
    MiB on after the 45 s (of 4,096); no OOM kill; 2,579 of 2,579 MiB resident after the 45 s.
  - **Kept**: every clause passes. The cost of the idle gate: a client that sends a query
    at least every 100 ms keeps the walk from reading at all. The cache then holds only
    what that client's queries read.
- Review changes, before the confirmation run: an index over the budget is not read at
  all (reading part of it would evict what queries read, and each new generation would
  read it from disk again); sync cycles count as activity like searches; a failed or
  stopped walk reports why in `/status` (`ended`).
- Default: `--warm-mb` is three quarters of usable RAM (3 GiB on the 4 GiB box).
  `serve-ebs.sh`, `cold-io.sh`, `serve-coldab.sh` and `run-e21.sh` pass `--warm-mb 0`,
  so their warm and cold rounds measure what they did before; binaries before E47 refuse
  the flag.
- E47b, the confirmation run of the revised code
  ([summary](../bench/results/E47b-warmup-confirm/ab/summary.txt),
  [reps](../bench/results/E47b-warmup-confirm/ab)), `run-e47b.sh`: the same A/B with a
  third scenario, queries 300 ms apart from ready (a client that is not a burst), whose
  clause was set before the run: pass geomean ≤ 1.00 in both reps, the rest unchanged.
  "The index" below is its 2,549 MiB of shards (`/status`'s `bytes_total`); residency
  counts the whole index directory, 2,579 MiB.
  - At ready, back to back: **0.990** (0.994 and 0.981); steady warm 1.012.
  - 45 s after ready: **0.283** (0.276 and 0.284), the pass 12.0 → 2.4–2.5 s; steady
    warm 0.985; the whole index resident.
  - 300 ms apart from ready: **0.774** (0.784 and 0.755). 15 of 18 queries were faster
    in both reps; `literal_none` 1.371 and `selective_symbol` 1.492 were slower in both,
    `llvm_regex` 1.276 in one. Steady warm **1.037**, against ≤ 1.02: the walk had read
    1,514 MiB when qbench's rounds began (`status-after-pass`), and the smallest queries
    were slower in both reps (`literal_none` 1.49 → 2.12 ms median, `selective_*` 1.05).
  - Ready within 1% in every scenario; no OOM kill; peak 3,048–3,093 MiB after 45 s.
  - Budget ([2 GiB](../bench/results/E47b-warmup-confirm/budget-2G.txt),
    [4 GiB](../bench/results/E47b-warmup-confirm/budget-4G.txt)), without the flag:
    under 2 GiB the index is over the 1,536 MiB budget and nothing is read (`ended`:
    "not read"); under 4 GiB all of it is read.
  - **Fails one clause**: steady warm in the spaced scenario.
- Diagnosis ([diag](../bench/results/E47b-warmup-confirm/diag-0-300)): the 0:300
  scenario again, 2 reps, with `/status` captured after qbench's rounds too
  (`status-after-warm.json`, added to `warmup-ebs.sh` for this). Steady warm **1.006**
  this time, pass 0.798 (0.825 and 0.762). The walk read 50–107 MiB during qbench's
  ~9 s, in the gaps before and between its runs, far less than the ~1 GiB it could at
  125 MB/s. `literal_none` was still 1.106. Across the runs, `literal_none`'s steady
  ratio follows whether the walk is alive, not how much it reads: 1.06 at 0:0, where the
  walk had read 256 KiB; 1.106 and 1.425 at 0:300; 0.823 at 45:0, where the walk had
  finished. Suspect: the walk woke every 5 ms (`PAUSE`) to look at the clock while
  searches ran, 200 wake-ups a second on 2 CPUs. The clause sits near noise (1.006 and
  1.037 in two runs of the same scenario), so neither run settles it alone.
- E47 was not committed on its own; E45–E49 are committed together, after E49.

### E48: waiting for idle without polling

- Rule set before the run: every clause of E47b's rule in all three scenarios (pass at
  ready ≤ 1.00 in both reps for 0:0 and 0:300, ≤ 0.50 at 45:0, steady warm ≤ 1.02 in
  each, ready within 5%, no OOM kill, ≥ 95% resident after 45 s), and the budget checks
  as in E47b. `warmupab.py` now checks each of these itself (E47b's failure printed no
  FAIL before).
- Change, E47b plus:
  - The walk sleeps on a condvar until no search or cycle runs, then exactly out the
    rest of `IDLE`; a search's start and end take an uncontended mutex. It wakes at
    least every second (`RECHECK`) to see whether its generation is still served.
  - Shards are read forward from the start, each opened only when its turn comes (E47
    held every shard open for the whole walk, which can last indefinitely under steady
    load).
  - A new generation's walk skips the shards the last finished walk read (`/status`
    `bytes_skipped`): a sync that changes one repo no longer copies the whole index
    through the CPUs again.
  - The default budget is usable memory less two generations of `--table-mb`, the same
    3 GiB on the 4 GiB box by default.
- Results ([summary](../bench/results/E48-warmup-condvar/ab/summary.txt),
  [reps](../bench/results/E48-warmup-condvar/ab)), `run-e47b.sh` with the E48 binary:
  - At ready, back to back: **1.020** (1.066 and 0.974): **fails**. The walk had read
    nothing before the pass in either rep, and 15–58 MiB by the end of qbench's rounds.
  - 45 s after ready: **0.271** (0.258 and 0.283); the whole index resident.
  - 300 ms apart from ready: **0.839** (0.823 and 0.858).
  - Steady warm 1.029 (0:0, **fails**), 1.027 (45:0, **fails**), 0.979 (0:300).
  - Ready within 5%, no OOM kill; budget as E47b (2 GiB: not read; 4 GiB: all of it,
    20,285 reads against E47b's 11,526: see below).
  - **Rejected** under its rule. But the failures sit where the walk could not have
    caused them: at 45:0 the walk had finished and its thread had exited, and steady warm
    was still 1.027, with `literal_none` 1.359. The pass at ready, where the walk read
    nothing during the pass in E47, E47b and E48 alike, came out 0.958, 0.990 and 1.020:
    a spread of 6% with no walk reads at all. The rule's ≤ 1.00 and ≤ 1.02 lie inside the
    harness's own noise at 2 reps and 3 qbench rounds, so it fails good and bad changes
    alike, and E47's and E47b's passes of those clauses were partly luck.
  - One real cost found: reading forward, the kernel's readahead split each 256 KiB read
    into two 128 KiB reads (20,285 reads for the index against 11,526), twice the share of
    the volume's IOPS. E49 reads backward again.

### E49: the warm-up against the harness's measured noise

- Rule set before the runs (`warmuprule.py`, `run-e49.sh`): first an A/A run, both arms
  `--warm-mb 0`, the same three scenarios and 4 reps, measures the noise: for each
  scenario, the largest per-rep deviation from 1 of the pass geomean (scenarios at ready)
  and of the steady warm geomean. The A/B's pooled ratio (median over 4 reps per query,
  then geomean) must be at most 1 + that deviation, for each. Unchanged from E47b: pass
  45 s after ready ≤ 0.50, ready within 5%, no OOM kill, ≥ 95% resident after 45 s, and
  the budget checks. qbench's warm rounds go from 3 to 10 in `warmup-ebs.sh`, so one
  ~1.4 ms query swings the steady geomean less.
- Change: E48 with shards read backward again (each chunk one 256 KiB read).
- Results ([rule](../bench/results/E49-warmup-noise/rule.txt),
  [A/A](../bench/results/E49-warmup-noise/aa/summary.txt),
  [A/B](../bench/results/E49-warmup-noise/ab/summary.txt)):

  | scenario          | clause | A/A per rep                | noise | A/B pooled | A/B per rep                |
  | ----------------- | ------ | -------------------------- | ----- | ---------- | -------------------------- |
  | 0:0, at ready     | pass   | 0.971, 1.067, 0.968, 1.012 | 0.067 | **1.013**  | 0.998, 1.012, 0.993, 1.028 |
  | 0:0               | steady | 0.961, 0.992, 0.983, 0.973 | 0.039 | **0.989**  | 0.967, 0.989, 0.999, 1.016 |
  | 45:0              | steady | 0.988, 1.000, 1.025, 1.019 | 0.025 | **1.001**  | 1.009, 0.988, 1.005, 1.007 |
  | 0:300, 300 ms gap | pass   | 1.041, 1.015, 0.969, 0.967 | 0.041 | **0.738**  | 0.689, 0.720, 0.763, 0.757 |
  | 0:300             | steady | 0.994, 1.015, 1.025, 0.987 | 0.025 | **0.992**  | 0.989, 0.972, 1.000, 1.014 |
  - 45 s after ready: **0.253** (0.247–0.274 per rep); the 18-query pass took
    2.30–2.43 s against 11.98–12.04 s, every query faster in all 4 reps
    (`literal_common` 3,216 → 205 ms, `literal_ident` 531 → 49, `selective_long`
    105 → 15); the whole index resident; peak 3,110–3,130 MiB of 4,096.
  - 300 ms apart from ready: 16 of 18 queries faster in all 4 reps; `llvm_regex`
    (60 → 75 ms, 0/4) and `selective_symbol` (54 → 73 ms, 2/4) were slower: a
    small query that lands behind one of the walk's reads waits for it.
  - Ready within 5% and no OOM kill in every scenario; budget as E47b (2 GiB: not
    read; 4 GiB: all of it, in 11,529 reads, against E48's 20,285 forward).
  - The A/A, both arms off, puts a rep of the pass at ready at 1.067 and steady warm
    reps at 1.025 (45:0 and 0:300): the fixed clauses E45–E48 were held to (≤ 1.00 in
    every rep, ≤ 1.02) sat inside the noise.
  - **Kept**: every clause of the rule passes.
  - The rule is lenient, and the review after the run said so: it holds a pooled ratio
    (median of 4 reps) to the A/A's single worst rep, and a rep faster than 1 counts as
    noise too, though a pooled ratio varies less than one rep. Pooled against pooled, the
    A/A against the A/B: pass at ready 0.996 against **1.013**, steady 0.979 against
    0.989 (0:0), 1.009 against 1.001 (45:0), 1.004 against 0.992 (0:300). So queries in a
    burst at ready may be ~1–2% slower with the warm-up on, a difference these 4 reps
    cannot tell from noise; the walk had read nothing before those passes and ~100 MiB by
    the end of qbench's rounds. The rule was in `run-e49.sh` and `warmuprule.py` before
    the runs; `warmupab.py` was changed after them, to drop the fixed clauses from its
    checks, which no verdict here reads.

- Cost, measured: a client that searches at least every 100 ms holds the walk off
  entirely. One that searches every few hundred ms got its 18 queries 0.738x as long by
  geomean, but a small query that landed behind a 256 KiB read waited 15–19 ms (medians),
  far more than the read's ~2 ms of transfer; the throttle dispatching by time slices is
  the likely cause ([research](research/background-warmup.md)), not measured.
- Not measured: the target box's own disk. Every run here emulates gp3's 125 MB/s and
  3000 IOPS, but a t-class 2 vCPU instance's EBS baseline is 43 MB/s and 2,000 IOPS
  (research, from AWS's tables), where the walk would take ~60 s, not ~21, and every
  cold read is ~3x slower. The next measurement runs the harness at that profile.
- After the run, from the review: `/status` says done only after the finished walk's
  shards are recorded for the next generation to skip; an over-budget walk reports no
  skipped bytes; a shard missing because another generation is served ends the walk as
  stopped, not failed; a default budget of 0 (two generations of `--table-mb` filling
  memory) is an error instead of a warm-up silently off. None of it touches the measured
  path, so it was not rerun.
- Gates: clippy, tests (140 on Windows, 142 on Linux), including the walker's budget, idle wait, stop,
  skipped shards and a missing file, the condvar wait itself, and `/status` reporting a
  finished warm-up of two generations (the second skipping the unchanged repo's shards)
  and one turned off.

### E50: joining prefetch gaps only while reads bind

- Why: the cold-path research ([cold-path-ideas.md](research/cold-path-ideas.md), idea 2)
  fit every row of E42's gap sweep to t ≈ max(MiB ÷ 119, reads ÷ 3000): joining two ranges
  across a gap saves a read and adds the gap's bytes, which pays only while the reads bind.
  E42's fixed 32 KiB joined gaps for queries already bound by bytes (`alternation` read
  490 → 685 MiB). Modelled: cold geomean ~0.965.
- Rule set before the run (`run-e50.sh`): cold geomean pooled ≤ 0.98 and below 1.00 in
  both reps; warm pooled ≤ 1.03; no query over 10% slower cold in both reps.
- Change: `coalesce` joins adjacent ranges, then the gaps under `BYTES_PER_READ`
  (125 MB/s ÷ 3000 = 41.7 KB) shortest first, stopping once the block's runs × that ≤
  its bytes. Format unchanged; both arms served E43's base index.
- Results ([summary](../bench/results/E50-prefetch-balance/ab-4G/summary.txt),
  [cold-io base](../bench/results/E50-prefetch-balance/cold-io-base.txt),
  [cold-io E50](../bench/results/E50-prefetch-balance/cold-io-bal.txt)):
  - Cold **0.959** (0.952 and 0.968 per rep): `selective_symbol` 0.693, `literal_common`
    0.860 (421 → 359 MiB), `alternation` 0.877 (694 → 608 MiB), `word` 0.882, `anchored`
    0.911. In cold-io the big queries' reads came out at 41–44 KiB each, the balance point.
  - Slower in both reps: `llvm_regex` 145 → 243 ms (1.44 and 2.03 per rep), `smart_case`
    384 → 462 (1.21 and 1.20), `literal_ident` 1.016.
  - Warm 0.999.
  - **Rejected**: two queries over 10% slower in both reps.
- Why the small ones lost: the balance is kept per prefetch block, but the caps bind per
  query. `llvm_regex` as a whole is bound by reads (cold-io: 585 reads ÷ 3000 = 195 ms
  against 20 MiB ÷ 119 MB/s = 168 ms, measured 185). Its byte-heavy blocks stopped
  joining, so the query made 694 reads (231 ms by the model; measured 246). Joining by
  the query's own balance needs the query's running reads and bytes inside `prefetch`, a
  change to the shard port; not tried yet.
- Probe for the research's idea 3 (store identical files once per shard): blobs that occur
  more than once are 140 of 4,174 MiB in chromium (3.4%) and 17 of 2,299 in llvm-project
  (0.7%), from `git ls-tree -r -l HEAD`, an upper bound on what dedup could save. Not
  worth a format change.

### E51: Content-Length response bodies

- Why: tiny_http sends a body of 32 KiB or more with chunked transfer encoding in 8 KiB
  frames, so the client parses a broad query's output frame by frame. `serve` builds every
  body in memory first, so its length is known before the first byte goes out.
- Rule set before the run (`run-e51.sh`): warm geomean pooled ≤ 0.99 and below 1.00 in
  both reps; cold pooled ≤ 1.02; no query over 10% slower warm in both reps.
- Change: `handle()` responds with `with_chunked_threshold(usize::MAX)`. Bodies are
  unchanged (serveab.py checks each query's output hash across arms).
- Results ([summary](../bench/results/E51-content-length/ab-4G/summary.txt)), 2 CPUs /
  4 GiB, E43's base index, 2 reps:
  - Warm **0.979** (0.977 and 0.982 per rep): `alternation` 0.925, `literal_none` 0.947,
    `smart_case` 0.947, `count_mode` 0.948; slowest `literal_long` 1.029 and
    `literal_common` 1.018, each faster in one rep of two.
  - Cold 0.990 (0.987 and 0.992 per rep): reads are unchanged, so the disk sets it.
  - Time to ready on an evicted index: 2,224 and 2,286 ms base, 2,253 and 2,271 ms E51.
  - The rule's clauses passed, and the change was committed as kept.
- The review after the run doubted the gain:
  - It does not grow with the body: the four largest bodies (22–62 MB) came out 0.925,
    0.996, 1.018 and 1.029, while bodies of 0.1–0.7 MB "gained" 4–5%.
  - `literal_none` and `selective_long` return nothing, so their bytes on the wire are the
    same in both arms, yet they moved 0.947 and 0.986.
  - Not every query was within 3%: cold `selective_long` was 1.034 and slower in both
    reps; single reps went to 1.067 warm (`literal_long`) and 1.061 cold (`date_regex`).
  - Cold `rare_regex` 0.871, steady within each arm, is more than a 236 KB body can
    explain: a difference between the two processes or builds, not the change.
  - The client is Python's urllib (qbench.py), whose chunked decoder is slow, so even a
    real gain would say more about the harness than about `serve`.
- A/A, set after the review with its rule in `run-e51aa.sh` before the run
  ([summary](../bench/results/E51-content-length/aa-4G/summary.txt)): E49's base binary
  in both arms, same settings. Warm 0.987 pooled, **0.954 and 1.022** per rep; cold 0.990
  (0.989 and 0.991). The rule kept E51 only if its warm 0.979 was below 1 − 0.046 = 0.954
  and below the A/A's pooled 0.987. It is not.
- **Reverted**: a 2% warm gain at 2 reps cannot be told from noise here, and the
  mechanism did not show in the per-query data. A 2-rep A/B can only show a warm change
  larger than ~5%.

### E52: E50's balance, only for queries bound by bytes

- Why: E50's per-block balance made the cold queries bound by bytes 12–14% faster and
  two small ones 20–68% slower (`smart_case`, `llvm_regex`). In E50's cold-io data, they lost reads in a
  regime the throughput model does not describe: `llvm_regex` paid ~0.9 ms per extra read,
  against 0.33 at 3000 IOPS. Choosing per query, not per block, could keep the wins.
- Probe first (a throwaway build counting `prefetch`'s coalesced reads and bytes): a
  content-only estimate undercounts the cold reads cold-io measured, 1.1–1.3x on the big
  queries, up to 3.1x where the kernel splits long ranges (`literal_long`), and 1.5–1.7x
  on E50's three losers. A plain reads-vs-bytes switch would have put all three on the
  balance side. With bytes required to cost 1.5x the reads, they fall on E42's side, and
  E50's per-query ratios predicted cold ~0.945.
- The 1.5 was chosen with those 18 queries in view, so 7 more, in
  [queries-big-heldout.tsv](../bench/queries-big-heldout.tsv), were picked and classified
  by the probe before the code: `small_vector`, `license_apache` and `const_auto_ref`
  (1.60, the closest) on the balance side; `llvm_debug` (17 MiB, 1.41),
  `web_contents_obs`, `singleton_ci` and `trace_event` on E42's.
- Change: a new port method, `Shard::read_cost`, estimates what `prefetch` of some wants
  would read (E42's joins). `plan_chunks` sums it over every candidate of every repo, as
  verification will read them; one disk serves them all. Every repo's plan carries the
  sum, which `prefetch` now takes: the native shard joins by `balance` when
  `2 × bytes ≥ 3 × reads × 41.7 KB`, else by `coalesce` at 32 KiB. A plan is made only
  once the sum is known (`RepoDraft`), so no read-ahead sees a partial one. `--stats`
  prints the sum (`cost_reads`, `cost_bytes`).
- Rule set before the run (`run-e52.sh`): cold pooled ≤ 0.97 and below 1.00 in both reps
  (E51's cold A/A: 0.989 and 0.991); warm pooled ≤ 1.03; no query, suite or held out, over
  10% slower cold in both reps.
- Results ([summary](../bench/results/E52-prefetch-switch/ab-4G/summary.txt), cold-io for
  [base](../bench/results/E52-prefetch-switch/cold-io-base.txt) and
  [E52](../bench/results/E52-prefetch-switch/cold-io-q.txt), each query's
  [estimate](../bench/results/E52-prefetch-switch/estimate.txt)), 2 CPUs / 4 GiB, 25
  queries:
  - Cold **0.949** (0.947 and 0.950 per rep). Balance side: `selective_symbol` 0.748,
    `alternation` 0.877, `word` 0.882, `literal_common` 0.886 (421 → 359 MiB),
    `anchored` 0.922, `literal_long` 0.952. E42's side, as before within ±1%:
    `llvm_regex` 0.993, `smart_case` 0.992, `count_mode` 0.998, `rare_regex` 0.998.
  - Held out: balance side `small_vector` **0.775**, `const_auto_ref` 0.901,
    `license_apache` 0.959; E42's side `llvm_debug` 1.005, `web_contents_obs` 1.000,
    `trace_event` 0.997, `singleton_ci` 0.954. The margin held on queries it was not
    fitted to.
  - Slowest cold: `date_regex` 1.047, slower in both reps. The build estimates per shard,
    not per read-ahead block as the probe did, which moves it from 1.45 to 1.58, onto the
    balance side; E50 had it at 0.958 there. Within the rule's 10%.
  - Warm **1.022** (1.029 and 1.011 per rep), passing its clause but with 10 of 25
    queries slower in both reps. Measured directly, the search alone with the index
    cached ([plan-cost](../bench/results/E52-prefetch-switch/plan-cost.txt)): +4.4 ms on
    `#include <vector>` (22k candidates, +7%), +5 ms on `std::unique_ptr` (51k, +4%),
    +2–3 ms on `TODO\(crbug…` (16k), ~+0.8 ms on a 1.6k-candidate query. A profile puts
    the cost in `meta` (its path's UTF-8 check), `stored` and `want`, per candidate.
  - **Kept**: every clause passes. The warm cost is what E53 goes after.
- Tried after the run and rejected: estimating from a sample of each shard's candidates
  (four runs of 32 neighbours). `literal_common`'s sampled ratio came out 1.39 against
  3.30 whole: its bytes sit in a few large files that runs of neighbours miss, so it
  would have lost its 0.886.
- Not tuned for: the t-class profile (43 MB/s, 2000 IOPS), where a read is worth ~21.5 KB
  and both constants move together.

### E53: a size lookup without the path

- Why: E52's estimate made broad warm searches 4–7% slower, and a profile of
  `std::unique_ptr` put most of a candidate's planning cost in `meta`: its path's UTF-8
  check (`from_utf8`, 2.4% of the whole search) and the lookup (2.1%). Planning
  (`chunk_repo`, `want`, twice with E52's estimate) and read-ahead (`queue_block`) call it
  for the size alone.
- Change: `Shard::size(doc)`, the doc row's length without the path, used in those three
  places. `visit_docs` still reads the path to print it.
- Harness: [search-ms.sh](../bench/harness/search-ms.sh), the search alone
  (`--stats`' `search_ms`, `--freshness off`, 2 CPUs, index cached), median of 9, two
  interleaved passes, over [queries-search-ms.tsv](../bench/queries-search-ms.tsv). A
  2-rep `serve-ab.sh` cannot see a change of a few percent warm (E51's A/A: ±5% a rep).
- The first run failed a rule set before this harness's noise was known (E53 faster on
  every query in both passes): `alternation` 1.035 in one pass, geomeans 0.975 and 0.979
  ([first run](../bench/results/E53-size-lookup/search-ms-first.txt)). An A/A of one
  binary ([aa](../bench/results/E53-size-lookup/search-ms-aa.txt)) then moved a query up
  to 1.044 in a pass and a pass's geomean up to 1.011. The rule for the second run, set
  after the A/A ([run-e53.sh](../bench/harness/run-e53.sh)): geomean ≤ 0.98 in both
  passes; no query over 1.045 in both; cold pooled ≤ 1.02 and no query over 10% slower
  cold in both reps (`serve-ab.sh`, 2 reps, the big suite).
- Results ([search-ms](../bench/results/E53-size-lookup/search-ms.txt),
  [serve-ab](../bench/results/E53-size-lookup/ab-4G/summary.txt)):
  - Warm, search alone: **0.965 and 0.957**; `crbug_regex` 0.952 and 0.944, `files_mode`
    0.942 and 0.937, `dcheck` 0.965 and 0.931; worst `smart_case` 1.007. `serve-ab.sh`
    warm 0.980.
  - Cold **1.068**: `llvm_regex` 1.382, `selective_long` 1.371, `selective_symbol`
    1.250, `smart_case` 1.135, `literal_none` 1.133, all small; the broad queries within
    ±1%.
  - **Rejected** on cold.
- Why cold lost, a hypothesis not yet measured: the paths section keeps the kernel's
  128 KiB read-around. Before, `meta` in planning faulted path pages in while the disk was
  idle; with `size`, the first touch of a candidate's path is in `visit_docs`, during
  verification, where each fault waits behind the read-ahead's queued content on the
  throttled volume (E49 saw small reads wait 15–19 ms there). Small queries are bound by
  that latency. E54 tests it: E53 with the read-ahead also queuing each block's paths.

### E54: queuing the paths with the content

- Why: to test E53's hypothesis that its cold loss came from path pages first faulted
  during verification, behind the read-ahead. `prefetch` also queued the paths of each
  block's docs (`MADV_WILLNEED`, joined across gaps up to 32 KiB), after the content.
- Rule set before the run (`run-e54.sh`), against E52: search-ms geomean ≤ 0.98 in both
  passes and no query over 1.045 in both; cold pooled ≤ 1.00 and no query over 10% slower
  cold in both reps.
- Results ([search-ms](../bench/results/E54-prefetch-paths/search-ms.txt),
  [serve-ab](../bench/results/E54-prefetch-paths/ab-4G/summary.txt), cold-io for
  [base](../bench/results/E54-prefetch-paths/cold-io-base.txt) and
  [E54](../bench/results/E54-prefetch-paths/cold-io-paths.txt)):
  - Warm, search alone: 0.979 and 0.966; `smart_case` 1.066 and `date_regex` 1.083 in
    one pass each.
  - Cold **1.085**, every query but two slower in both reps: `llvm_regex` 1.504,
    `selective_symbol` 1.377, `smart_case` 1.233, the broad ones 1.03–1.08.
  - cold-io: 4–21% more reads per query (`llvm_regex` 586 → 655, `smart_case` 1,180 →
    1,389, `date_regex` 4,951 → 5,552).
  - **Rejected.**
- What it says about E53: queuing the paths as their own small reads costs more reads
  than it saves waits. In E52 the paths were faulted in while planning, one 128 KiB
  read-around covering the paths of many candidates, while the disk was otherwise idle;
  that, not reading them early as such, is what the small queries depended on. E55 keeps
  `meta` where it does that (`chunk_repo`, during planning) and uses `size` only after it.

### E55: the size lookup after planning only

- Why: E54 showed the small cold queries depend on `chunk_repo`'s `meta` faulting the
  paths in while planning. So keep it there, with a comment saying why, and use E53's
  `size` only in `want` (twice a candidate with E52's estimate) and `queue_block`.
- Rule set before the run (`run-e55.sh`), against E52: search-ms geomean ≤ 0.98 in both
  passes and no query over 1.045 in both; cold pooled ≤ 1.01 (E51's cold A/A) and no query
  over 10% slower cold in both reps.
- Results ([search-ms](../bench/results/E55-size-after-planning/search-ms.txt),
  [serve-ab](../bench/results/E55-size-after-planning/ab-4G/summary.txt), cold-io for
  [base](../bench/results/E55-size-after-planning/cold-io-base.txt) and
  [E55](../bench/results/E55-size-after-planning/cold-io-size.txt)):
  - Warm, search alone: **0.986** and 0.979; `crbug_regex` 0.970 and 0.964, `files_mode`
    0.976 and 0.940; worst `smart_case` 1.013 in one pass.
  - Cold 0.978, no query slower in both reps; cold-io reads identical to E52's (its
    paths are read where they were).
  - **Rejected**: the first pass misses ≤ 0.98 by 0.006. The rule is held as set; the
    ~2% warm it shows is about twice the A/A's pass noise, too small to keep a port method
    for on this evidence.
- What is left of E52's warm cost (4–7% on broad searches) is the estimate's per-candidate
  `want` and `stored`, run serially within each repo. Not tried: folding the estimate into
  `chunk_repo`'s own pass over the candidates.

### Probe: what `anchored`'s anchors cost

- Question: does `^` or `$` in `anchored` (`^#include "base/\w+\.h"$`) push the regex
  engine off its literal fast path, so the query pays for the anchors?
- Probe: `scout search` on E43's base index, page cache warm, all CPUs, 5 runs each:
  - both anchors: 1,174–1,224 ms (80,240 lines)
  - no `^`: 1,113–1,153 ms
  - no `$`: 1,170–1,205 ms
  - neither: 1,133–1,237 ms (80,323 lines)
- No: the anchors cost at most ~5%, within run-to-run spread. No experiment follows.

## Ranking experiments

Golden set: `bench/golden/`, kept out of the public repository because its labels are
paths in the author's own projects ([ranking.md](ranking.md), "Evaluation", describes
it). Harness: `scout-rank-eval`. Design: [ranking.md](ranking.md). The set has 79 queries
and 1,556 labels, every one assigned by hand, plus known-item targets graded 3
automatically. Labels refer to paths in a frozen golden index, so they never drift with
the live corpus. Metrics
are at a cutoff of 10 results. p is from a paired permutation test on per-query values
(10,000 permutations, so 0.0001 is the floor). Raw output comes from one
`scout-rank-eval eval` run with three `--compare` pairs, and one `crossval` run.

| #   | Change                                                                                          | nDCG@10               | noise@10              | Verdict                                                                                                                                                                                                                                         |
| --- | ----------------------------------------------------------------------------------------------- | --------------------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R0  | Unranked (path order), noise-only, BM25-only baselines                                          | 0.451 / 0.553 / 0.630 | 0.339 / 0.233 / 0.223 | baselines                                                                                                                                                                                                                                       |
| R1  | The scorer (BM25 per term, plus definition, basename and path bonuses, times noise multipliers) | **0.795**             | 0.129                 | **kept** (vs unranked +0.344, 69 better / 8 worse, p ≤ 0.0001; vs BM25-only +0.165, 55 / 11, p ≤ 0.0001; noise@10 lower than both, p ≤ 0.0001)                                                                                                  |
| R2  | Collapse identical copies (same content hash) behind all first copies                           | 0.795                 | **0.090**             | **kept** (noise@10 −0.039, 15 better / 0 worse, p ≤ 0.0001; nDCG@10 0.000, 9 / 5, p = 0.98)                                                                                                                                                     |
| R3  | Coordinate-ascent tuning of the 15 free weights (`w_word` fixes the scale)                      | 0.774 held out        | —                     | **rejected** (leave-one-project-out, both with copies collapsed: defaults 0.795 → tuned 0.774, p = 0.013. Tuned weights are significantly worse on projects they did not see)                                                                   |
| R4  | Latency of `--rank`                                                                             | —                     | —                     | **kept as opt-in** (interleaved, same binary: `--top 20` geomean 1.05x plain, private memory on broad queries down, e.g. 239 → 16 MiB; printing every file 1.26x, and 1.1–2.4x the private memory on broad queries, 239 → 574 MiB at the worst) |

### Notes

- By class, for what ships (full+dedup): known-item 0.917, alternation 0.832, phrase
  0.824, glob-scoped 0.770, structural 0.725, case-insensitive 0.696, identifier 0.691.
  Bare identifiers are the weakest class: many files use the name, and the definition
  bonus only helps when the defining file is among them.
- Known miss (q075, `RetailerCategoryMapping`): the definition and basename bonuses match
  substrings, so `RetailerCategoryMappingsController.cs` and `class
  RetailerCategoryMappingAdminService` earn them too, and the defining file does not
  stand out. Requiring the defined name or the file stem to equal the term is the next
  experiment. It needs a new dump and a new pool, because it changes the top 10.
- R1's definition feature first counted any match on a line that looked like a definition,
  so `const w = makeWidget(3)` counted as defining `makeWidget`. It now counts only a match
  on the defined name (`rank::defined_name`). The end-to-end test `tests/rank.rs` caught
  this.
- R2 and nDCG: identical copies carry the same grade (the harness rejects labels where they
  differ), so a copy in the top 10 still adds gain. Moving it down frees the slot for
  another file, which can grade higher or lower: nDCG@10 moved both ways (9 / 5) and
  netted zero. noise@10 counts a copy of a file ranked above as noise, and R2 is judged on
  it.
- Labelling: the first round's agents assigned grades with path-rule scripts. Those rules
  are the scorer's own noise rules, so the evaluation would have graded the scorer on its
  own assumptions. All of those labels were thrown away, and the queries relabelled in
  batches of about 68, with scripts forbidden. One batch's "hand redo" turned out to keep
  most of the script's grades (208 of 238), so its 10 queries were discarded and
  relabelled blind too. Then the copy-consistency check found q043 (`^["']use client["']`)
  graded by location: the main repo's copy of a file 3 and the fork's identical copy 1.
  Its 22 labels were discarded and relabelled blind (21 files 2, a vendored bundle 0). The
  random baseline was dropped from pooling to make relabelling feasible. It is still
  evaluated, with judged@10 0.57.
- Reproducibility: labels and dumps come from a frozen copy of the bench index without the
  scout repo (which holds the golden files and changes with every edit). It is never
  reindexed. Fixing two noise rules (a top-level `Migrations/`, `a.b.test.ts`) changed one
  dump and no metric.
- R3 per held-out project (defaults → tuned, both with copies collapsed): agentic-shopping
  0.854 → 0.850, auto-jev 0.629 → 0.602, battlefield-2 0.748 → 0.751, buck-cheap 0.795 →
  0.786, garmin-mcp 0.819 → 0.752, kanban-man 0.888 → 0.880. The tuned weights do not
  transfer across projects, so `Weights::default` stands. An earlier run compared tuned
  weights with copies collapsed against defaults without (0.792 → 0.770): the same verdict.
- R4 (`bench/results/R4-ab.txt`): phase 2 re-reads only the printed files. With a small
  `--top`, ranked search prints far less than plain search, which is where the memory win
  on broad queries comes from. It is not a latency win: `--top 20` was faster only on
  `literal_common` (288 → 269 ms) and `word` (173 → 152), and equal or slower elsewhere, up to
  +21% (`count_mode` 56 → 68 ms; `short_unindexable` 312 → 350). Printing every file
  costs term matching on every match line plus holding every result: `short_unindexable`
  312 → 746 ms, private memory 285 → 566 MiB. An earlier report of top20 at 0.95x plain
  compared against a noisier plain run.

## Research notes

Summary of a literature/code survey (sources linked), ranked for scout's constraints.

- **Frequency-ordered intersection** (zoekt `findSelectiveNgrams`): pick the two rarest
  ngrams by stored frequency and decode only those; verifying a few docs is cheaper than
  decoding a 20k-entry list. (An earlier version of this note said zoekt "stops once few
  candidates remain"; it does not: it keeps a fixed two, and its positional distance check
  between them filters further, which scout has no equivalent for.) Tried as E22 arm A and
  rejected; a decode budget won instead.
  <https://github.com/sourcegraph/zoekt/blob/main/index/indexdata.go>
- **Compound shards** for many small repos (Sourcegraph: 75% of shards < 2.1 MiB; merging
  cut memory ~50%). Scout's parallel open (E5) already removed most of the open cost.
  <https://sourcegraph.com/blog/tackling-the-long-tail-of-tiny-repos-with-shard-merging>
- **Roaring-style posting containers** (sorted u16 arrays / 8 KiB bitmaps, zero decode):
  Pibiri & Venturini report AND at 0.6 ms vs 2.8 ms for VByte on Gov2.
  <https://arxiv.org/abs/1908.10598>
- **Next-byte / position masks** per posting (GitHub's earlier design, described by
  Cursor): quadgram-like filtering for 2 extra bytes per posting, but the masks saturate on
  large docs. Posting units (E4) attack the same false positives differently. One-byte
  position masks were tried as E20 and rejected; next-byte masks were not tried.
  <https://cursor.com/blog/fast-regex-search>
- **Sparse grams** (GitHub Blackbird): variable-length grams chosen by byte-pair weights;
  index bigger, gains mostly on common short tokens. No published precision numbers.
  <https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/>
- **Suffix arrays** (livegrep): precise but 4–8 bytes per corpus byte and costly updates,
  which does not fit 2 GB machines or incremental updates.
- **Unicode case folding**: `rg -i k` also matches U+212A (Kelvin sign). Checked: scout's
  planner already expands such classes into an OR over the UTF-8 byte variants, so the
  folded index stays exact.
- **Freshness without a daemon**: Windows `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL`; macOS
  FSEvents history replay (`sinceWhen` + device UUID); Linux has no unprivileged journal,
  so a directory scan remains. With a daemon: `ReadDirectoryChangesW`, inotify (per-dir
  watches, overflow → rescan), FSEvents; Watchman's clock tokens avoid timestamp races.

### Cold start (after E21)

A cold query costs about one read-around per shard, and they are taken one at a time. On a
fault in a file mapping, Linux reads `ra_pages` around the faulting page (`filemap.c`),
which is the disk's `read_ahead_kb`. That is 128 KiB by default, as on a typical cloud
VM's disk. On the WSL2 disks E21–E24 ran on it is 8 MiB (found in E24), so one fault
brings in most of a shard's metadata, the tail after its content. E21–E23's cold numbers
are therefore for 8 MiB read-arounds; with 128 KiB a cold shard takes more, smaller faults.
(An earlier version of this note assumed 128 KiB throughout.) Ranked for scout:

1. **Hot metadata in memory at shard open:** decode the `ALWAYS_SEARCH` and `READER_MODE`
   marker lists (read by `plan_repo` for every shard on every query) and copy the fence
   into the heap, ~1.4 KiB per shard. Zoekt keeps its ngram bucket splits on the heap the
   same way. <https://github.com/sourcegraph/zoekt/pull/1172> **Done as E23: cold faults
   only 3–4% fewer and cold latency no better** (a cold query still waits on about one
   read-around window per shard for its own trigrams; at 8 MiB the marker keys' pages
   arrived with that window anyway, which at 128 KiB they may not, so E23's cold effect on
   a cloud disk is unmeasured); it saved ~0.2 ms per warm served query instead. With it measured, the cold lever that skips a shard's
   pages entirely is item 4; item 2 only helps the first queries after a start, not a box
   whose page cache evicts the index under load (E21's cliff).
2. **`madvise(WILLNEED)` on each shard's metadata tail when a generation opens:**
   schedules the reads asynchronously, so ~140 serial cold faults become one overlapped
   batch. memmap2's `advise_range` is safe. Only at open, never per query; and not on a
   burstable instance's 21.75 MB/s EBS baseline, where reading ~180 MB takes 8 s.
   <https://manticoresearch.com/blog/about-startup-mmap-mlock-and-force-preread/>
3. **Measure the target disk first**: its `read_ahead_kb`, and `fio` 4k and 128k random
   read at queue depth 1. Predicted cold cost is faults × (latency + read-around /
   bandwidth).
4. **Per-shard trigram Bloom filter in heap** (~7 MiB for this index): a shard missing a
   query trigram is rejected without touching its pages. Zoekt added word-level Bloom
   filters (PR 126), measured no production gain (PR 318) and removed them (PR 431), but
   its ngram map was in heap; scout's dictionary is on disk.
5. **8-byte dictionary values** (u32 postings offset): ~23 MiB less tail.
6. **Compound shards** for small repos: Sourcegraph merged them mainly to save heap, a
   reason it later removed by moving ngrams off heap (PR 522/540). For scout the cold gain
   is modest and planning loses per-repo parallelism. Only after 1, 2 and 4.
   <https://sourcegraph.com/blog/tackling-the-long-tail-of-tiny-repos-with-shard-merging>
7. Not worth it: `mlock` (locks content too through the safe API; 8 MiB default
   `RLIMIT_MEMLOCK`), and metadata at the file's front (it moves the read-arounds, it does
   not remove any).
8. **`MADV_RANDOM`** disables read-around, so it means more faults of 4 KiB each. At
   128 KiB that likely loses. At 8 MiB, though, a selective cold CLI query reads ~580 MiB
   in ~170 faults (E24), so paired with `WILLNEED` on just the pages a query needs it might
   win where read-ahead is large. Unmeasured; an earlier version of this note dismissed it.

### Output through a pipe (found in E24)

E24's in-process check ran the same searches twice, with stdout sent to `/dev/null`
([results](../bench/results/E24-stats-devnull/summary.txt), from the first version of
`stats-ab.sh`, before it piped the output and timed the whole process) and piped to a
reader ([results](../bench/results/E24-stats/summary.txt)), as scout-bench and an agent's
tool call read it. Piped, the search's own time grows most where the most CPUs verify:
`case_insensitive` (`-i todo`) took 81 → 101 ms at 1 CPU, 32 → 76 at 4 and 27 → 87 at 12
(the base build, medians of 15). So for broad queries at 4 CPUs and up the output path,
not verification, may set the floor. That would fit the CLI A/Bs, where broad queries run
no faster at 12 CPUs than at 4, and often slower (E24's base: `literal_common` 412 → 514
ms, `short_unindexable` 478 → 624). Not yet profiled: how many bytes these queries print, and whether
the time goes to write calls, to the ordered output (E18) waiting on a full pipe, or to the
reader sharing the scope's CPUs.
