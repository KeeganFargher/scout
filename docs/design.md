# scout — design

Indexed, multi-repo grep built on ripgrep's engine. Index hundreds of repos once, then
answer `grep`-style queries across all of them in milliseconds instead of re-scanning
every file.

## Goals

1. **Correct first.** A search returns exactly what `rg` would return over the same repos.
   The index is only a _pre-filter_: every candidate is confirmed on real contents,
   using byte search for an exact case-sensitive literal or ripgrep's regex engine.
   The index can make a search faster, never different.
2. **Fast.** A selective query over ~100k files should touch only a handful of them.
3. **Runs on small machines.** 4 GiB RAM and a few CPUs is the smallest supported box
   (since 2026-10-28; earlier measurements below include 1 CPU / 2 GiB); 30 cores / TBs
   of RAM must scale. Nothing assumes the index fits in RAM.
4. **Modular like an OSS library.** Index format, where repos come from, and where index
   files live are all swappable behind small traits. One core, many front-ends (CLI now,
   MCP later).

Non-goals for now: semantic search, MCP server, remote/S3 implementations
(the seams exist, implementations come later).

## Decisions (agreed 2026-10-17)

| Decision     | Choice                                                            | Why                                                                                                                                                                                                                                   |
| ------------ | ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Language     | Rust core + Rust CLI                                              | In-process use of ripgrep's crates. No process spawn per repo, mmap, no GC.                                                                                                                                                           |
| ripgrep      | Vendored as a git subtree in `vendor/ripgrep`, used via path deps | We own the code and can patch internals, but can still `git subtree pull` upstream. One patch so far: `Searcher::set_first_line_number` in grep-searcher, so a unit read from the middle of a file keeps its file line numbers (E32). |
| Freshness    | Detect + verify                                                   | Files that changed since indexing are searched from disk, deleted files are dropped, so results are never stale.                                                                                                                      |
| TS workspace | Kept for future thin front-ends (e.g. the MCP)                    | The empty TS `apps/cli` and `domains/*` placeholders are removed; the Rust crates own those domains now.                                                                                                                              |

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

- **`RepoSource`**: where repo _contents_ come from at index time (and for freshness
  checks). It lists files and reads bytes. Today that's the local FS (`ignore` crate, the
  same rules as rg: `.gitignore`, hidden files, binary detection). Later: git remote, S3.
- **`IndexBackend`**: turns a stream of documents into a _shard_, and opens a shard for
  querying: `postings(trigram) -> unit ids`, `unit_doc(unit)`, `meta(doc)`, `content(doc)`.
  Query planning and candidate sets live in the core, so a backend only stores and reads.
  Today: native mmap format, SQLite.
- **`ShardStore`**: where shard files live (a local directory today; an object store with a
  local cache later). A remote store downloads shards to the local cache and the backend
  mmaps them. That's why the shard is self-contained.

### Shard = metadata + corpus bin + postings

Each repo becomes one or more shards. A shard holds:

- **Doc table**: path, size, mtime, offset into the content blob, and the doc's first
  content block.
- **Corpus bin**: the contents of every indexed file, concatenated. Verifying a candidate
  is a slice into an mmap, not a file open. On Windows, opening 100k files costs seconds
  (Defender, NTFS), so this is likely the biggest single win. It also makes a shard
  self-contained, which is what lets remote stores work.
- **Content blocks** (E29): each doc's content is stored as its units' spans, each block
  either as is or lz4-compressed (HC level 6, only where that is smaller: HC output is
  smaller and decodes faster than lz4's fast mode, E38), with a table of stored and
  raw lengths. A raw index borrows content straight from the map; an lz4 index decodes a
  doc into a per-thread buffer. `scout index --content auto` (the default, also used by
  `sync`) picks lz4 once the content of every registered repo is more than half of usable
  memory: then the raw index cannot stay in the page cache, and every query reads from disk
  again what it missed, where the lz4 index (content ~3x smaller, the whole index ~2.5x on
  chromium + llvm-project) stays cached and costs a decode
  instead. Below that, raw. It is decided per indexing run for the repos that run writes
  (a `sync` cycle reindexes only what moved), so repos left alone keep their codec and an
  index can hold both: the codec is per block, read alike. `auto` sizes against the
  machine that indexes; an index built for a smaller one takes `--content lz4`.
- **Units table**: large docs are split into line-aligned ~64 KiB _units_; the table
  maps each unit to its doc. Small docs are one unit. (Experiment E4.)
- **Gram postings**: for every 3-byte sequence and tagged 2-byte sequence (ASCII case-folded), the sorted list of
  _units_ that contain it, delta + varint encoded. Lookup goes through a fence of every
  128th key to one 128-key block of the sorted trigram keys, so it usually touches about
  three pages (sections are not page-aligned, so a block can straddle two) (E19); the fence
  itself is copied to the heap when the shard opens (E23). A doc can match only if one of
  its units holds every trigram of an AND; that is sound because matches are
  line-oriented and units end at newlines. The planner may stop short of reading every
  trigram's list (below), so candidates are a superset of those docs, never a subset.
  The marker lists (docs searched for every query, and docs rg must read through its
  reader) are decoded once at open, not per query (E23).
- **Trigram filter**: a register-blocked Bloom filter over the shard's trigram keys. Each
  key sets 4 bits inside one 64-bit word, and there are 6 bits of filter per key. A query
  that needs a trigram the shard lacks skips the shard without reading its dictionary or
  postings (E24). `serve` copies every table between content and postings (docs, blocks,
  paths, units, the dictionary, the filter) to the heap at startup, so a query on an index
  the page cache has dropped reads only postings and content: ~211 MiB for chromium +
  llvm-project (8% of the index). Cold selective queries ran 3–6x faster than with only
  the filter there (`selective_long` 522 → 133 ms, no-match 242 → 40; E36 then E37,
  separate A/Bs). Each generation of shards may copy an eighth of memory by default
  (`--table-mb`); shards past that read their tables through the map. The CLI reads them in place through the map, so a short-lived
  process reads only what its query looks up instead of copying everything.
- **Warm-up**: once a sync cycle has finished, `serve` reads each newly served generation's
  shard files into the page cache in the background (E49), if the whole index fits the
  budget: usable RAM less two generations of tables, three quarters by default
  (`--warm-mb`; 0 turns it off). A larger index is not read at all, since reading part of
  it would evict what queries read and each new generation would read it from disk again.
  It reads 256 KiB at a time, each shard from its end (one read per chunk; forward, the
  kernel split them in two), only after no search or sync cycle has run for 100 ms, and
  skips the shards an earlier generation's walk read. A client that searches at least every
  100 ms keeps it from reading; one that searches every few hundred ms can wait behind a
  read (up to ~20 ms on a small query). On the 4 GiB, gp3 box a restart's 18 bench
  queries ran in 2.3 s instead of 12.0 s when sent 45 s after ready, and those sent at ready
  stayed within the harness's noise.

Native format v9 (`SCOUTNI9`): header, content, docs, content blocks, paths, units,
dictionary keys, values and fence, trigram filter, postings. Every section starts where the one before it
ends. A shard in another format version is refused with a message to rebuild the index
with `scout index`.

Each document row also stores a 32-byte Bloom summary of normalized numeric four-character
shapes. Regexes with no literal gram constraint can use it to reject documents before
decoding. Letters collapse to one symbol; ASCII digits and all non-ASCII Unicode
characters share another, so Unicode digits are never excluded by a Unicode-version
assumption. Unsupported raw-byte projections impose no shape constraint. Binary/BOM
reader-mode documents are verified independently of these summaries.

Big repos are split into several shards by a size cap. The cap comes from the memory
budget: building a shard keeps its (trigram, unit) pairs in RAM, so the cap bounds
indexing memory on small machines.

Each indexing worker takes one repo at a time, so one huge repo (chromium) would get one
thread. Inside a repo, files are therefore read, classified and split into trigrams on the
worker's share of the threads, ahead of the one thread that writes shards, which takes them
in path order: the shards are byte-identical to a serial build's. The read-ahead may hold
an eighth of a shard's content (plus 4 KiB a file), a small part of the 5x-shard memory a
worker plans for. A one-CPU machine reads inline, since the handoff to another thread only
costs CPU there (E25).

### Query planning (regex → trigram query)

Following Russ Cox's _Regular Expression Matching with a Trigram Index_: walk the
`regex-syntax` HIR, computing for each node its exact/prefix/suffix literal sets and a
boolean query. Each literal of 3+ bytes becomes an AND of its trigrams; alternations become
ORs; a two-byte literal uses its tagged pair, and a one-byte unconstrained pattern plans
a scan. Case-insensitive queries fold the same way the index does. A "match all" gram
plan can additionally require normalized numeric shapes derived from the regex HIR.
Only survivors are decoded and matched by the full matcher.

Indexed plain text writes rg's path/line/count format directly into a chunk's output
buffer. Printed output buffers are recycled with a 32 MiB retention cap; one path buffer
and one initialized decode buffer are reused while visiting a chunk's documents.
Exact, case-sensitive literals without word boundaries use `memmem`;
other patterns use the regex searcher with the same text sink. Binary/BOM reader-mode
documents use rg's reader and printer so transcoding and binary warnings stay exact.
The reference LZ4 decoder validates input/output bounds and the exact decoded length;
compressed blocks larger than its signed 32-bit length limit are rejected before allocation.
Even with LZ4 enabled, blocks smaller than 4 KiB stay raw to avoid small decoder calls
and borrow the mmap directly. This trades some page-cache space for less CPU work on
broad queries; larger blocks are compressed only when that saves space.

### Search pipeline

1. Plan the query once.
2. Plan every repo in parallel on a thread pool sized to the machine
   (`available_parallelism`, overridable with `--threads`). Per shard, the query is first
   checked against the trigram filter. A shard where it cannot match is skipped, though
   its always-searched docs are still verified, and an OR branch the filter rejects is
   dropped (E24). Then intersect/union postings → candidate docs, cut into chunks of
   ~4 MiB of candidates. For an AND, every
   trigram's unit count is read from the dictionary first (a zero empties it without
   decoding anything); lists are then decoded rarest first, and only while decoding the
   next costs less than verifying the candidates already left (E22).
3. Verify the chunks on up to that many threads (fewer when there are fewer chunks):
   byte search for exact case-sensitive literals, `grep-searcher` + `grep-regex` for
   other patterns, and rg's reader/printer for binary/BOM content. Plain text uses the
   direct rg-style formatter. Chunks cross repo boundaries, so a few large repos no longer
   bound the search (E16); one file is still verified by one thread, as in rg. Output is
   printed in chunk order; workers run ahead of the printer only while less than 64 MiB
   is waiting to print (E18), and one thread verifies inline without a buffer. Each
   verifier asks the kernel to read its candidates' content ahead in ~4 MiB blocks
   (`MADV_WILLNEED`, queued a block before it is needed), so a cold or evicted index is
   read in large requests instead of one 128 KiB read-around per page fault (E28).
   Candidates at most 32 KiB apart are queued as one range: on gp3 most cold queries are bound
   by reads per second, and one read saved is worth more than the gap's bytes (E42).
   A candidate bigger than one unit (64 KiB) is read only at its candidate units: the
   blocks table stores each block's count of lines before it, so the searcher numbers
   those units' lines from where they sit in the file (E32); counts and `-m` carry across
   a file's units (E33). Ranking, haystack anchors, reader-mode files, always-searched docs,
   docs of one unit and files searched from disk read the whole file. Posting lists are mapped without read-around
   (`MADV_RANDOM`) and each list asks for exactly its own range; the dictionary keeps
   read-around, which an index bigger than memory needs (E30).
4. Freshness (default `strict`): docs whose file changed on disk are searched from disk
   instead, deleted files are dropped, new files are searched directly.
   `--freshness off` trusts the index. See _Strict freshness_ below.
5. Stream results in a stable order. `-m`/limits stop early.

### Adaptivity to hardware

- Shards are mmapped, so the OS pages them in and out and RAM use tracks the working set.
- Threads default to the available cores.
- The indexing memory budget defaults to a quarter of usable RAM (capped at 4 GiB) and sets
  the shard size cap.
- No component needs the whole index in memory.
- On Linux, usable RAM is the tightest cgroup memory limit (v2 or v1) when one is set,
  as in a container or a systemd unit, not the host's RAM (E13). Windows reads physical
  RAM.

### Measured on small and large machines

Linux (WSL2 Ubuntu), each profile a cgroup: `systemd-run --user --scope` with
`MemoryMax`, no swap and `CPUQuota`, plus `taskset`. The page cache for the mmapped
shards is charged to the same cgroup, as on a real VM. Corpus: the 137 bench repos
mirrored onto the Linux disk (54k files, 854 MiB of content, 1.0 GiB index). The harness
scripts are described in `docs/experiments.md` (E13–E17).

| profile           | index (warm) | index peak RSS | selective p50 | broad sweep p50 (`date_regex`) | 20-query p50 total |
| ----------------- | ------------ | -------------- | ------------- | ------------------------------ | ------------------ |
| 1 CPU / 2 GiB     | 16.2 s       | 130 MiB        | 9–17 ms       | 680 ms                         | 3.1 s              |
| 2 CPUs / 4 GiB    | 11.3 s       | 183 MiB        | 9–14 ms       | 361 ms                         | 2.1 s              |
| 4 CPUs / 4 GiB    | 7.5 s        | 273 MiB        | 9–14 ms       | 220 ms (E16: ×0.955)           | 1.9 s              |
| 8 CPUs / 16 GiB   | 10.9 s       | 350 MiB        | 10–15 ms      | 166 ms                         | 1.9 s              |
| 12 CPUs, no limit | 6.2 s        | 449 MiB        | 12–16 ms      | 154 ms (E16: ×0.859)           | 1.9 s              |

CLI numbers (a process per query, so they include start-up and opening 140 shards);
100 runs per query, measured before E16 (the marked ratios are E16's interleaved A/B).
No profile was OOM-killed. Indexing memory follows the budget (workers × shard cap), not
the corpus. A search's RSS counts the shard pages it touched, so it is an upper bound; its
own allocations were 5–60 MiB in the Windows bench, plus buffered output, which `serve`
caps at 16 MiB. 8 CPUs indexed slower than 4, presumably because its 4 GiB budget doubles the shard cap
(102 vs 51 MiB), so fewer, larger shards leave cores idle at the end. One run each, not
yet investigated.

Served (`scout serve`, closed-loop clients sending a mix that is 81% selective lookups,
E17; 60 s per cell, each profile at its default worker count, on the first E16 build, so
1 CPU reads a few percent low), selective p50 / p99:

| profile         | 1 client   | 4 clients   | 16 clients    | throughput |
| --------------- | ---------- | ----------- | ------------- | ---------- |
| 1 CPU / 2 GiB   | 11 / 83 ms | 97 / 530 ms | 476 / 1165 ms | ~29 req/s  |
| 4 CPUs / 4 GiB  | 5 / 26 ms  | 35 / 119 ms | 171 / 334 ms  | ~85 req/s  |
| 8 CPUs / 16 GiB | 5 / 24 ms  | 28 / 86 ms  | 145 / 246 ms  | ~103 req/s |

What does not scale with cores: one file is searched by one thread, so a query whose
output comes mostly from one huge file (two CSVs of 20 and 24 MB print 60% of
`literal_common`) stays at ~435 ms (CLI bench) to ~475 ms (E16's A/B) from 4 to 12 CPUs. Throughput past one loaded
client is set by the CPUs: a 1-CPU box serves about 29 searches a second of this mix.

Cold and warm, one request at a time (E21, geomean p50 of the 20 queries; cold = the index
evicted from the page cache before every request, checked with `mincore`):

| profile           | CLI warm | CLI cold | served warm | served cold |
| ----------------- | -------- | -------- | ----------- | ----------- |
| 1 CPU / 2 GiB     | 72 ms    | 2232 ms  | 37 ms       | 2047 ms     |
| 4 CPUs / 4 GiB    | 49 ms    | 1720 ms  | 22 ms       | 1527 ms     |
| 12 CPUs, no limit | 52 ms    | 1725 ms  | 24 ms       | 1522 ms     |

A cold query waits on one or two random dictionary reads per shard (146–294 major faults),
one after another, so cold latency follows the disk's read latency and the shard count,
not the CPUs. Warm needs the whole index in the page cache: served at 1 CPU, the geomean
is 36–40 ms down to a 1280 MiB cgroup limit (1.07 GB index), 215 ms at 1 GiB, 1040 ms at
768 MiB and 1594 ms at 512 MiB, never OOM-killed.

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
shows nothing under any of the repo's directories, the current listing provably _is_ the
indexed one. The repo is then searched exactly like `--freshness off`, with no map, no
listing and no full decode. That is the common case: most repos are untouched between
searches, so strict costs a few ms over off.

Known limits, shared with git's index: a file whose content changed but whose size and
mtime did not looks unchanged, and so does a hard link modified through another path.

## Hosted sync

`scout serve` runs scout as a long-lived server. It mirrors git remotes into working
copies on its own disk, reindexes what changed on a schedule, and answers searches over
HTTP. `scout sync` runs one cycle of the same loop and exits (for cron, CI and
tests). The loop lives in `scout-sync` (configuration, GitHub listing, git) and
`scout-cli` (indexing, publishing, serving), so the MCP server can later call the same
pieces.

**Configuration** is a TOML file (`--config`):

```toml
mirror_dir = "/var/lib/scout/mirrors" # one working copy per repo; scout owns it (see below)
interval_secs = 300
token_env = "GITHUB_TOKEN" # name of the env var, never the token itself
fetch_jobs = 8 # optional; default clamp(2 x cores, 4, 16)
github_api = "https://api.github.com" # optional; GitHub Enterprise, or a test fake
git_timeout_secs = 900 # optional; hard deadline per repo per cycle

[[repo]]
url = "https://github.com/BurntSushi/ripgrep"
name = "ripgrep" # optional; default "<owner>-<repo>"
branch = "master" # optional; default: the remote's HEAD

[[github]]
owner = "rust-lang"
kind = "org" # or "user"
include_forks = false
include_archived = false
```

`[[github]]` entries are expanded through the REST API on every cycle
(`/orgs/{o}/repos?type=all` or `/users/{u}/repos?type=owner`, 100 per page; `owner`, because
a user listing with `type=all` adds repos of others they merely collaborate on), so repos added to or removed from
an org follow automatically. The token is read from the named env var once at startup.
It goes to the API as a bearer header. It goes to git through `GIT_CONFIG_COUNT`
environment config, as `http.https://github.com/.extraHeader =
AUTHORIZATION: basic base64("x-access-token:" + token)`, the form `actions/checkout` uses. It is never put on a command line
(other users could see it in the process list) or in an error message. The header is scoped
to the git host of `github_api` (`api.github.com` → `https://github.com/`, an Enterprise
`https://ghe.example/api/v3` → `https://ghe.example/`), so it is never sent to any other
remote. A token over plain `http://` is refused unless the host is loopback. Remote URLs
carrying credentials (any user part over http(s), a password on any scheme) or starting
with `-` (option injection) are rejected at config load. Inherited `GIT_DIR`-style
variables and git's tracing variables (`GIT_TRACE*`, `GIT_CURL_VERBOSE`, which can print
the Authorization header into stderr and so into failure reports) are cleared, and
prompts are disabled, so git never reads a terminal or the wrong repository.

One remote is mirrored once. Remotes are compared by normalised URL (`.git`, trailing
slash, scheme and ssh user dropped, so `git@github.com:a/r` equals `https://github.com/a/r`;
host case ignored, path case ignored only on GitHub), and the first mention wins:
`[[repo]]` entries come before owner listings, so an explicit entry's name and branch
override the listing's. The same remote twice in `[[repo]]` is reported, not silently
merged. A name already
used by a different remote, in the config or in the catalog, is refused and reported;
taking it over would reset that working copy onto an unrelated repo and replace its
index. Freeing a name is an explicit `scout remove`.

**Fetching shells out to `git`.** That runs the real git, which supports every protocol,
credential helper, proxy and server quirk that users' remotes already work with. It is the
fastest fetch implementation, and it adds no C build dependency (a cross-build already trips
on bundled SQLite). gix is pure Rust, but its checkout/reset of a working tree is less
complete. libgit2 lags git on protocol v2 and partial clone, and adds a C toolchain
requirement. The cost is a runtime dependency on `git` ≥ 2.31 (for `GIT_CONFIG_COUNT`),
checked at startup.

- **First cycle:** `git clone --depth 1 --single-branch [--branch B]`. The index needs the
  tree, not the history, so a shallow clone is the cheapest way to get it.
- **Later cycles:** `git fetch --depth 1 origin <B or HEAD>`, then
  `git reset --hard FETCH_HEAD` only when `FETCH_HEAD` differs from `HEAD`.
- **Only sync writes there:** nothing else writes to the working copies, so there is
  nothing to merge or clean.
- **Parallelism:** fetches are network-bound, so they run on their own pool. It defaults
  to `clamp(2 × cores, 4, 16)` and is independent of the indexing workers.
- **Stalls:** each repo's sync has one hard deadline (`git_timeout_secs`) across all its
  git commands. Past it, git and the helpers it spawned (`git-remote-https`, `ssh`) are
  killed: on Unix git runs in its own process group and the group is killed (reaching
  `git-remote-https` → `fetch-pack` → `index-pack` too), on Windows `taskkill /T` kills
  the tree; git itself is always killed and reaped even if reaching the helpers fails.
  Every `*.lock` a killed git leaves in `.git` and `.git/refs` is cleared before the
  repo's next sync, under the mirror lock. `http.lowSpeedLimit`/`lowSpeedTime` end dead http transfers sooner
  (they do not apply to ssh, where the deadline does), and API requests have connect and
  total timeouts. Indexing starts once every fetch is done, so one hung remote delays the
  whole cycle's publish by up to `git_timeout_secs` (15 min by default), then fails alone.
- **Failures:** a repo whose fetch fails keeps its current index, which is still exactly
  the commit it was built from. The failure is reported, the cycle carries on, and the next
  cycle retries. `scout sync` exits non-zero if any repo failed.

**Change detection is by commit.** `RepoIndex.commit` records the commit an index was built
from. A synced repo is reindexed only when its working copy's `HEAD` differs from that
commit, or when it has no index yet. This is exact and costs nothing: no listing, no
journal. On Linux, where strict freshness has no journal and would rescan, it is the only
cheap check. Repos added with `scout add` have no commit (their working tree may be dirty),
and sync never touches them.

**Repos follow the configuration.** Every entry that sync creates records its remote
(`RepoEntry.remote`: URL and branch). When a cycle's resolved set no longer contains a
synced repo (removed from the config, or from the org), the repo leaves the catalog, its
shards are retired, and its working copy is deleted. Only entries with a remote are ever
removed this way.

**GitHub errors fail fast.** A 403 or 429 with `X-RateLimit-Remaining: 0` ends the listing
with an error naming the reset time. The cycle then keeps every repo of that owner as it
was, rather than reading a partial listing as "repos were deleted". There is no retry
inside a cycle; the next cycle is the retry.

**Incremental indexing: whole repos, only the changed ones.** A cycle reindexes only the
repos whose commit moved, and each of those fully. Shards are immutable and cover one repo.
A repo's reindex costs time in proportion to its size: the median repo in the bench corpus
indexes in well under a second, and the whole 137-repo corpus in about 40 s. Reusing
unchanged files inside a repo (`git diff --name-only old new` to pick the files, and a
shard that copies the rest) would save time only on large repos with small commits. It
also needs a merge step in the shard format. Deferred until a measured cycle shows
reindexing dominating.

**Publishing is generational, so readers never see a mix.** Before this, shard names were
deterministic (`repo.0.native`) and a reindex renamed new shards over the old ones. A
search reads the catalog, then opens its shards a few milliseconds later. A reindex
landing in between gave it the new shard 0 next to the old catalog's digest, or a deleted
shard 1. (Renaming over or deleting a _mapped_ shard is not itself a problem, on Windows
either: measured, both succeed, because files are opened with delete sharing and existing
maps keep the old bytes.) Now:

1. Shards are named `<repo>.<generation>.<n>.<backend>`, where the generation is a fresh
   per-index-run id, so new shards never collide with live ones.
2. New shards are published, then the catalog is replaced with one atomic rename. A reader
   sees the old catalog with the old shards, or the new catalog with the new shards.
3. Replaced shards are not deleted at once. The new catalog lists them as `retired`, with
   the time they were retired, and any writer deletes retired shards older than 60 s. That
   grace covers a reader that loaded the previous catalog and has not opened its shards
   yet (milliseconds in practice). A failed delete is an error like any other.
4. Writers (`add`, `remove`, `index`, a sync cycle) take an exclusive lock on
   `<index-dir>/write.lock` (`File::lock`), so two writers never lose each other's catalog
   updates. The catalog's temp file is per process. Searches take no lock. A sync cycle
   takes it only after fetching, so slow fetches never block `scout add` or `scout index`;
   the mirror directory has its own lock (`.scout-sync.lock`), held for the whole cycle, so
   two sync processes never share working copies.
5. After the commit, the mirror directory is swept: everything that is neither the root
   (or holds the root) of any catalog repo, path-added ones included, nor wanted this
   cycle is deleted (removed repos, `.partial`
   clones, leftovers of a cycle that failed between publishing and cleaning up). So the
   directory converges on the catalog whatever went wrong before, and a working copy goes
   only once no published catalog names it. Because of the sweep the directory is scout's:
   a missing or empty one is adopted (a `.scout-mirrors` marker records it); one with other
   content is refused rather than emptied. Changing `mirror_dir` leaves the old directory
   behind for the operator to delete.

Disk is freed by writers only: retired shards are deleted by the next writer after the
grace period, so an idle index directory keeps them until then.

The snapshot pack keeps its last-writer-wins rule. Every snapshot is valid at its own
journal cursor, so a lost update only costs a longer refresh. A search persists its refreshed snapshots
under a _try_ of the writer lock, against the catalog as it is then: a snapshot of a repo
removed or moved meanwhile is dropped, never written back under a name a later repo may
reuse. When a writer holds the lock the search skips persisting rather than wait behind a
cycle. Sync writes snapshots for the
repos it indexed, so CLI strict searches on the same index dir keep their fast path.

**The server searches with `--freshness off`.** The working copies change only when a sync
cycle changes them, and a cycle publishes the new index before the next search can see the
new commit. So the index always equals the working copy at the published commit, and strict
mode would only add cost (a full rescan on Linux) with nothing to find. The server holds the
open shards of the current catalog as one _generation_ behind an `Arc`. Before and after
every cycle the server compares the catalog on disk with the one it serves (repo names,
roots and shards) and, when they differ, opens the new generation and swaps the pointer.
Comparing rather than trusting the cycle's report also picks up a `scout index` run from
the CLI, and a cycle that failed after publishing. In-flight searches finish on the generation
they started with, and the old shards are unmapped when the last of those searches drops
it. Content is served from the shards, never from the working copy, so a reset in the
middle of a search cannot tear its results.

**Hardware adaptivity.**

- **Indexing:** a cycle uses the same worker and memory plan as `scout index`: a quarter of
  RAM, capped at 4 GiB.
- **Searching:** each search uses up to `cores` threads. The HTTP pool defaults to
  `clamp(cores / 2, 1, 4)` concurrent searches (E17), so one box neither oversubscribes 1 CPU nor
  leaves 30 cores idle.
- **Memory:** the shards are mmapped, so a 2 GB box serves an index larger than its RAM, at
  page-cache speed for the working set.

**Toward AWS.** Shard objects are now write-once with unique names, which maps directly
onto S3. The catalog is the single mutable object (a conditional put replaces the rename).
A `ShardStore` for S3 needs a local cache behind `local_path`, because shards are mmapped.
The working copies stay on local disk: they are a build input, not state, and the next
cycle rebuilds them if lost.

**Tests use local bare repos over `file://` URLs**, never the network. A plain-path clone
ignores `--depth`, so `file://` is what exercises the shallow path the server uses.
GitHub listing is tested against a local fake API (the base URL is configurable).

**HTTP API (first cut).**

- **`GET /search`:** query parameters `q` (pattern), `F`, `i`, `S`, `w`, `n`, `l`, `c`
  (flags, `=1`), `m` (max count), `repo` (repeatable), and `rank=1` with optional `top`
  (default 20; [ranking.md](ranking.md)), which prints only the best `top` files, each
  exactly as plain search prints it. `top` without `rank=1` is a 400. It returns
  `text/plain`, rg-identical output, with `X-Scout-Matched-Files` and `X-Scout-Generation` headers. It
  returns 400 for a bad pattern, an unknown repo, an unknown parameter or a missing `q`.
  Each search has two limits, checked inside the search after every document, so it stops
  within one document of crossing either (one huge matching document can still overshoot
  the output cap by its own size; the deadline is not checked while strict freshness
  lists a repo, which the server never does): output (`--max-output-mb`, default 16; the
  count spans all repos, because each repo's output is buffered until its turn in the
  ordered output) answers 422 (not 413, which means the _request_ was too large), and time (`--search-timeout-secs`, default 30, so a few
  broad queries cannot hold every HTTP worker) answers 503. Each request runs under
  `catch_unwind`, so a panic costs that request, not a worker.
- **`GET /status`:** JSON with the generation, its repos and commits, and the last
  cycle's time and failures, or its error when the whole cycle failed.

There is no authentication: bind to localhost or put it behind a proxy. MCP will sit next
to it and call the same core.

**Catalog format.** The catalog gained `remote`, `commit` and `retired`, with no migration (by policy): an index directory from before this change must be rebuilt (`scout add`, `scout index`).

## Open risks

- **Strict freshness off Windows** uses `scan` mode (no journal integration yet). The
  natural next steps are fanotify on Linux and FSEvents on macOS, which need a resident
  process; see `docs/experiments.md`.
- **Output parity with rg.** rg's defaults (hidden files, `.ignore`, binary handling,
  max file size) must be mirrored at index time, or results diverge. The bench gate catches
  this. One case is rg disagreeing with itself: a file whose first NUL lies past rg's first
  64 KiB read prints differently depending on what the same rg thread read before. scout
  defines it as rg with a fresh buffer, and the gate re-checks such files that way (E14).
