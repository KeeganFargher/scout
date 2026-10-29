# Cold-path ideas after E47

Research notes, not experiments: ranked proposals for cutting **cold** query latency on
the small box (2 vCPUs, 4 GiB, gp3 at 125 MB/s and 3000 IOPS). Nothing here was run. The
numbers come from the repo's results, and the arithmetic is shown so each estimate can be
checked and thrown out. Written 2026-10-29 against the tree at E47.

## What the numbers already decide

**1. Cold broad queries are bound by bytes at the volume cap, not by reads or CPU.** After
E42, `cold-io.sh` with and without the IOPS cap put every query within 8% (E43), and one
model fits every row of the E42 gap sweep (`bench/results/E42-coldio/cold-io-gap*.txt`):

    t ≈ max(bytes / 119.2 MiB/s, reads / 3000)        (125 MB/s = 119.2 MiB/s)

| query, gap     | bytes term         | reads term         | model | measured |
| -------------- | ------------------ | ------------------ | ----- | -------- |
| alternation 4  | 490/119.2 = 4.11 s | 29,885/3000 = 9.96 | 9.96  | 9.96     |
| alternation 16 | 559/119.2 = 4.69   | 15,701/3000 = 5.23 | 5.23  | 5.30     |
| alternation 32 | 685/119.2 = 5.75   | 8,839/3000 = 2.95  | 5.75  | 5.67     |
| lit_common 4   | 311/119.2 = 2.61   | 17,331/3000 = 5.78 | 5.78  | 5.64     |
| lit_common 32  | 412/119.2 = 3.46   | 4,066/3000 = 1.36  | 3.46  | 3.53     |

The per-query table (`E42-coldio/ab-gap32/1-gap32/summary.txt`) says the same: alternation
693.8 MiB / 5.812 s = 119 MiB/s, literal_long 113, literal_common 116, date_regex 123. All of
them are at the cap. Cold time is ~15–20x warm time, so decode speed hardly matters cold.
That leaves two ways to make a cold query faster: **read fewer bytes**, or **get a faster
volume**. More concurrency cannot help, because the pipe is already full (see "Not
recommended").

**2. Better filtering cannot help the three slowest queries.** Their candidates almost all
match (`E43-small/candidates.txt`): literal_long 184,684 → 182,037 files matched,
literal_common 50,646 → 50,302, files_mode 22,456 → 22,339. Filtering can only help where
candidates far outnumber matches: ident_regex 22,206 → 4,241 (296 MiB), date_regex 56,542 →
5,699 (248 MiB), rare_regex 5,781 → 322 (110 MiB), anchored 83,223 → 47,569 (406 MiB), word
31,758 → 16,602 (423 MiB), case_insensitive 16,139 → 9,108, selective_long 106 → 0.

**3. Storage changes are judged by the E43/E44 rule.** Keep only if cold geomean ≤ 0.95,
warm geomean ≤ 1.05, and no warm query more than 10% slower in both reps of `serve-ab.sh`.
E43 (small blocks compressed) and E44 (per-shard dictionary) both cut bytes and both
failed on warm. Each idea below is graded against this rule.

**4. Which "MiB read" to trust.** `qbench.py`'s column and `cold-io.sh`'s `io.stat` disagree
on small queries: selective_long reads 24.5 against 15 MiB, literal_none 9.0 against 0.4.
qbench seems to count ~9 MiB that is not index reads. Size estimates on `io.stat` (cold-io).

## Ranked

| #   | idea                                                                | kind            | expected cold win                                                             | warm / size risk                                      |
| --- | ------------------------------------------------------------------- | --------------- | ----------------------------------------------------------------------------- | ----------------------------------------------------- |
| 1   | Measure the real bound; maybe move the index to instance NVMe       | deployment      | NVMe unmeasured (fio it); the EBS baseline may be 37–65% _below_ the harness  | none                                                  |
| 2   | Byte-budgeted prefetch coalescing (adaptive gap)                    | read path       | −14/15% on alternation, literal_common, word; geomean ~0.965 at gp3 caps      | none, no format change                                |
| 3   | Within-shard content dedup                                          | format, writer  | proportional to duplicate bytes (unmeasured; probe first)                     | none or better; index smaller                         |
| 4   | Physical content order clustered by file type                       | format, writer  | ≤ the gap filler, 25–34% of bytes on broad queries; realistically half        | none expected; deterministic order                    |
| 5   | Cold-aware decode budget (E22 priced for disk)                      | planner         | selective_long 155 → ~40–60 ms if the full AND empties it; literal_ident some | warm only if applied while warm; gate it              |
| 6   | Pack small docs into shared lz4 chunks (Lucene stored-fields style) | format          | ~−20% stored content → broad cold ~0.85                                       | warm +5–10% on literal_long; borderline               |
| 7   | Over-budget warm-up by heat, and scan-resistant broad reads         | serve           | indexes over the warm budget only                                             | none at the current size                              |
| 8   | Co-location masks in a side array (E20's untried follow-up)         | format, planner | ident/rare_regex, anchored; simulate first                                    | postings ~2x: may push the index past the warm budget |

---

### 1. Measure the bound on the target, then choose the disk

**Mechanism.** The harness models the _volume_ (gp3 125 MB/s, 3000 IOPS). The _instance_
has its own EBS limit. A 2-vCPU, 4 GiB Graviton box (`c7g.large`, `c6g.large`) has an EBS
**baseline of 630 Mbps = 78.75 MB/s and 3,600 IOPS**, with bursts to 1,250 MB/s
(c7g) or 594 MB/s (c6g) for up to 30 minutes per 24 h
([AWS, compute optimized EBS specs](https://docs.aws.amazon.com/ec2/latest/instancetypes/co.html)).
Once the burst budget is spent, the instance, not gp3, is the bottleneck. The burstable
`t4g.medium` / `t3.medium` (2 vCPU, 4 GiB) are lower still: **43.38 MB/s and 2,000 IOPS**
baseline, 260.62 MB/s burst
([AWS, general purpose EBS specs](https://docs.aws.amazon.com/ec2/latest/instancetypes/gp.html)).
The `d` variant,
`c7gd.large` (2 vCPU, 4 GiB), adds a **118 GB local NVMe** (33,542 random-read IOPS on the
same page).

**Arithmetic.** At the c7g baseline, alternation's 693.8 MiB (727 MB) takes 727 / 78.75 =
**9.2 s**, not 5.8. On a t4g.medium it takes 727 / 43.38 = **16.8 s**. During burst, it is
capped by the volume (gp3 125 MB/s unless provisioned higher), not the instance. So paying
for gp3 throughput above 125 MB/s helps only while the instance is bursting. That is the
case at boot, which is when E47's warm-up runs.

On NVMe, the index (2.58 GiB, 2.8 GB) can be copied from the gp3 volume at boot. That read
is volume-bound: 2.8 GB / 125 MB/s ≈ **22 s** (E45 estimated ~21 s for the same read), or
~5.6 s with gp3 provisioned at 500 MB/s inside the c7g burst. The instance store is lost on
stop, but the index is a rebuildable cache, as the design already treats the working
copies. After the copy, cold reads run at NVMe speed. AWS publishes the instance store's
IOPS but not its throughput, so measure it with `fio` before estimating.

**Risk.** None to the code. It is a cost and operations decision. The instance store needs
a copy-on-boot step (or a rebuild).

**Measure.** On the real instance: `fio` random reads of 16 / 128 KiB at queue depth 1–8
on the gp3 volume, cold and after an hour of load (to burn the burst budget). Then run
`cold-io.sh` with `ebs-run.sh` caps set to the measured baseline (`… 79 3000`) to see the
cold table at that bound. Also record `read_ahead_kb` (README: the cloud default is
128 KiB). This sets the constants that idea 2 needs.

---

### 2. Byte-budgeted prefetch coalescing

**Mechanism.** `PREFETCH_GAP` is a global 32 KiB (`scout-index-native/src/lib.rs:110`).
E42 chose it while queries were bound by reads. Now they are bound by bytes, and the gap
fills are paid in bytes: from 4 to 32 KiB, alternation read +40% (490 → 685 MiB) and
files_mode +52% (160 → 244). A read-ahead block knows all its ranges before it issues a
`WILLNEED`. So it can pick its own threshold: join gaps smallest first, and stop once
`reads × (BW / IOPS) ≤ bytes`, which is where the two terms of the model meet. On gp3 that
is 125 MB/s ÷ 3000 = 41.7 KB a read. At the c7g baseline it is 78.75 MB/s ÷ 3000 =
26 KB. Take BW and IOPS from flags (`--disk-mbps`, `--disk-iops`) set from idea 1's
measurement. Joining smallest gaps first is what a uniform threshold does, so the sweep
interpolation below is a fair estimate of the per-query optimum.

**Arithmetic** (model, linear between the swept gaps, compared with the model at 32 KiB):

- alternation: the bytes term rises 4.69 → 5.75 s and the reads term falls 5.23 → 2.95 from
  16 to 32 KiB. They meet at ~18.6 KiB, t ≈ 4.86 s against 5.75: **−15%**.
- literal_common: they meet at ~14 KiB (between 4 and 16), t ≈ 2.93 against 3.46: **−15%**.
- word: ~18 KiB, 2.97 against 3.47: **−14%**. ident_regex −6%, crbug_regex −3%,
  case_insensitive −2%, files_mode −2%, count_mode −1%, the rest ~0.
- Geomean over 18: exp((3·ln 0.85 + ln 0.94 + 4·ln 0.98)/18) ≈ **0.965**.
- At the c7g baseline (78.75 MB/s), the balance moves to ~11 KiB. Alternation goes from
  max(727/78.75, 2.95) = 9.2 s at 32 KiB to ~7.0 s (**−23%**). The right gap depends on the
  box, so it should be a measured constant, not a literal.

**Risk.** No warm reads, so no warm risk. No format change. The caveat from E42 still
holds: gp3 merges sequential I/O up to 256 KiB into one operation
([EBS I/O characteristics](https://docs.aws.amazon.com/ebs/latest/userguide/ebs-io-characteristics.html)),
while `io.max` counts every bio. The reads term may be pessimistic on real gp3, which would
push the optimum gap smaller still.

**Measure.** `cold-io.sh "3000 max"` for each arm (time, reads, bytes), then `serve-ab.sh`
for the rule. Re-run at the caps from idea 1.

**Prior art.** The same trade-off as readahead sizing. Linux `force_page_cache_ra` issues
WILLNEED in `ra_pages` chunks. Coalescing under a cost model is the standard
"read-merge" in LSM/Parquet readers, for example Arrow's `ReadRangeCache` with
`hole_size_limit` / `range_size_limit`
([arrow io/caching.h](https://github.com/apache/arrow/blob/main/cpp/src/arrow/io/caching.h)).

---

### 3. Within-shard content dedup

**Mechanism.** Store identical documents once per shard. In v9 a doc's content offset
(`row + 0`) is a free pointer, and its block range is taken from the next row's first
block (`stored()`, lib.rs:913). So a copy can get its own block rows (12 bytes each, the
same lengths and line counts) with its content offset pointing at the first copy's bytes.
The reader only bounds-checks the offset, so the format change is small: the writer
hashes each doc (xxh3-128, which R2 already computes for ranking) and keeps a map from
hash to offset per shard. Cold, the second copy's verification finds its pages already
cached, so the bytes are read once. Step two, optional: within a chunk, cache the
verification result per content offset and print it under each path. That also saves
the warm decode and match.

**Why on gp3.** Fewer distinct bytes for broad queries. Duplicates cluster by directory,
and path order puts them in the same shard.

**Arithmetic.** Unmeasured, so this is a sensitivity estimate: every 10% of a broad query's
stored bytes that are duplicates is ~10% of its cold time (fact 1). GitHub reported 115 TB
→ 28 TB of unique content across all of GitHub, but that figure includes forks and delta
indexing ([GitHub blog](https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/)).
A single monorepo will duplicate much less, so do not assume that ratio. Probe first,
for free: `git -C chromium ls-tree -r -l HEAD`, sum the bytes of blob ids that occur more
than once. This over-approximates slightly, since it includes files rg ignores. Or count
distinct content hashes among each query's matched files from a `--rank-features` dump.

**Risk.** Warm: none from storage dedup (the same decode work), better with step two. Size:
smaller. Correctness: the output for each path is unchanged, and the parity gate covers it.

**Measure.** The probe, then `run-e43.sh`-style: build both indexes, run parity, then
`serve-ab.sh` and `cold-io.sh`.

**Prior art.** Blackbird shards by git blob id to avoid duplication (link above). livegrep
dedups identical _lines_
([Elhage](https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/)).
That is a bigger lever (one license line instead of 182k) but a different architecture.

---

### 4. Physical content order clustered by file type

**Mechanism.** Doc ids stay in path order, so output order, units and markers are
unchanged. The writer only lays out the _content bytes_ in another order, sorted by
(extension class, path), for example C/C++, then tests and generated files. Content
offsets are already per-row pointers, and nothing searches the doc table by offset or
path (checked: the only `binary_search`es are the marker lists, the fence and freshness).
Queries are language-skewed (`#include`, `DCHECK`, `TODO(crbug…)`, `dyn_cast`), so their
candidates become denser on disk. The same read count then costs fewer gap bytes, or the
same bytes cost fewer reads. `prefetch` must sort a block's ranges by offset before
`coalesce`; `coalesce` already starts a new run for an out-of-order range, so today that
would only cost reads, not correctness.

**Arithmetic.** The most this can save is the gap filler, which is bytes at gap 32 minus
bytes at gap 4 (E42 sweep): alternation 195 MiB of 685 (28%), literal_common 101/412 (25%),
word 116/414 (28%), files_mode 84/244 (34%), count_mode 59/209 (28%), crbug 72/224 (32%),
ident_regex 72/287 (25%). If clustering halves the filler at the same read count,
broad cold queries save **12–17%**. This stacks only partly with idea 2: both spend the
same filler.

**Risk.** Warm: the same bytes are decoded, so no change is expected. Indexing: one sort
per shard, deterministic, so shards stay byte-identical across builds. It is a format
change only in that offsets become non-monotonic. It pairs with idea 6, where similar
neighbours compress better.

**Measure.** Simulate first, offline, as E40 did. Dump each query's candidate `Want`s with
stored offsets and lengths (an `--stats` extension) and replay `coalesce` over both layouts
to count bytes and reads. Apply the model from fact 1. Build only if the simulation
predicts ≥ 10% on the broad queries.

**Prior art.** Zoekt orders documents in a shard by rank, with tests and vendored files
last (`sortDocuments` in
[zoekt builder.go](https://github.com/sourcegraph/zoekt/blob/main/index/builder.go)).
Lucene's `BPIndexReorderer`, recursive graph bisection
([Lucene](https://lucene.apache.org/core/9_8_0/misc/org/apache/lucene/misc/index/BPIndexReorderer.html);
Dhulipala et al., KDD 2016, [arXiv 1602.08820](https://arxiv.org/abs/1602.08820)), clusters
documents that share terms, which is exactly "candidates of one query sit together".
Blackbird ingests in a similarity order (GitHub blog above).

---

### 5. Cold-aware decode budget

**Mechanism.** E22 stops decoding an AND's lists once decoding costs more than verifying,
with a verified unit priced at `VERIFY_COST_POSTINGS` = 4096 decoded postings
(`search.rs:1350`). That price is CPU, measured warm. Cold, a candidate unit costs a disk
read: about 20 KiB stored (64 KiB ÷ 3.2) ≈ 0.16 ms of bytes plus a share of an IOP. A
postings list for a rare trigram is a few KiB. When content is not resident, price
verification by its stored bytes at disk speed. Decoding more lists then cuts false
positives before any content is read.

**Arithmetic.** selective_long (`kMaxNumberOfPartitionsForWebRtcAudio`, 34 trigrams): 106
candidates, 0 matches, 15 MiB / 350 reads, 155 ms cold (`io.stat`). If decoding every list
empties the AND, cold falls to the postings reads: **~40–60 ms**. literal_ident
(`RenderFrameHostImpl`, 2,342 → 729, 55 MiB, 560 ms): its false positives probably contain
every trigram (`RenderWidgetHostImpl` + `RenderFrameHost`), so expect little there.
Check with `--stats` with the budget forced off before building anything.

**Risk.** Warm: E22 measured decoding everything as 2–5% slower. Use the disk price only
while the generation is not fully resident. E47's walker knows whether it read the whole
index, so this costs nothing to tell. Size: none.

**Measure.** `--stats` per query with `VERIFY_COST_POSTINGS` very high (candidates for an
exhaustive AND), then `serve-ab.sh` (cold and warm). Most of the gain is on selective
queries, which agents send.

**Prior art.** Zoekt picks the two rarest ngrams
([indexdata.go](https://github.com/sourcegraph/zoekt/blob/main/index/indexdata.go)).
The general idea of cost-based conjunction ordering with I/O pricing is classic in IR
(Lucene `ConjunctionDISI` leads with the lowest cost iterator).

---

### 6. Small docs packed into shared lz4 chunks

**Mechanism.** Docs under 4 KiB are stored raw today. They are 9.6% of content but 30% of
stored bytes (`E43-small/dict-probe.txt`). Pack consecutive small docs (ideally clustered
by type, idea 4) into ~64 KiB chunks, each one lz4 HC 6 block, so lz4's 64 KiB window sees
the repeated headers across files. A reader decodes a chunk once per chunk and verifies
every candidate doc in it in place, with no copy. That avoids E44's decode-and-copy of
every doc and its dictionary prefix.

**Arithmetic.** In the 615 MiB sample, small docs are 59 MiB stored raw (30% of 195.3).
At E38's measured 3.2x for large blocks, packed is ~18 MiB, so −41 MiB, **−21% stored
content**. That sits between E43's −15% (166.1) and E44's −24% (148.2). E43's −15% bytes
gave literal_long 0.842 cold, so expect broad cold ~0.82–0.85 and geomean ~0.93–0.95.

**Risk.** This is the E43/E44 trade again. Bytes that were borrowed for free are now
decoded. Decoding packs runs at large-block speed (4.97 GB/s, against E43's 4.57 aggregate
with small calls), and broad queries amortize a pack over its candidates. But literal_long
warm is ~61% decode (E38), and E43 lost 11% there. Expect +5–10%, at the edge of the rule.
A selective candidate of 2 KiB now reads and decodes a ~20 KiB pack (the 32 KiB gap
already reads neighbours, so cold selective is roughly a wash). **Run only after a probe**:
add a `pack` arm to `dict-probe` that times a literal_long-shaped pass (decode every pack
holding a candidate) against borrowing.

**Prior art.** Lucene stored fields compress many documents per LZ4 chunk (`BEST_SPEED`)
with a shared preset dictionary
([Lucene90StoredFieldsFormat](https://lucene.apache.org/core/9_0_0/core/org/apache/lucene/codecs/lucene90/Lucene90StoredFieldsFormat.html)).

---

### 7. Over-budget warm-up by heat, and scan-resistant broad reads

**Mechanism.** E47 reads nothing when the index is over `--warm-mb`. Once it grows past
~3 GiB (hundreds of repos on 4 GiB), every query is cold-ish again. Two parts:

- (a) Warm in priority order, within budget: postings first, then content blocks ranked by
  a decayed hit count that `serve` keeps per shard (a counter per content block touched
  by verification, persisted beside the catalog).
- (b) After a broad query verifies a range, drop it with `POSIX_FADV_DONTNEED` or
  `MADV_COLD`, so one 700 MiB scan does not evict the selective queries' working set.
  Linux's MGLRU and `FADV_NOREUSE` (6.3+) aim at the same thing
  ([kernel MGLRU doc](https://docs.kernel.org/admin-guide/mm/multigen_lru.html)).

**Arithmetic.** Postings are small next to content for every bench query (E37:
selective_long's 26 MiB was 23.2 content). So (a) pays off only through content heat,
which depends on the workload and is unknown. (b) protects steady state at the E21 cliff
(5.6x slower at 94% cached).

**Risk.** None at the current 2.58 GiB index, which is fully warm.

**Measure.** `warm-budget-check.sh` and `warmup-ab.sh` at 2 GiB (2.58 GiB index over a
1.5 GiB budget), or on the E35 250-repo, 4.88 GiB fixture at 4 GiB, with a
Zipf-distributed query mix. The cold protocol evicts everything, so it cannot see this.

**Prior art.** Elasticsearch `index.store.preload` by file extension
([docs](https://www.elastic.co/guide/en/elasticsearch/reference/current/preload-data-to-file-system-cache.html)).

---

### 8. Co-location masks in a side array

**Mechanism.** This is E20's own "what could still work" note. Store per posting a small
mask of which line-aligned ~8 KiB sub-unit of the 64 KiB unit holds the trigram, in a
separate array. Read the masks only for units that survive the plain intersection, and
only when that leaves many units. Every trigram of a match lies on one line, and a line
lies in one sub-unit, so an AND can require one common sub-unit: sound for the same reason
units are (E4). That catches `class … Delegate` (ident_regex) and `fn … parse_`
(rare_regex), whose literals must share a line. A per-sub-unit start-offset rotation as in
E20 would also catch anchored's `#include "base/` and `-i deprecated`.

**Arithmetic.** It only helps docs bigger than a sub-unit; chromium's average file is 7.5
KiB (5.1 GiB / 697k). If it halves the false positives, bytes scale with candidates:
ident_regex 296 × (4.2 + 9.0)/22.2 ≈ 176 MiB (2.48 → ~1.5 s), rare_regex 110 → ~58 MiB.
These are guesses until simulated bytes-weighted, which E20's simulation was not.

**Risk.** Size is the problem. E20's one byte per posting about doubled postings (+6.5% of
a raw index). On the lz4 index postings are ~800 MiB (2,580 − ~1,570 content − 211
tables), so +~800 MiB would put the index at ~3.4 GiB, over E47's 3 GiB warm budget. The
walker would then read none of it, and every warm number would get worse. Rank last
unless a simulation shows large bytes-weighted gains.

**Prior art.** Zoekt positional ngrams
([design](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)), GitHub's earlier
next-byte / location masks ([Cursor](https://cursor.com/blog/fast-regex-search)).

---

## Not recommended, with the reason

- **zstd, or any slower codec.** zstd -1 against lz4 on Silesia: ratio 2.896 against 2.101
  (1.38x fewer bytes), decode 1,550 against 3,850 MB/s
  ([zstd README](https://github.com/facebook/zstd)). literal_long warm is 61% decode (E38):
  271 ms × (0.39 + 0.61 × 3,850/1,550) ≈ **517 ms, 1.9x**. That fails the rule by a wide
  margin for a ~20–25% byte cut, which ideas 3, 4 and 6 might get without the decode cost.
  `--fast` levels (1,850–2,050 MB/s, ratio 2.1–2.4) lose on both counts.
- **More I/O concurrency or io_uring.** The bandwidth-delay product is 125 MB/s × ~1 ms ≈
  125 KB in flight. Two verifiers each keep a 4–8 MiB content window (~1.3–2.6 MiB stored)
  queued, and `io.stat` shows the cap reached. Concurrency would matter only during the
  c7g burst (1,250 MB/s × 1 ms ≈ 1.25 MB in flight, which is still covered) or on NVMe.
  `READ_AHEAD_BLOCK_BYTES` counting content rather than stored bytes (search.rs:1031) is
  the same story: zero expected gain at the baseline.
- **Reading less inside a candidate (line or offset maps).** E32 already reads only a
  large doc's candidate units. Going finer needs either positions (idea 8's size problem)
  or decoding part of an lz4 block, and the slowest queries are made of small files
  anyway. literal_long reads 571 MiB over 182,037 matched files ≈ **3.2 KiB per file**,
  which is one block each, so there is nothing inside a candidate to skip. files_mode
  (`-l`) could prefetch only the first candidate unit of a multi-unit doc, but `#include
  <vector>` sits at the top of a file, so its other units are rarely candidates.
- **Faster decode or matching for cold.** Cold is 15–20x warm (literal_long 5.1 s against
  271 ms), so CPU is not the bound.
- **Full positional index / suffix arrays.** 3–8x the corpus in index size, which is
  outside 4 GiB (design.md research notes).
- **Bloom per block** for trigrams or quadgrams. A 64 KiB unit holds ~10–20k distinct
  grams; 1% false positives needs ~10 bits a key, ~25 KB per unit. That is bigger than the
  content it would save reading.

## Order of work

1 (measure the box) and 2 (adaptive gap) first: they are cheap, carry no risk, and fix the
constants everything else depends on. Then the probes for 3 (`git ls-tree`) and 4 (offline
coalesce replay), which cost no build. Build whichever predicts more. 5 is a small planner
change worth one A/B. 6 and 8 only after their probes clear the warm rule.
