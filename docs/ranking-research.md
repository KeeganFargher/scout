<!-- Research report gathered 2026-10-17 by a research subagent; sources linked inline. Kept as the evidence behind docs/ranking.md. -->

# Scout ranking research report

The zoekt constants below come from `sourcegraph/zoekt` on `main`, read 2026-10-17. They change between releases, so treat them as a snapshot.

## Key findings by question

**1. Zoekt scoring.** Sources: [index/contentprovider.go](https://github.com/sourcegraph/zoekt/blob/main/index/contentprovider.go), [index/score.go](https://github.com/sourcegraph/zoekt/blob/main/index/score.go)

- **Per-match constants:**
  - `scoreSymbol` = 7000 and `scoreBase` (filename match) = 7000. A filename hit counts the same as a symbol hit.
  - `scorePartialSymbol` and `scorePartialBase` = 4000.
  - `scoreWordMatch` = 500, `scorePartialWordMatch` = 50.
  - `scoreKindMatch` = 100 × a per-kind factor: class 10, struct 9.5, enum 9, type and interface 8, function 7, field 5.5, constant 5, variable 4. Go `_test.go` files get ×0.8.
- **Per-file additions:**
  - `scoreFactorAtomMatch` = 400 × (1 − 1/atomMatchCount).
  - `scoreLineOrderFactor` = 1 × (1 − i/n). Earlier matches score slightly higher.
- **Final score:**
  - `ScoreOffset` (10,000,000) × query score + `scoreRepoRankFactor` (100) × repoRank (0–65535) + `scoreFileOrderFactor` (10) × docOrder.
  - Repo rank and doc order therefore only break ties.
  - In the default (non-BM25) path, test, vendored and generated files get no penalty at all.
- **BM25 path (`scoreFileBM25`):**
  - tf per term = number of matches, with filename or symbol matches adding `importantTermBoost` = 5 instead of 1.
  - Low-priority files have tf integer-divided by `lowPriorityFilePenalty` = 5.
  - k = 1.2, b = 0.75, L = file bytes / average file bytes.
  - No IDF: zoekt has no index-time tokenizer.
  - Line-level BM25 uses L = line length / 100.
- **File categories** ([index/file_category.go](https://github.com/sourcegraph/zoekt/blob/main/index/file_category.go)):
  - Assigned with go-enry `IsTest`, `IsVendor`, `IsGenerated`, `IsDotFile`, `IsConfiguration`, `IsDocumentation`.
  - `lowPriority()` is true for Test, Vendored, Generated and Binary.
- **Index-time doc order** ([index/builder.go](https://github.com/sourcegraph/zoekt/blob/main/index/builder.go) `rank()`):
  - Sort key in order: skipped last, then generated, vendored, test, then shorter name, more symbols, shorter content, more branches.
  - Each numeric key is squashed with x/(1+x).
  - `SizeMax` = 2 MiB and `TrigramMax` = 20000. Files over either are skipped, which also catches huge data files.

**2. Sourcegraph.**

- **BM25F** ([blog](https://sourcegraph.com/blog/keeping-it-boring-and-relevant-with-bm25f)):
  - Fields are content, symbols and filename.
  - An earlier design summed per-field scores (weights 2.5 and 5). It was dropped because it over-rewarded repeated matches on one term.
  - The ×5 tf boost was picked with a small grid search, and ranking was not very sensitive to it.
  - The main gain came from rewarding files that match _all_ query terms. Before, files ranked highly on a strong match to one term.
  - Reported "roughly 20% improvement across all key metrics" on an unnamed internal evaluation set.
  - The same post says they also compared embeddings and semantic reranking.
- **PageRank:**
  - [Ranking in a week](https://sourcegraph.com/blog/ranking-in-a-week) (2022) describes PageRank over a file graph whose edges are SCIP symbol references.
  - The current docs ([precise-ranking](https://docs.sourcegraph.com/dev/background-information/architecture/precise-ranking)) describe a weekly map/reduce job that _counts inbound references_ per file. That is different from PageRank.
  - No numbers were published for either.
- **Older docs** ([indexed-ranking](https://4.5.sourcegraph.com/dev/background-information/architecture/indexed-ranking)):
  - Results are bucketed by repo priority (star count).
  - The doc-order priors are the same as zoekt's.

**3. GitHub Blackbird.**

- Almost nothing about its ranking is public.
- The [engineering post](https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/) says only this:
  - Document IDs are assigned by rank, so posting lists yield good documents first.
  - Shards are keyed by Git blob SHA, which removes duplicates (115 TB down to 28 TB).
- The actionable part is the exclusion rules in the [docs](https://docs.github.com/en/search-github/github-code-search/about-github-code-search):
  - Vendored and generated code is excluded, as are empty files, files over 350 KiB, and binaries.
  - Lines over 1024 characters are truncated.
  - Files with more than one line over 4096 bytes are excluded.
- I found no source confirming that `.gitattributes` `linguist-*` overrides affect code search. They are documented for language stats and diffs ([overrides](https://github.com/github-linguist/linguist/blob/main/docs/overrides.md)), and `linguist-generated=false` is documented for the file finder.
- I saw nothing showing that zoekt reads `.gitattributes`. go-enry works on the path and content only.

**4. How developers search** ([Sadowski et al. FSE'15](https://research.google/pubs/pub43835/), numbers extracted from the PDF):

- **Query shape:** 1.85 keywords per query on average (median 1). 26.5% of queries used `file:`, 5.4% used `lang:`. About a third of searches are reformulations.
- **Intents:**
  - How / example code: 33.5%
  - What / reading code: 26%
  - Where / localization: 16%
  - Why / impact: 16%
  - Who/When / metadata: 8.5%
- **Familiarity:** 61% of searches were for code the developer already knew well or somewhat. Many queries are known-item lookups, so MRR matters.
- **Benchmarks:**
  - [CodeSearchNet](https://ar5iv.labs.arxiv.org/html/1909.09436): 99 queries, 4,026 expert labels on a 0–3 scale, NDCG ("within" the annotated set and over "all"). Its automatic set uses MRR against 999 distractors.
  - CoSQA: binary labels on Bing queries.
  - [Facebook NCS](https://ai.facebook.com/blog/neural-code-search-evaluation-dataset/): 287 Stack Overflow questions against about 24.5k Android repos. All three are natural-language-to-function benchmarks, a poor match for grep-style queries.

**5. Linguist noise rules.**

- [generated.rb](https://github.com/github-linguist/linguist/blob/main/lib/linguist/generated.rb):
  - Minified: `.js`/`.css` with average line length > 110.
  - Source-map comment in the last 2 lines, and `.map` files.
  - Go: `^// Code generated .*` in the first 40 lines.
  - protoc: "Generated by the protocol buffer compiler. DO NOT EDIT!" in the first 3 lines, plus rules for ts-proto, Thrift, gRPC, Cython and Dart.
  - Lockfiles: `package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `Cargo.lock`, `composer.lock`, `poetry.lock`, `uv.lock`, `Pipfile.lock`, `Gemfile.lock`-style names, `flake.lock`, `bun.lock`, `deno.lock`.
  - Paths: `node_modules/`, `.designer.cs`, `__generated__/`.
  - Not covered: no `@generated` marker check, no `yarn.lock` rule, no generic "DO NOT EDIT" scan.
- [vendor.yml](https://github.com/github-linguist/linguist/blob/main/lib/linguist/vendor.yml):
  - `(^|/)vendors?/`, `node_modules/`, `(3rd|[Tt]hird)[-_]?[Pp]arty/`, `(^|/)dist/`, `(^|/)cache/`, `^deps/`, `(\.|-)min\.(js|css)$`, `(^|/)testdata/`.
  - Bundled libraries such as jQuery and Bootstrap.
  - Also `.d.ts` and `.github/`, which is too broad for scout.

**6. Learning to rank vs hand-tuned scoring.**

- Without click logs, use a linear model with fewer than 10 weights, tuned by coordinate ascent that directly maximizes nDCG ([Metzler & Croft 2007](https://link.springer.com/doi/10.1007/s10791-006-9019-z)), with 5-fold cross-validation.
- LambdaMART and other tree models need thousands of labelled queries. With about 100 they overfit.

## Signals ranked by priority

1. **Saturated, length-normalized tf, summed over query terms (BM25F without IDF).**
   - Per file and term t: tf_t = Σ over matches of w(m).
   - w = 5 if the match is in the basename or path, 1 for a whole-word content match, 0.3 for a substring-only match. The 0.3 is my proposal; tune it.
   - s_t = 2.2·tf / (1.2·(0.25 + 0.75·L) + tf), with L = min(bytes / avgBytes, 20).
   - Why: saturation stops a 50k-match data file winning on raw count. Summing over terms rewards coverage, which is where Sourcegraph's 20% came from.
   - For a single-atom regex this reduces to saturated count × length normalization, which is correct for grep-style queries.
2. **Noise-class multiplier P.**
   - generated 0.2, vendored 0.2, minified 0.1, lockfile 0.1, testdata/fixtures 0.3, test 0.5, binary or huge 0.05.
   - Huge means over 2 MiB, over 20k unique trigrams, or average line length over 110 / any line over 4096 bytes.
   - The 0.2 for generated and vendored matches zoekt's ÷5.
   - Use go-enry's rules ported to Rust, or linguist's regexes. Add `@generated` and `DO NOT EDIT` checks in the first 5 lines.
   - Honour `.gitattributes` `linguist-generated`/`linguist-vendored`, including their negations. They are local and cheap.
3. **Filename/basename match equal to symbol strength.** An exact basename-stem match on the query gets +1 term-weight bonus. Source: zoekt `scoreBase` = `scoreSymbol`.
4. **IDF from query-time document frequency.**
   - Scout verifies matches anyway, so it knows df per atom for free.
   - idf = ln(1 + (N − df + 0.5) / (df + 0.5)). Only matters for multi-term queries.
   - Zoekt skips this only because it can't compute it cheaply.
5. **Duplicate collapse by content hash.** Across 140 repos, vendored copies and forks repeat. Show one blob with "+N copies". Source: Blackbird's blob-SHA sharding.
6. **Static prior used as a tiebreaker.** Precompute at index time and store as one u16 per file, so it costs nothing at query time.
   - Shorter path depth and name, more definitions, smaller file (zoekt `rank()`).
   - Git recency of the file and repo, and repos the user commits to. Sourcegraph uses commit counts and contribution-based boosts ([changelog](https://sourcegraph.com/changelog/personalized-search-ranking)). Scout has local clones, so this is nearly free.
7. **Symbol-definition boost (later).**
   - A cheap regex for definition lines (`fn|def|class|struct|func|interface|type X`) gives tf weight 5, like zoekt's ctags symbols.
   - Inbound-reference counts are a v3 idea and have no published evidence behind them.

## Recommended v1 score

```
score(f) = P(f) · Σ_t idf_t · s_t(f)  +  1e-3 · prior(f)       // prior in [0,1]
```

Starting weights:

| Parameter              | Value |
| ---------------------- | ----- |
| k1                     | 1.2   |
| b                      | 0.75  |
| L cap                  | 20    |
| w_path                 | 5     |
| w_word                 | 1     |
| w_substr               | 0.3   |
| P: generated, vendored | 0.2   |
| P: minified, lockfile  | 0.1   |
| P: fixtures            | 0.3   |
| P: test                | 0.5   |
| P: huge                | 0.05  |

- Compute avgBytes over **non-noise** files only. Otherwise vendored bulk shifts it.
- Use a multiplicative penalty, not a hard partition. A filename or symbol exact hit in a `_test` or vendored file should still beat a weak substring hit in the main code.
- Turn P off when the query's path filter targets a noise class, for example `-g '*.lock'` or `vendor/`.
- Order files by score. Keep lines in line order within each file, as rg does.
- Leave room for a later per-line BM25 (L = line length / 100) to pick the top-N lines to show.

## Golden set and evaluation protocol

- **Queries:** 60–100 real ones taken from the user's shell history (rg/scout invocations).
  - Stratify by Sadowski intent: how, what, where, why.
  - Stratify by form: single identifier, multi-term, regex, path-ish, natural-language-ish.
  - Include about 20 known-item queries ("I want file X").
- **Pooling:** take the union of the top 20 from rg order, priors-only, BM25-only and v1, so labels are not biased toward one ranker. (Superseded: the golden set pools the top 10 of five rankers, see ranking.md, "Evaluation".)
- **Labels:** file level, 0–3 as in CodeSearchNet.
  - 3 = the target or definition, 2 = a useful usage, 1 = related, 0 = noise.
  - An LLM pre-labeller is fine if a human audits about 20% and agreement (κ) is checked.
- **Metrics:**
  - Primary: nDCG@10.
  - MRR on known-item queries.
  - Recall@100 against the rg set, as a no-loss guard.
  - Label-free: noise rate in the top 10, using the noise classifier. It can run on every commit.
- **Significance:**
  - Paired randomization (permutation) test or paired t-test on per-query nDCG. They agree closely ([Smucker et al. CIKM'07](https://ciir-publications.cs.umass.edu/pub/web/getpdf.php?id=744)) but diverge below about 25 queries ([Smucker 2009](https://ciir-publications.cs.umass.edu/pub/web/getpdf.php?id=885)).
  - Use at least 50 queries ([Voorhees & Buckley SIGIR'02](https://www.nist.gov/node/707561)).
  - Power estimate: n ≈ (2.8·σ/Δ)². If σ(ΔnDCG) is about 0.15 (an assumption; measure it), detecting Δ = 0.05 needs about 70 queries.
  - Report bootstrap confidence intervals.
- **Tuning:** coordinate ascent on nDCG@10 with 5-fold cross-validation. Freeze a held-out 20% of queries.

## Semantic reranking (brief)

- For identifier and regex queries, which are most of them (median 1 keyword), lexical ranking plus priors wins. A reranker adds latency and can demote the exact hit.
- It pays off only for natural-language queries ("how do we retry http").
- If added: rerank the lexical top 50 with a small code embedding model or cross-encoder, gated on queries that look like natural language (several dictionary words, no regex metacharacters).
- Sourcegraph evaluated embeddings and reranking against BM25 but shipped BM25F as the baseline.

## Pitfalls

- **Parity gate:** ranking must be an opt-in flag, such as `--rank`. The default output must stay byte-identical to rg so the `scout == rg` gate stays green. Alternatively, ranked mode could be checked as a set comparison against rg.
- **Latency:** ranking needs every match before the first output, so streaming and time-to-first-result are lost. Per CLAUDE.md, benchmark it as an experiment with `scout-bench --runs 100` and log it in docs/experiments.md.
- **vendor.yml is too broad for scout:** `.d.ts`, `.github/`, `dist/`, `cache/`, `testdata/` are sometimes exactly what the user wants. Curate the list and keep penalties soft.
- **Length normalization:** b = 0.75 on bytes can bury legitimately large core files. Cap L and treat b as tunable.
- **Static priors as tiebreakers don't remove noise.** Zoekt's default path shows this: no category penalty, and repo/doc rank only break ties. The demotion has to be multiplicative inside the score.
- **Pool bias:** labelling only v1's top 10 inflates v1. Always pool from several rankers.
- **Constants drift:** cite the zoekt commit you copy from. Sourcegraph's evaluation sets and their "20%" are internal, so treat the number as unverified.
- **Not verified:** the Sourcegraph PageRank vs reference-count discrepancy, and whether GitHub applies `linguist-*` attributes to code search.
