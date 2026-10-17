# Experiment log

Corpus: 137 repos under `C:\Users\User\source` (54,258 files, 853 MiB indexed content).
Machine: Ryzen 5 5500 (12 threads), 32 GB RAM, NVMe, Windows 11.
Bench: `scout-bench` with [`bench/queries.tsv`](../bench/queries.tsv). Every query runs as a fresh
process: 1 warm-up, then median of 5 (scout) and median of 2 (rg). From E7 on: 100 scout runs
per query, reporting median, p90 and p99, and 1 rg run. Four `selective_*` queries (0–2
matching files in the whole corpus) were added at E7, so geomeans from E7 on are not
comparable with earlier rows.
**Gate:** each query's `(path, line)` set must equal `rg`'s. A failing gate rejects the change.
Raw results are in [`bench/results/`](../bench/results).

Timings on Windows are noisy (±15% run to run), so only clear wins are kept.

| # | Change | Strict geomean | Off geomean | Verdict |
|---|---|---|---|---|
| E0 | Baseline: per-repo shards, trigram AND/OR, strict freshness via full ignore walk | 3.7x | — | baseline |
| E1 | Trigram-index the bytes before the first NUL of binary files (they used to be always-candidates) | 4.0x | — | **kept** (candidates for `literal_rare` 4096 → 209) |
| E2 | Directory snapshot for strict freshness (re-enumerate known dirs, skip unchanged empty dirs by mtime, evaluate ignore rules only for new names) | 6.7x | 11.6x | **kept** (strict: `literal_rare` 386 → ~220 ms) |
| E3 | Read snapshots only in strict mode; no `is_dir` stat per repo on the search path | 5.8x | 12.4x | **kept** (neutral within noise; removes work). Found and fixed the reader-buffer bug below. |
| E4 | Posting units: large docs indexed as line-aligned 64 KiB blocks, so a doc is a candidate only if one block holds all trigrams | 6.4x | 13.9x | **kept** (`literal_rare` off 132 → 71 ms, candidates 209 → 109, working set 302 → 120 MiB; index +2.4%) |
| E5 | Open repos (shards + snapshot) in parallel | 7.3x | 16.5x | **kept** (open 23 → 5.5 ms off, 51 → 11 ms strict; `literal_none` off 22 ms wall) |
| — | Frequency-ordered intersection / Roaring postings (from research) | — | — | **not tried**: candidate evaluation over all 139 shards measured at 0.4–3.5 ms single-threaded; verification dominates |
| E6 | Unmap shards from several threads before exit | 8.3x | 21.3x | **kept** (teardown 21 → 7 ms for `literal_rare`; `-F function` 184 → 138 ms wall) |
| E7 | Strict freshness from the USN change journal: re-list only directories with journal records since the snapshot's cursor; the search persists refreshed snapshots | 21.4x (scan mode: 9.2x) | 30.4x | **kept** (selective queries strict p50 ~155 → 37 ms, p99 180–256 → 47–50 ms; strict is now off + ~18 ms) |
| E8a | Snapshots encoded with bincode instead of JSON | 21.1x | — | **kept** (on equal storage, single-thread p50 99.5 → 84.9 ms, 12 threads 35.2 → 32.2 ms; listing thread time 100 → 80 ms; snapshots 5.3 → 2.2 MiB). The bench table did not move because of drift, see below |
| E9 | One snapshot pack file instead of one file per repo; journal cursors persisted every 256 KiB of lag instead of 4 MiB | 22.8x | — | **kept** (paired A/B, `SCOUTNI2`: 1 thread p50 120 → 101 ms, p99 170 → 141; 12 threads p50 41.4 → 38.1, p99 52.4 → 49.0) |
| E10 | Unchanged-repo fast path: a listing digest in the index and in a snapshot header decoded alone; a repo the journal proves untouched is searched like `off` | 29.0x | 29.4x | **kept** (paired A/B: 1 thread p50 97.6 → 64.9 ms; 12 threads p50 38.3 → 28.4, p99 44.8 → 33.3). Loading the pack on its own thread, overlapping the shard opens: 12 threads p50 28.4 → 26.5 ms, open 8.0 → 5.8 ms (the same as off) |

Bench memory columns (from E4): *peak WS* includes touched mmap pages, which are shared and
evictable; *peak private* is the commit the process really needs, and is the number that has
to fit a 2 GB machine. Private peaks are 5–60 MiB, except queries with very large output
(`short_unindexable`, ~500k lines: ~290 MiB), because each repo's output is buffered until
its turn in the ordered output.

## Notes

### E7: change-journal freshness
- Design: [design.md, *Strict freshness*](design.md#strict-freshness). Snapshot v3 stores directory
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
- rg searches the bytes *before* the first NUL of a binary file, then prints a warning. Binary
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
  *without output* when the first buffer fill already contains the NUL. A reused searcher
  whose buffer had grown read the whole late-NUL file in one fill and printed nothing.
  A fresh rg process (64 KiB buffer) prints the lines before the NUL.
- Fix: every reader-mode doc is searched with a clone of a searcher whose line buffer was
  never used (slice searches never touch it). Regression test:
  `reader_output_does_not_depend_on_previous_docs`.

### E4: why posting units
- Off-mode `literal_rare` spent ~45 ms of 130 ms wall time *after* the search: unmapping
  ~300 MiB of touched shard pages at exit. Leaking the mappings did not help; the OS pays
  the same at process teardown. So the real problem was touching the pages at all.
- Its 206 candidates totalled 268 MiB: CSV dumps, `typescript.js`, 5 MB Lua tables. They
  contain every trigram of `uselayouteffect` somewhere, but never on one line.
- A simulation over the bench index picked the block size:

  | query | doc-level | 16 KiB | 64 KiB | 256 KiB |
  |---|---:|---:|---:|---:|
  | `useLayoutEffect` | 268 MiB | 64 MiB | 86 MiB | 175 MiB |
  | `class \w+Controller` | 471 MiB | 220 MiB | 372 MiB | 438 MiB |
  | posting entries | 1.00x | 2.06x | 1.50x | 1.20x |

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
- A strict search used to open 276 files (139 shards + 137 snapshots). The pack makes that
  140. It also makes advancing every repo's journal cursor one write instead of 137, so the
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
  large docs. Posting units (E4) attack the same false positives differently.
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
