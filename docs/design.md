# scout — design

Indexed, multi-repo grep built on ripgrep's engine. Index hundreds of repos once, then
answer `grep`-style queries across all of them in milliseconds instead of re-scanning
every file.

## Goals

1. **Correct first.** A search returns exactly what `rg` would return over the same repos.
   The index is only a *pre-filter*: every candidate is confirmed by ripgrep's regex
   engine on real contents. The index can make a search faster, never different.
2. **Fast.** A selective query over ~100k files should touch only a handful of them.
3. **Runs on small machines.** 1 CPU / 2 GB RAM must work; 30 cores / TBs of RAM must
   scale. Nothing assumes the index fits in RAM.
4. **Modular like an OSS library.** Index format, where repos come from, and where index
   files live are all swappable behind small traits. One core, many front-ends (CLI now,
   MCP later).

Non-goals for now: ranking, semantic search, MCP server, remote/S3 implementations
(the seams exist, implementations come later).

## Decisions (agreed 2026-10-17)

| Decision | Choice | Why |
| --- | --- | --- |
| Language | Rust core + Rust CLI | In-process use of ripgrep's crates. No process spawn per repo, mmap, no GC. |
| ripgrep | Vendored as a git subtree in `vendor/ripgrep`, used via path deps | We own the code and can patch internals, but can still `git subtree pull` upstream. |
| Freshness | Detect + verify | Files that changed since indexing are searched from disk, deleted files are dropped, so results are never stale. |
| TS workspace | Kept for future thin front-ends (e.g. the MCP) | The empty TS `apps/cli` and `domains/*` placeholders are removed; the Rust crates own those domains now. |

## Architecture

```
crates/
  scout-core            domain model + ports (traits); no I/O
    query/              regex -> trigram query planner
    search/             orchestration: plan -> candidates -> verify -> results
  scout-index-native    custom mmap shard format (corpus bin + trigram postings)   [IndexBackend]
  scout-index-sqlite    same contract on SQLite (rusqlite)                          [IndexBackend]
  scout-source-fs       repos on the local filesystem, gitignore-aware walking,    [RepoSource]
                        directory snapshots + change journal for strict freshness
  scout-store-fs        shards, snapshots and the catalog in a local directory     [ShardStore]
  scout-cli             `scout` binary: composition root, flags, output
  scout-bench           benchmark + correctness harness (scout vs rg)
vendor/ripgrep          upstream ripgrep (grep-searcher, grep-regex, grep-printer, ignore, ...)
```

Dependencies point inward: backends and sources depend on `scout-core`; only the CLI and
bench depend on concrete backends. Cargo enforces this, because a crate can only use what it
declares.

### Ports (the swappable seams)

- **`RepoSource`**: where repo *contents* come from at index time (and for freshness
  checks). It lists files and reads bytes. Today that's the local FS (`ignore` crate, the
  same rules as rg: `.gitignore`, hidden files, binary detection). Later: git remote, S3.
- **`IndexBackend`**: turns a stream of documents into a *shard*, and opens a shard for
  querying (`candidates(query) -> doc ids`, `content(doc)`, `path(doc)`). Today: native
  mmap format, SQLite.
- **`ShardStore`**: where shard files live (a local directory today; an object store with a
  local cache later). A remote store downloads shards to the local cache and the backend
  mmaps them. That's why the shard is self-contained.

### Shard = metadata + corpus bin + postings

Each repo becomes one or more shards. A shard holds:

- **Doc table**: path, size, mtime, and offset into the content blob.
- **Corpus bin**: the contents of every indexed file, concatenated. Verifying a candidate
  is a slice into an mmap, not a file open. On Windows, opening 100k files costs seconds
  (Defender, NTFS), so this is likely the biggest single win. It also makes a shard
  self-contained, which is what lets remote stores work.
- **Units table**: large docs are split into line-aligned ~64 KiB *units*; the table
  maps each unit to its doc. Small docs are one unit. (Experiment E4.)
- **Trigram postings**: for every 3-byte sequence (ASCII case-folded), the sorted list of
  *units* that contain it, delta + varint encoded. Lookup is a binary search in a sorted
  trigram dictionary. A doc is a candidate only if one of its units holds every trigram of
  an AND. That is sound because matches are line-oriented and units end at newlines.
  Native format v2 (`SCOUTNI2`): header, content, docs, paths, units, dictionary,
  postings.

Big repos are split into several shards by a size cap. The cap comes from the memory
budget: building a shard keeps its trigram map in RAM, so the cap bounds indexing memory
on small machines.

### Query planning (regex → trigram query)

Following Russ Cox's *Regular Expression Matching with a Trigram Index*: walk the
`regex-syntax` HIR, computing for each node its exact/prefix/suffix literal sets and a
boolean query. Each literal of 3+ bytes becomes an AND of its trigrams; alternations become
ORs; anything unbounded becomes "match all". Case-insensitive queries fold the same way the
index does. When the plan is "match all" (e.g. `.`, or patterns shorter than 3 bytes),
the search falls back to scanning the corpus bins, which is still faster than walking the FS.

### Search pipeline

1. Plan the query once.
2. Fan out over shards on a thread pool sized to the machine (`available_parallelism`,
   overridable with `--threads`).
3. Per shard: intersect/union postings → candidate docs → run `grep-searcher` +
   `grep-regex` on each candidate's content slice, with `grep-printer` producing rg-style
   output.
4. Freshness (default `strict`): docs whose file changed on disk are searched from disk
   instead, deleted files are dropped, new files are searched directly.
   `--freshness off` trusts the index. See *Strict freshness* below.
5. Stream results in a stable order. `-m`/limits stop early.

### Adaptivity to hardware

- Shards are mmapped, so the OS pages them in and out and RAM use tracks the working set.
- Threads default to the available cores.
- The indexing memory budget defaults to a fraction of physical RAM and sets the shard
  size cap.
- No component needs the whole index in memory.

## Benchmarking & the optimisation loop

`scout-bench` runs a fixed query suite over the indexed corpus (the ~121 repos under
`C:\Users\User\source`):

- **Query classes:** rare literal, common literal, identifier regex, alternation,
  case-insensitive, short (<3 bytes, unindexable), and no-match.
- **Metrics:** wall time (median of N warm runs, plus one cold run), candidates vs actual
  matching files (filter precision), index size, build time, peak memory.
- **Correctness gate:** every query's set of `(path, line)` matches must equal `rg`'s over
  the same repos. A change that breaks this is rejected regardless of speed.
- **Baseline:** plain `rg` over all repo roots.

Loop: hypothesis → change → bench → keep or revert, recorded in
[`experiments.md`](experiments.md). Stop when several consecutive experiments stop
improving.

Candidate experiments (informed by codesearch, zoekt, GitHub Blackbird):

- positional trigrams (zoekt) to check literal adjacency without verification;
- sparse n-grams (Blackbird);
- posting compression (varint vs. bitpacked / Elias–Fano / roaring);
- corpus-bin compression for low-disk setups;
- verification via `memchr::memmem` for pure literals instead of the regex engine;
- shard sizing and parallelism granularity.

### Strict freshness

Strict mode needs the repo's current listing (the files rg would search, with size and
mtime) to compare against the index. A full ignore-aware walk costs ~170 ms across the
bench corpus, so the source keeps a **directory snapshot** per repo (`scout-source-fs`,
`snapshot.rs`). All repos' snapshots live in one store file, `snapshots.pack`
(`scout-core`, `snapshots.rs`), because each file open is a measurable cost on Windows.
A snapshot holds:

- every directory the walker entered, with its file id, its accepted files (name, size,
  mtime), and the child names it rejected (kind-tagged, because `build/` rejects a
  directory but not a file);
- the files outside those directories that can change ignore decisions (ancestor ignore
  files, `.git/info/exclude`, global gitignore and git config). If any changed, the source
  does a full walk;
- the file ids of the repo root's ancestors, and the volume change-journal position (USN
  cursor) the snapshot is valid at.

A refresh re-lists only the directories that may have changed and decides new names with
a real walker scoped to their directory. A refreshed listing therefore always equals a
full walk; only the cost differs. Two ways to choose which directories to re-list
(`--change-detection`):

- **`journal`** (default): read the NTFS/ReFS USN journal since the snapshot's cursor,
  using `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (no admin needed). Only directories that
  have a record of a child at or after the cursor are re-listed. A rename or delete of the
  root or one of its ancestors forces a scan, and so does a cursor the journal no longer
  covers (the journal keeps roughly 35 minutes on a busy volume). One journal read per
  volume serves every repo (`FsContext`).
- **`scan`**: re-list every known directory, skipping fileless directories whose mtime is
  unchanged. Used automatically on volumes without a journal.

A search hands back the snapshots it changed, and the CLI writes them into the pack
(staging name, then atomic rename), so the next search starts from the new state. A cursor
is also advanced once it lags by more than 256 KiB of journal, which keeps the journal read
on every search under ~1.5 ms.

**Unchanged fast path.** The index records a 128-bit, order-independent digest of the
listing it indexed. Each snapshot carries the digest of its own listing, in a header that
decodes on its own. When the digests match, the watch list is unchanged, and the journal
shows nothing under any of the repo's directories, the current listing provably *is* the
indexed one. The repo is then searched exactly like `--freshness off`, with no map, no
listing and no full decode. That is the common case: most repos are untouched between
searches, so strict costs a few ms over off.

Known limits, shared with git's index: a file whose content changed but whose size and
mtime did not looks unchanged, and so does a hard link modified through another path.

## Open risks

- **Strict freshness off Windows** uses `scan` mode (no journal integration yet). The
  natural next steps are fanotify on Linux and FSEvents on macOS, which need a resident
  process; see `docs/experiments.md`.
- **Output parity with rg.** rg's defaults (hidden files, `.ignore`, binary handling,
  max file size) must be mirrored at index time, or results diverge. The bench gate catches
  this.
