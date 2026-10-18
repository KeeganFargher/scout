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
harness wrote one); E13's scaling runs and E17's serve load runs were not kept.

Timings on Windows are noisy (±15% run to run), so only clear wins are kept.

| #   | Change                                                                                                                                                                                                                                                                                | Strict geomean                                        | Off geomean | Verdict                                                                                                                                                                                                                                                                                                  |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| E0  | Baseline: per-repo shards, trigram AND/OR, strict freshness via full ignore walk                                                                                                                                                                                                      | 3.7x                                                  | —           | baseline                                                                                                                                                                                                                                                                                                 |
| E1  | Trigram-index the bytes before the first NUL of binary files (they used to be always-candidates)                                                                                                                                                                                      | 4.0x                                                  | —           | **kept** (candidates for `literal_rare` 4096 → 209)                                                                                                                                                                                                                                                      |
| E2  | Directory snapshot for strict freshness (re-enumerate known dirs, skip unchanged empty dirs by mtime, evaluate ignore rules only for new names)                                                                                                                                       | 6.7x                                                  | 11.6x       | **kept** (strict: `literal_rare` 386 → ~220 ms)                                                                                                                                                                                                                                                          |
| E3  | Read snapshots only in strict mode; no `is_dir` stat per repo on the search path                                                                                                                                                                                                      | 5.8x                                                  | 12.4x       | **kept** (neutral within noise; removes work). Found and fixed the reader-buffer bug below.                                                                                                                                                                                                              |
| E4  | Posting units: large docs indexed as line-aligned 64 KiB blocks, so a doc is a candidate only if one block holds all trigrams                                                                                                                                                         | 6.4x                                                  | 13.9x       | **kept** (`literal_rare` off 132 → 71 ms, candidates 209 → 109, working set 302 → 120 MiB; index +2.4%)                                                                                                                                                                                                  |
| E5  | Open repos (shards + snapshot) in parallel                                                                                                                                                                                                                                            | 7.3x                                                  | 16.5x       | **kept** (open 23 → 5.5 ms off, 51 → 11 ms strict; `literal_none` off 22 ms wall)                                                                                                                                                                                                                        |
| —   | Frequency-ordered intersection / Roaring postings (from research)                                                                                                                                                                                                                     | —                                                     | —           | **not tried**: candidate evaluation over all 139 shards measured at 0.4–3.5 ms single-threaded; verification dominates                                                                                                                                                                                   |
| E6  | Unmap shards from several threads before exit                                                                                                                                                                                                                                         | 8.3x                                                  | 21.3x       | **kept** (teardown 21 → 7 ms for `literal_rare`; `-F function` 184 → 138 ms wall)                                                                                                                                                                                                                        |
| E7  | Strict freshness from the USN change journal: re-list only directories with journal records since the snapshot's cursor; the search persists refreshed snapshots                                                                                                                      | 21.4x (scan mode: 9.2x)                               | 30.4x       | **kept** (selective queries strict p50 ~155 → 37 ms, p99 180–256 → 47–50 ms; strict is now off + ~18 ms)                                                                                                                                                                                                 |
| E8a | Snapshots encoded with bincode instead of JSON                                                                                                                                                                                                                                        | 21.1x                                                 | —           | **kept** (on equal storage, single-thread p50 99.5 → 84.9 ms, 12 threads 35.2 → 32.2 ms; listing thread time 100 → 80 ms; snapshots 5.3 → 2.2 MiB). The bench table did not move because of drift, see below                                                                                             |
| E9  | One snapshot pack file instead of one file per repo; journal cursors persisted every 256 KiB of lag instead of 4 MiB                                                                                                                                                                  | 22.8x                                                 | —           | **kept** (paired A/B, `SCOUTNI2`: 1 thread p50 120 → 101 ms, p99 170 → 141; 12 threads p50 41.4 → 38.1, p99 52.4 → 49.0)                                                                                                                                                                                 |
| E10 | Unchanged-repo fast path: a listing digest in the index and in a snapshot header decoded alone; a repo the journal proves untouched is searched like `off`                                                                                                                            | 29.0x                                                 | 29.4x       | **kept** (paired A/B: 1 thread p50 97.6 → 64.9 ms; 12 threads p50 38.3 → 28.4, p99 44.8 → 33.3). Loading the pack on its own thread, overlapping the shard opens: 12 threads p50 28.4 → 26.5 ms, open 8.0 → 5.8 ms (the same as off)                                                                     |
| E11 | Per-search limits (output cap, deadline; checked after every document, skipped when unbounded), the hosted server and generational shards. Not meant as a speed change                                                                                                                | 24.4x (a non-interleaved run, `E11-plain-quiet.json`) | —           | **regression, fixed in E12** (interleaved A/B against E10: `literal_none` and the selective queries +4.6–6.9 ms p50 in both modes, 1.10–1.35x, slower in 47 of 50 rounds; parity all OK). The first, non-interleaved check called it noise, see notes                                                    |
| E12 | Physical memory from one Win32 call (`win.rs`) and `/proc/meminfo` instead of the sysinfo crate, which linked shell32, oleaut32, pdh, powrprof and psapi into every process                                                                                                           | 26.9x                                                 | 27.7x       | **kept** (interleaved A/B against E11: 4.8–6.5 ms faster p50 on every small query, 50 of 50 rounds; `scout --version` 15.5 → 10.2 ms in a direct run, E10 9.3; parity all OK, all 20 queries, both modes)                                                                                                |
| E13 | Indexing budget from usable memory: Linux caps `MemTotal` by every cgroup memory limit (v2 `memory.max`, v1 `memory.limit_in_bytes`) up to the root (containers, systemd units). A correctness fix, not a speed change                                                                | —                                                     | —           | **kept** (budget in a 2 GiB cgroup 4096 → 512 MiB, in 4 GiB 4096 → 1024. Smaller shard caps: 137 → 139–143 shards, search p50 geomean 1.02–1.03x slower, indexing peak RSS 139 → 130, 234 → 183, 341 → 273 MiB at 1/2/4 CPUs)                                                                            |
| E14 | Parity gate: a diff file holding a NUL is re-checked against rg searching it alone. A gate fix, not a speed change                                                                                                                                                                    | —                                                     | —           | **kept** (the gate failed on Linux for `short_unindexable`, 46 lines of `sherlock-nul.txt` that rg printed alone and not among 137 roots; green on Windows presumably by scheduling luck)                                                                                                                |
| E15 | Shard content as per-document lz4 blocks (raw when lz4 does not shrink it), decompressed per candidate                                                                                                                                                                                | —                                                     | —           | **rejected** (1 CPU / 2 GiB: index 1.1 GB → 410 MB and indexing 16.2 → 13.3 s, but search p50 geomean 2.03x slower; `literal_rare` 19 → 61 ms, `rare_regex` 27 → 118, selective queries on small files unchanged)                                                                                        |
| E16 | Verification split into ~4 MiB chunks of candidates across and within repos (was: one repo per task), planned in parallel first                                                                                                                                                       | —                                                     | —           | **kept** (interleaved A/B, same index, 5 rounds: p50 geomean 0.993 at 1 CPU, 0.978 at 4, 0.985 at 12; CPU-bound scans 5–14% faster at 4–12 CPUs in 5/5 rounds; `case_insensitive` +5–9% and 12-CPU selective queries +1–9% slower; parity all OK, and strict parity on Windows)                          |
| E17 | `serve`'s default HTTP workers `clamp(cores / 2, 1, 4)` (was `clamp(cores / 4, 1, 8)`): 4–5 cores 1 → 2, 6–7 cores 1 → 3, 8–11 cores 2 → 4, 12–15 cores 3 → 4, 16–31 cores 4–7 → 4, 32+ cores 8 → 4                                                                                   | —                                                     | —           | **kept** (4 CPUs, mixed closed-loop load: 76.6 → 83.9 req/s at 4 clients, 69.9 → 85.5 at 16; 16 clients p99 selective 473 → 334 ms, broad 493 → 382, ranked 508 → 365. 8 CPUs 2 → 4: 101.9 → 103.5 req/s, p99 within ±8%. 1–3 CPUs unchanged; 6–7 and 12+ cores not measured)                            |
| E18 | Bound E16's reorder buffer (it buffered every chunk ahead of a slow one: all of a CLI search's output). A: at most 4 chunks per worker ahead of the printer. B: at most 64 MiB of unprinted output                                                                                    | —                                                     | —           | **A rejected** (geomean 1.041 at 4 CPUs, 1.042 at 12; `date_regex` 1.215 and 1.254, 0/5 rounds). **B kept** (geomean 0.983 at 4 CPUs, 0.996 at 12; no query slower in all 5 rounds, worst `short_unindexable` 1.061 at 12 CPUs (faster in 1/5); per-query peak RSS at 4 CPUs below E13's on every query) |
| E19 | Fenced dictionary (format v3): trigram keys in their own u32 array, values (count, offset) beside it, and a fence of every 128th key; a lookup binary-searches the fence, then one 512-byte key block (was: binary search over 16-byte entries, ~16 probes over ~10 pages per lookup) | —                                                     | —           | **kept** (interleaved A/B, 5 rounds of 20 runs: geomean 0.707 at 1 CPU, 0.777 at 4, 0.820 at 12; selective queries 0.56–0.63 at 1 CPU; only `case_insensitive` at 12 CPUs slower, 1.055 (2/5); index +0.02%; parity all OK)                                                                              |
| E20 | Positional masks (format v4): each posting carries a byte with bit p mod 8 set for every offset p of the trigram in the unit; a literal's trigrams must agree on a start (masks rotated by their index, ANDed non-zero)                                                               | —                                                     | —           | **rejected** (interleaved A/B vs E19, 5 rounds of 20 runs: geomean 0.998 at 1 CPU, 0.981 at 4, 0.962 at 12; at 1 CPU `selective_*` 1.087–1.127, `ident_regex` 1.168, `case_insensitive` 1.158, all 0/5; index +6.5%, indexing at 1 CPU 16.6 → 20.8 s; parity all OK)                                     |

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

## Ranking experiments

Golden set and harness: [bench/golden](../bench/golden/README.md), design
[ranking.md](ranking.md). The set has 79 queries and 1,556 labels, every one assigned by
hand, plus known-item targets graded 3 automatically. Labels refer to paths in the frozen
golden index (see the golden README), so they never drift with the live corpus. Metrics
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

- **Frequency-ordered intersection** (zoekt `findSelectiveNgrams`): intersect the rarest
  lists first and stop once few candidates remain; verifying 50 docs is cheaper than
  decoding a 20k-entry list. Scout already sorts by decoded length; using the stored
  per-trigram count avoids decoding the long lists at all.
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
