# Warm-path ideas after E47

Research notes, not experiments: ranked proposals for cutting **warm** query latency
(the index fully in the page cache) on the small box (2 vCPUs, 4 GiB, `scout serve`), without
regressing 8–12 CPU scale-up. Nothing here was run. The numbers come from the repo's results,
mainly the E42 warm pass (`bench/results/E42-coldio/ab-gap32/1-gap32/warm.json`), E38's
`perf` profiles (`bench/results/E38-ebs/probes/perf.txt`) and E43's candidate counts
(`bench/results/E43-small/candidates.txt`). Every estimate shows its arithmetic so it can be
checked and thrown out. Written 2026-10-29 against the tree at E47. The cold-path
companion is [cold-path-ideas.md](cold-path-ideas.md).

## What the numbers already decide

**1. Most of a broad warm query is lz4 decoding, and the decoder cannot get faster.** E38's
`perf` (CLI, 2 CPUs, the lz4 index cached) put `LZ4_decompress_safe` at 57–63% of
`files_mode`, `literal_common`, `literal_long`, `word`, `alternation` and `case_insensitive`,
37% of `anchored` and 25% of `date_regex`. The same probe decoded the whole 5.4 GB index at
~4.6 GB/s per core, and E38's `codec-probe` put HC6 decode at 4.97 GB/s, at the top of
lz4's range. HC12 decodes only 2.4% faster (5.09) and encodes 4.6x slower (27 against 125
MB/s). So a warm query gets faster only if it **decodes fewer bytes**, or if the **work that
is not decoding** shrinks.

Bytes decoded per query, estimated as p50 × 2 cores × decode share ÷ 4.6 GB/s. This is
approximate: E38's profile is the CLI on the E38 build, and the p50s are `serve` on E42.

| query            | warm p50 | decode share |  decoded | per candidate | 2-core decode floor | not decoding |  output |
| ---------------- | -------: | -----------: | -------: | ------------: | ------------------: | -----------: | ------: |
| literal_long     |   271 ms |        61.3% | ~1.53 GB |        8.3 KB |             ~166 ms |      ~105 ms | 31.4 MB |
| alternation      |      279 |        56.3% | ~1.44 GB |         29 KB |                ~157 |         ~122 | 22.6 MB |
| literal_common   |      176 |        62.7% | ~1.01 GB |         20 KB |                ~110 |          ~66 | 32.0 MB |
| anchored         |      256 |        37.0% | ~0.87 GB |       10.5 KB |                 ~95 |         ~161 |  8.9 MB |
| date_regex       |      387 |        24.5% | ~0.87 GB |         15 KB |                 ~95 |         ~292 | 61.6 MB |
| word             |      137 |        56.8% | ~0.72 GB |         23 KB |                 ~78 |          ~59 | 13.0 MB |
| case_insensitive |      141 |        55.8% | ~0.72 GB |         44 KB |                 ~79 |          ~62 | 12.8 MB |
| files_mode       |       56 |        57.0% | ~0.29 GB |         13 KB |                 ~32 |          ~24 |  1.8 MB |

(literal_long: 271 × 2 × 0.613 = 332 core-ms × 4.6 GB/s = 1.53 GB, ÷ 184,684 candidates =
8.3 KB.)

These are rough, and biased in known directions:

- E38's shares were profiled on the pre-HC index, which decodes ~14% slower than the HC6
  index behind these p50s, so they overstate decode a little.
- They assume both cores busy for the whole query, which idea 3 shows is false during
  planning.
- Raw blocks under 4 KiB are borrowed, not decoded, so bytes _scanned_ per candidate are
  higher than bytes decoded.

Step zero below replaces them with measured numbers.

**2. Better filtering cannot help most of the slow queries.** Their candidates almost all
match: literal_long 184,684 → 182,037 files, literal_common 50,646 → 50,302, files_mode
22,456 → 22,339, alternation 49,040 → 44,213. A filter can help only `date_regex` (10%
precision), `ident_regex` (19%), `word` (52%), `anchored` (57%) and `case_insensitive`
(56%).

**3. The non-decode part has three parts we can name.**

- _Matching._ Teddy is 15–17% on `alternation` and `case_insensitive`, memmem 6–10% on the
  literals, and the lazy DFA **39%** on `anchored`. On `date_regex` the dense DFA is 26%,
  memchr 5.6% and the reverse DFA 3.3% (the meta engine's reverse-inner strategy on `-`).
- _Planning._ On `literal_long`, postings decode is 4.75%, `intersect` 3.7% and `from_utf8`
  2.4%. On `files_mode` they are 3.7%, 2.3% and 2.7%.
- _Output._ An unattributed 10.9% in libc on `date_regex`, the query with 61.6 MB of output,
  is probably memmove. In `serve`, sending the body comes after all of that (idea 1).

**4. Keep rule for every idea below** (E43's, adapted for warm): keep only if the
`serve-ab.sh` warm geomean is ≤ 0.98, no warm query is more than 5% slower in both reps,
cold geomean is ≤ 1.02, and parity is ALL OK on all four profiles (`parity-e30.sh`). Ideas
that touch threading also need an `ab.sh` run at 12 CPUs.

**Step zero (not an experiment).** Add `decoded_bytes` (and `decode_calls`) to
`SearchStats` (`crates/scout-core/src/search.rs`), counted in `NativeShard::decode`
(`crates/scout-index-native/src/lib.rs`), and print them in `--stats`. Every estimate below
then uses a measured number instead of one derived from `perf`. In the same pass, run
`perf record -g` on `date_regex` to place its 10.9% libc. If the cost is TextSink copies,
printer-buffer doubling, or the `out` Vec's growth in `serve`, the last two have a cheap fix
of their own: size buffers from the previous chunk's output (idea 1's `write_vectored`
follow-up also covers it).

## Ranked

### 1. Send the HTTP body with Content-Length in one write

- **Mechanism.** `search_response` (`crates/scout-cli/src/serve.rs`) builds the whole body
  in a `Vec` and returns `Response::from_data(out)`. tiny_http 0.12 then picks
  `Transfer-Encoding: chunked` for any body of 32 KiB or more (`chunked_threshold` default
  32768, `response.rs`), and wraps the socket in `chunked_transfer::Encoder::new`, which
  frames 8 KiB chunks (`encoder.rs`: `with_chunks_size(output, 8192)`). That goes over a
  1 KiB `BufWriter` (`client.rs`), fed by `io::copy`'s 8 KiB stack buffer.
  - The server therefore makes ~one `write` syscall per 8 KiB, plus three user-space copies:
    `Cursor` → stack buffer → encoder buffer → kernel.
  - The bench client (`qbench.py`, `urllib`) parses chunked bodies in Python, chunk by
    chunk (`http.client._read_chunked`: a `readline` and a `_safe_read` per chunk).
  - `date_regex`'s 61.6 MB is ~7,500 chunks.

  Change: send `Content-Length` and one `write_all` of the buffer. The cheap first cut is
  `.with_chunked_threshold(usize::MAX)`: identity encoding, but still 8 KiB writes from
  `io::copy`. The full version uses `Request::into_writer()`, writes the status line and
  headers, then `write_all(&out)`, so the kernel takes the body in socket-buffer-sized
  pieces. A follow-up could write chunk buffers straight from the reorder buffer
  (`write_vectored`) instead of first copying them into `out`. That also saves the `out`
  Vec's growth to 64 MiB: ~15k first-touch page faults on `date_regex`.

- **Estimate.** `smart_case` moves 19.8 MB in 38 ms end to end, so transfer costs at most
  ~1.9 ms/MB. That bounds it at ≤ 117 ms of `date_regex`, ≤ 61 of `literal_common`, ≤ 60
  of `literal_long` and ≤ 43 of `alternation`. A realistic share:
  - 7,500 chunks × (~2–4 µs server syscall ‖ ~4–6 µs Python per chunk) ≈ 30–45 ms on
    `date_regex`. A single write plus a `Content-Length` read is ~memcpy speed, ~10–15 ms.
  - Savings: `date_regex` −20 to −35 ms (0.91–0.95), `literal_common` and `literal_long`
    −10 to −18 (0.92–0.95), `alternation` −8 to −12, `smart_case` −8 to −12 (0.7–0.8),
    `word` and `case_insensitive` −4 to −7.
  - Geomean **~0.96–0.98**. Selective queries are unchanged: their bodies are under 32 KiB
    or a few chunks.
- **Caveat.** Part of the win is the Python client's chunk parsing, which a production
  client may pay less for. Separate the two with a transfer-only probe: serve a saved
  61.6 MB body both ways, then time it from `qbench`'s client and with `curl -o /dev/null`.
- **Risk.** Low. Outputs are byte-identical (`serveab.py` fails on any body difference).
  `into_writer` gives up tiny_http's keep-alive bookkeeping for that response, so send
  `Connection: close`, or use only the threshold variant. No effect on CPU scaling.
- **Measure.** `serve-ab.sh` warm and cold, 2 reps. Also record server CPU per request
  (`cpu.stat` of the cgroup) to show the syscall saving apart from the client.
- **Note.** The bench runs `--max-output-mb 4096` (`serve-ebs.sh`). With the default of 16,
  the five outputs over 16,777,216 bytes (`date_regex`, `literal_common`, `literal_long`,
  `alternation`, `smart_case`) answer 422. Those five carry most of the 0.96–0.98 above, so
  under production defaults the gain is mostly `word` and `case_insensitive` (geomean
  ~0.995). It grows if the cap is raised, or for clients that ask for large bodies.
  Streaming the body during verification would overlap transfer with search, but it
  conflicts with `X-Scout-Matched-Files` and with answering 422 or 503 after the headers
  are sent. Not proposed until those contracts change.
- **Prior art.** tiny_http `Response::with_chunked_threshold`
  <https://docs.rs/tiny_http/0.12.0/tiny_http/struct.Response.html#method.with_chunked_threshold>;
  CPython's chunked reader <https://github.com/python/cpython/blob/main/Lib/http/client.py>.

### 2. A literal candidate-line finder for line-anchored regexes (`anchored`: lazy DFA 39%)

- **Mechanism (a hypothesis grounded in the source; verify first).**
  - grep-searcher's fast path calls `matcher.find_candidate_line(&buf[pos..])` once per
    match, each time on a new slice (`vendor/ripgrep/crates/searcher/src/searcher/core.rs`,
    `find_by_line_fast`).
  - For `^#include "base/\w+\.h"$`, grep-regex skips its own inner-literal line regex,
    because the regex "is accelerated" by its prefix prefilter (`#include "base/`) and has
    no Unicode word boundary (`vendor/ripgrep/crates/regex/src/literal.rs`,
    `InnerLiterals::new`). So it calls the meta regex's `shortest_match`.
  - The lazy DFA runs the prefilter only while it is in a state _tagged_ start
    (`regex-automata-0.4.18/src/hybrid/search.rs`, `if sid.is_start()`). A state is tagged
    only when it is first built as a start state (`hybrid/dfa.rs`, `cache_start_group`, and
    the `old_id.is_start()` carry-over).
  - Every search begins at `Start::Text`, whose look-behind set includes `(?m)^` being
    satisfied. After any non-newline byte, the DFA is in a state with a different
    look-behind set. No search ever _started_ there, so it is never tagged.
  - So after the first `#include "base/` that fails the rest of the pattern (e.g.
    `base/memory/…`, which is most of them), the DFA walks every byte to the next real
    match, without the prefilter.
  - Source check (independent review): the `Text` and `LineLF` start states have look-have
    {StartLF, StartCRLF} (`util/determinize/mod.rs`, ~607–636). A `\n` in the middle of a
    search adds only StartLF (~226), and StartCRLF needs CRLF mode (~237). So no state
    reached mid-search equals a tagged start state.
  - The profile is consistent with this but does not prove it: the lazy DFA takes 39%
    (~200 core-ms), while the confirmations alone would be ~1M lines × ~40 bytes ≈ 40 MB,
    a few ms. Memmem's 4.7% is the prefilter's share of whatever it still scans.
- **Verify first (one command each, no code).**
  - Time `scout search -c '^#include "base/\w+\.h"$'` against
    `-c '#include "base/\w+\.h"$'` (without `^`) on the warm index. The hypothesis predicts
    ~100 ms more for the `^` form.
  - Run `rg` against both on an extracted tree. If rg shows the same gap, this is upstream
    behaviour worth reporting to BurntSushi/ripgrep.
- **Change (simplest first).** Patch the vendored `literal.rs` (~line 72) so that an
  "accelerated" regex whose HIR starts with a line-anchor look-around still gets inner
  literals (`fast_line_regex`). That is one condition in code scout already owns (the
  vendoring decision in design.md). It also makes a clean upstream PR.
- **Change (if more control is needed).** Wrap `RegexMatcher` in a scout matcher that implements
  `grep_matcher::Matcher` by delegation and overrides only `find_candidate_line`. It
  memmem-searches (`memchr::memmem::Finder`, or Teddy via `aho-corasick` for a small set) a
  literal every match must contain, then returns `LineMatchKind::Candidate(offset)`.
  grep-searcher then confirms the line with the full regex (`is_match(&buf[line])`), so the
  result is exact by construction.
  - Take the literal from the HIR the planner already walks (`crates/scout-core/src/query.rs`,
    required/prefix literal sets), or from `regex_syntax::hir::literal::Extractor`.
  - Use it only when the pattern is case-sensitive, the literal has at least 3 bytes, and
    the HIR starts with a line anchor (`(?m)^`). Unicode `\b` is excluded: grep-regex
    already builds inner literals for it (`literal.rs` ~72–73), and the two prefilters
    must not stack. An ASCII `(?-u:\b)` start may share the problem; check it with the
    same one-command test before including it.
  - Locations: `build_matcher` and the `matcher: &RegexMatcher` signatures in
    `crates/scout-core/src/search.rs`, `search/bounded.rs` and `ranked.rs`.
- **Estimate.** If the DFA share falls from 39% to ~3% (confirmations only): 0.36 × 512
  core-ms ≈ 185 core-ms ≈ 90 ms wall, so `anchored` 256 → **~165 ms (0.65x)**. Geomean
  ≈ 0.976 from this one query. The class matters for agents (`^import`, `^func `,
  `^class `), not only the bench.
- **Risk.** Low to moderate. Parity is guaranteed by confirmation, but the trigger must not
  catch patterns where the engine already does better: an alternation of literals, `-i`
  (the case-fold set explodes). A regex whose required literal is common but whose matches
  are rare must not get slower. Check `ident_regex`, `crbug_regex`, `llvm_regex` and
  `rare_regex` in the A/B. Neutral to positive at 12 CPUs (the work per byte falls).
- **Measure.** `serve-ab.sh` warm, plus `perf record` on `anchored` to see `find_fwd`
  drop. Parity gate.
- **Prior art.** rg's inner-literal line optimization
  <https://github.com/BurntSushi/ripgrep/blob/master/crates/regex/src/literal.rs>;
  regex-automata on start-state specialization and prefilters
  <https://docs.rs/regex-automata/0.4.9/regex_automata/hybrid/dfa/struct.Config.html#method.specialize_start_states>;
  `Matcher::find_candidate_line`
  <https://docs.rs/grep-matcher/latest/grep_matcher/trait.Matcher.html#method.find_candidate_line>;
  ripgrep's design notes on literal extraction <https://blog.burntsushi.net/ripgrep/>;
  livegrep also confirms candidate lines with the full regex
  <https://blog.nelhage.com/2015/12/regular-expression-search-with-suffix-arrays/>.

### 3. Plan per shard, not per repo

- **Mechanism.** `plan_chunks` fans `plan_repo` + `chunk_repo` out with
  `parallel_indexed(opts.threads, repos.len(), …)`, and `plan_repo` loops over that repo's
  shards serially (`search.rs`). Verification (phase 2) starts only after every repo is
  planned.
  - On chromium + llvm-project (+ the tiny sync repo), chromium holds most of the 51
    shards, so one core plans chromium while the other finishes llvm-project and idles.
  - Phase 1 includes postings decode, `intersect`, unit → doc mapping, the shape filter
    over every doc (`date_regex`: ~697k `shape_mask` checks), and `chunk_repo`'s
    `meta()` (with `from_utf8`) per candidate.
  - Change: parallelize over (repo, shard) pairs and assemble each repo's `RepoPlan` in
    shard order, so chunk order and output do not change.
- **Estimate.** `literal_long` planning ≈ 4.75 + 3.7 + ~1 (part of `from_utf8`) ≈ 9.5% of
  542 core-ms ≈ 51 core-ms, ~85% of it chromium's. Serial: ~43 ms wall. Balanced over 2
  cores: ~26. So **−15 to −20 ms (0.93–0.95)**.
  - `files_mode`: 3.74 + 2.26 + 2.67 ≈ 8.7% of 112 = ~10 core-ms, −4 ms (0.93).
  - `date_regex`: `shape_mask` does not appear in its profile, whose lowest listed entry
    is 3.26% (~25 core-ms). So planning there is under ~25 core-ms, and the saving is at
    most ~12 ms (≥ 0.97).
  - Selective queries: ~0 (planning is a few ms).
  - Geomean ~0.985. The win grows with cores (shards ≫ repos), the one idea here that
    clearly helps 8–12 CPUs.
  - A bigger version is E16's stated next step: one pool that plans and verifies, so
    verification of planned shards starts while others are still planning. That also hides
    planning entirely, but it is a larger change to the reorder buffer.
- **Verify first.** `scout search --stats` already reports `postings_time` (summed thread
  time) and `elapsed`. On the warm big index, `postings_time` against the planning wall
  time shows the idle core.
- **Risk.** Low. The existing tests `output_does_not_depend_on_chunking_or_threads` and
  `ranking_does_not_depend_on_chunking_or_threads` guard order. Strict freshness's overlay
  is per repo and must be computed once, before the shard tasks. The bounded search
  (`search/bounded.rs`) plans shard by shard on purpose and stays as it is.
- **Measure.** `serve-ab.sh` warm and `ab.sh` at 12 CPUs.
- **Prior art.** Zoekt searches shards, not repos, in parallel
  <https://github.com/sourcegraph/zoekt/blob/main/search/shards.go>.

### 4. Skip `MADV_WILLNEED` when the content is already resident

- **Mechanism.** Every verifier queues each ~4 MiB block of candidates with
  `NativeShard::prefetch` → `coalesce(…, PREFETCH_GAP)` → one `madvise(MADV_WILLNEED)`
  per joined range (`crates/scout-index-native/src/lib.rs`, `prefetch` / `will_need`).
  - On a warm index every call does nothing useful, but still pays a syscall and
    `force_page_cache_readahead`'s page-cache lookup for every page it covers.
  - `serve` keeps its maps alive, so pages it touched before have PTEs, and `mincore`
    reads residency from page tables cheaply.
  - Change: before advising a block, call `mincore` once over the block's span (first
    range's start → last range's end). Skip the WILLNEED calls when every page is
    resident. (Alternative: compare `getrusage(RUSAGE_THREAD).ru_majflt` across the
    previous block, with no syscall over the range.)
- **Estimate.** The cold read count after E42's coalescing bounds the calls per broad
  query from above (one range longer than the read-ahead window becomes several reads):
  `alternation` 8,839, `literal_common` 4,066 (`cold-io-gap32.txt`).
  - At ~1 µs a call plus ~30 ns per covered page (alternation ≈ 1.44 GB / ~2.8 ratio ≈
    510 MB ≈ 125k pages ≈ 4 ms), that is ~13 core-ms, about 6 ms wall on 2 cores: **~2%**
    of `alternation`.
  - E42's data points the same way but does not agree in size. Halving the calls (4 → 32
    KiB gap) gave warm geomean 0.978 (16 KiB: 0.990), about twice what this model gives for
    removing _every_ call. So either a call costs more than modelled, or E42's warm number
    is partly noise.
  - Expect 1–3% on the broad queries; the `strace` gate below decides.
  - `mincore` reads PTEs only where the process has mapped the page. Elsewhere it falls
    back to a page-cache lookup per page, like WILLNEED does. The saving is then only the
    call count, not the per-page work.
  - `mincore` cost: ~360 blocks × ~2 µs ≈ 1 ms on `literal_long`.
- **Verify first.** Count and time the calls on a warm serve pass:
  `strace -f -c -e trace=madvise` or `perf trace -s` against the server pid while `qbench`
  runs warm. If the total is under ~1% of the pass, drop the idea.
- **Risk.** Low to moderate. On a partly evicted index, a block judged resident is never
  partly evicted (the check is the whole span). Cold is unchanged except for one `mincore`
  per block (≪ one read). Must show cold geomean ≤ 1.02. Unix only, like `will_need`.
- **Measure.** `serve-ab.sh` warm and cold. `cold-io.sh` at 3000 IOPS to confirm reads and
  bytes are unchanged.
- **Prior art.** `mincore(2)` <https://man7.org/linux/man-pages/man2/mincore.2.html>;
  readahead implementation <https://elixir.bootlin.com/linux/latest/source/mm/readahead.c>.

### 5. A wider numeric shape filter (`date_regex`: 10% precision)

- **Mechanism.** Each doc row carries a 32-byte Bloom of 4-character shapes with at least
  two digits, 2 bits each in 256 (`crates/scout-core/src/shape.rs`). `date_regex`
  (`\d{4}-\d{2}-\d{2}`) still keeps 56,542 candidates for 5,699 matching files.
  - `0000` and `000-` are in nearly every file with a year in its header.
  - A numeric-heavy file saturates 256 bits.
  - Change (format v10): add a second 32-byte Bloom of 8-character shapes with at least
    four digits (`0000-00-`, `000-00-0`, …), or enlarge the filter. `ShapeQuery` then
    requires the longer shapes when the regex HIR implies them.
- **Estimate (conditional).** The 51k false candidates cost decode + DFA, about 50% of
  `date_regex`'s CPU (24.5 + 26%), but only in proportion to their bytes.
  - 5.7k matching files print 61.6 MB, so the matched files may hold most of the 0.87 GB
    decoded.
  - With f = the false candidates' share of decoded bytes: saving ≈ f × 0.5 × 387 ms.
  - f = 0.3 → −58 ms (0.85); f = 0.7 → −135 ms (0.65).
  - Index: +32 bytes × 697,675 docs = +22 MB (+0.9% of 2,580 MiB), all of it in the heap
    tables (`--table-mb`).
- **Verify first.** An offline probe over the index (an `examples/` binary like
  `codec-probe`) that sums decoded bytes of candidates, matched against not matched, for
  `date_regex` (and `ident_regex`, `word`), and simulates precision for 6- and 8-char
  shapes. The probe sets f and decides.
- **Risk.** Moderate: a format change. Planning gains one more Bloom check per doc (~697k
  for `date_regex`; idea 3 overlaps it). The cold side wins too (fewer content bytes read).
- **Measure.** The probe, then `serve-ab.sh` warm and cold with an index per arm
  (`run-e43.sh`'s shape), then parity.
- **Prior art.** The general case is n-gram selectivity in code search, Cox's trigram index
  <https://swtch.com/~rsc/regexp/regexp4.html>; GitHub's sparse grams pick longer, rarer
  grams for the same reason
  <https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/>.

### 6. Decode and verify identical content once (probe first)

- **Mechanism.** A broad query decodes and scans every copy of a file. Store a content
  hash per doc row (16 bytes; R2's ranked pass already hashes content with xxh3). At query
  time, memoize per search: hash → "no match", or the rendered matched lines without the
  path. Each copy then prints the cached lines under its own path, with no decode and no
  match.
  - Within a shard, the writer could point duplicate rows at one stored copy, which also
    saves bytes (cold-path-ideas.md idea 3). The query-time memo also covers copies across
    shards and repos.
- **Estimate.** Proportional to the duplicate share of candidate bytes, d. Saving ≈ d ×
  (decode + match share, ~65–75%) on the broad queries.
  - d = 5% → ~0.965 on them. d = 20% → ~0.86.
  - For a hosted index of a GitHub org (forks, vendored copies), d is likely much higher.
    GitHub reports 115 TB of code reduced to 28 TB of unique content after deduplication.
- **Verify first.** Hash every doc of the big index offline and report duplicate bytes
  overall and among the broad queries' candidates. No format change is needed to know.
- **Risk.** Moderate: memo memory, bounded by output bytes since cached lines are what gets
  printed anyway. Line numbers and `-m` are per file, but identical content gives identical
  per-file results, so exactness holds. Reader-mode docs stay outside it. No CPU-scaling
  risk.
- **Measure.** The probe, then `serve-ab.sh` and parity.
- **Prior art.** GitHub Blackbird shards by blob id and indexes unique content once
  <https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/>;
  file-level clones on GitHub (Lopes et al., DéjàVu, OOPSLA 2017)
  <https://dl.acm.org/doi/10.1145/3133908>.

### 7. Search each candidate only for the alternation branches its unit can hold

- **Mechanism.** For a case-sensitive alternation of literals (`TODO|FIXME|HACK`), a
  candidate unit's postings already prove which branches it can contain. A unit missing a
  trigram of `FIXME` cannot contain `FIXME`. Most candidates hold only one branch's
  trigrams.
  - Change: the planner tags each candidate with a branch bitmask (from the OR's operand
    lists, at unit level).
  - Verification uses a single-needle `memmem` when one branch survives, else Teddy over
    the surviving set (matchers cached per mask).
  - Exact: lines matching an absent branch cannot exist in that unit.
- **Estimate.** Teddy is 16.7% of `alternation` ≈ 93 core-ms over ~1.44 GB. A
  single-needle memmem runs ~1.9x Teddy's speed here: memmem's 10% of `literal_common` over
  ~1.0 GB is ≈ 35 core-ms per GB, against Teddy's ≈ 65.
  - If ~70% of candidates are single-branch: 0.7 × 93 × (1 − 35/65) ≈ 30 core-ms ≈ −15 ms
    wall → `alternation` **~0.95**.
  - Also `case_insensitive` (Teddy 15%) if the fold variants are grouped per branch, which
    is harder; skip it at first.
- **Risk.** Moderate: planner state per candidate (`Candidate` gains a mask), and the OR
  plan must keep its operands' lists instead of only their union. No CPU-scaling risk.
- **Measure.** `serve-ab.sh` warm; parity.
- **Prior art.** Zoekt evaluates a per-document match tree that prunes branches whose
  ngrams are absent <https://github.com/sourcegraph/zoekt/blob/main/index/matchtree.go>;
  Teddy <https://github.com/BurntSushi/aho-corasick/tree/master/src/packed/teddy>.

### 8. Finer posting units for docs under 64 KiB (simulate first)

- **Mechanism.** A doc of ≤ 64 KiB is one unit and one block, so it is decoded whole even
  when its only possible match is in the first lines. That is `literal_long`'s license
  header, and `files_mode`'s `#include <vector>` (where `-l` can stop early but only at
  block granularity, E33).
  - 16 or 32 KiB units would let E32's candidate-unit reads skip the rest.
- **Estimate.** Bounded by bytes per candidate: 8.3 KB on `literal_long`, 13 KB on
  `files_mode`, 20 KB on `literal_common`. Most candidates are already smaller than 16 KiB,
  so the saving is the tail of 16–64 KiB docs, likely 10–30% of decoded bytes on
  header-literal queries.
  - Costs: more postings. E4 measured a 16 KiB arm: postings 2.06x against 1.50x at 64
    KiB (experiments.md, E4 notes), so this revisits E4's rejected arm, and the new angle
    has to be the decode bytes saved under lz4, which E4 (raw content) could not see. Also
    a second decoder call per split doc (E43 and E44 show per-call cost matters
    warm), and a worse lz4 ratio across block boundaries (cold).
- **Verify first.** An offline simulation like E40's: for each query, decoded bytes at
  64/32/16 KiB units, plus postings growth. Drop the idea unless decoded bytes fall by
  more than ~20% on at least three broad queries.
- **Risk.** High: a format change on both sides of E43/E44's trade.
- **Prior art.** Zoekt and Blackbird index per document but keep positions to localize
  matches. Scout's E20 (positional masks, rejected) is the posting-bytes warning.

### 9. Per-candidate trims (last; E41 says expect little)

- **Mechanism.** `meta()` decodes the path with `from_utf8` (2.4–2.7% of `literal_long` and
  `files_mode`) and is called 4 times per candidate: `chunk_repo`, `want`, `queue_block`,
  `visit_docs`, though three of them need only `size`. Each candidate also gets two
  `reader_mode` binary searches, a shared `produced.fetch_add` across verifier threads per
  matched doc, and `Instant::now()` twice.
  - Change: a `size(doc)` port method, `meta` once per candidate, the reader-mode flag
    computed once in `want`, and the output count added per chunk every ~64 KiB of output
    instead of per doc.
- **Estimate.** ~2–4% on `literal_long` and `files_mode`, ~0 elsewhere. This is E41's
  rejected class applied to the plain search. E41 found such trims unmeasurable in wall
  time on 2 CPUs and warned that profile percentages overstate them. Gate: write no code
  until a `perf stat` of a quick prototype shows a cycles drop beyond the ±2% noise.
- **Risk.** Low.
- **Prior art.** E41 in [experiments.md](../experiments.md).

## Not recommended, with the reason

- **A decoded-block cache.** The broad queries decode 0.3–1.5 GB each, more than the box's
  free memory once the 2.6 GB index is cached, so an LRU over them hits ~0% (a scan). It
  would also inflate the bench's warm rounds, which repeat the same queries.
- **Storing hot docs raw.** E29, E43 and E44 already traded warm decode against index
  size, and on this box the size wins.
- **Partial decode of a block's prefix** (for `-l`, `-m`, bounded search). lz4-sys does not
  bind `LZ4_decompress_safe_partial`, and binding it is new FFI outside the two places
  `unsafe` may live. A writer-side split of the first block is idea 8's cost with a smaller
  benefit (only summary modes).
- **Searching the compressed stream.** LZ77-aware matching can skip re-matching copied
  spans, but every matched line must still be materialized and printed, and matching is
  only 6–17% of the broad queries.
- **Hyperscan or Vectorscan.** C/C++ FFI (the unsafe policy). E38 measured the regex
  crate's Teddy beating Aho-Corasick here already.
- **Skipping line counting.** `-n` is part of rg parity and the bench. Counting is a
  memchr-speed pass: memchr `One::find_raw` is 5.6% of `date_regex` and 2.1–2.4% of three
  other queries, likely line-terminator scanning. Only a rough ~2–5% is in play, and it
  cannot be removed without dropping `-n`.
- **HC9 or HC12.** Decode is 0.6–2.4% faster, but encode is 1.6–4.6x slower (E38
  `codec-probe`).
- **More verifier threads than CPUs.** The work is CPU-bound once warm, so the 2-CPU quota
  is the limit.
- **Finer or coarser verification chunks.** Checked, and not a lever:
  - A ~4 MiB chunk of candidates takes ~1–2 ms to decode and match (4 MiB at 4.6 GB/s ≈
    0.9 ms, plus matching), so imbalance at the end of 2 workers is at most ~2 ms.
  - The printer's copy into the output is at most ~61 MB of memcpy, overlapped with
    verification.
  - Only planning has a granularity problem (idea 3).

## Order of work

Step zero (`decoded_bytes` in `--stats`), then 1 and 2: cheap, exact by construction, and
each has a one-command check first. Then 3, which also helps 8–12 CPUs, then 4 (only if
`strace -c` shows it matters). Then the probes for 5, 6 and 8, each a small offline example
binary, and build only what the probe justifies. Then 7, then 9.
