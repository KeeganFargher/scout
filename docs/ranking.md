# Ranking

`scout search --rank` returns the files most worth reading first, instead of every match
in walk order. The evidence behind each choice is in
[ranking-research.md](ranking-research.md). Every change to the scorer is an experiment,
measured on the golden set and logged in [experiments.md](experiments.md).

## Contract

- **Opt-in.** Without `--rank`, output is byte-identical to rg, and the parity gate keeps
  proving it. Ranking never changes _which_ files match: the ranked set equals rg's set
  (recall@all = 1 by construction, checked end to end by `tests/rank.rs`, which compares
  the ranked files with the plain search's). It only changes
  their order and how many are shown.
- **Output.** Files in score order, best first, each printed exactly as rg prints it
  (lines in line order, or the path for `-l`, or `path:count` for `-c`). `--top N`
  (default 20) bounds the files shown, so ranked output also stays small for agents.
- **Cost.** Ranking needs every match before printing the first file, so it gives up
  streaming. It verifies the plain search's chunks on every thread, with its candidates
  (shape filter included), and scores each matching file in one pass (E39): on chromium +
  llvm-project at 2 CPUs, `--top 20` costs 0.95–2.4x the plain search warm. It is
  benchmarked like any other mode.
- **Server.** `rank=1&top=N` on `/search`.

## What a query is made of

Agents search with alternations of identifiers far more than with single words: of 1,044
distinct Grep patterns that Claude Code agents ran on this machine's projects, most are
`a|b|c` lists of names (see the golden set). So the scorer splits the pattern's top-level
alternation into **terms**, one per branch. A non-alternation pattern is one term. Each
term gets its own matcher, with the search's case and word settings.

## Features (query time, matched files only)

Computed while verifying, for files that match, from content the search already holds:

| Feature                        | How                                                                                                                                                                                                                                                                                                              |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| per-term matches               | term matcher over each matching line                                                                                                                                                                                                                                                                             |
| per-term whole-word matches    | match bounded by non-word bytes on both sides                                                                                                                                                                                                                                                                    |
| per-term definition matches    | the match is the name a line defines: the identifier after `fn`, `def`, `class`, `struct`, `interface`, `type`, `enum`, `func`, `function`, `const`, … (past modifiers like `export`, `pub`, `public`), or before `(` in a modifier-led C-family member; `const w = makeWidget(3)` defines `w`, not `makeWidget` |
| per-term path / basename match | term matcher over the repo-relative path and its file name                                                                                                                                                                                                                                                       |
| size, line shape               | bytes, average and longest line length                                                                                                                                                                                                                                                                           |
| noise class                    | below                                                                                                                                                                                                                                                                                                            |

Nothing is stored in the shard yet. Index-time priors (path depth, symbol counts, git
recency) are deferred until the query-time version is measured.

## Score

For file _f_ and term _t_:

```
tf_t   = w_word·word_t + w_sub·(matches_t − word_t)
s_t    = (k1 + 1)·tf_t / (k1·(1 − b + b·L) + tf_t)       L = min(bytes / avg_bytes, L_cap)
g_t    = w_def·[def_t > 0] + w_base·[basename_t] + w_path·[path_t]
idf_t  = ln(1 + (N − df_t + 0.5) / (df_t + 0.5))          N = docs searched, df_t = files matching t
score  = P(f) · Σ_t idf_t · (s_t + g_t)
```

- **Saturation (BM25 `k1`)** stops a data file with 50,000 hits outranking a source file
  with 5. **Structural hits (`g_t`) are added outside it**: inside, every term tops out
  near `k1 + 1`, so a 400 KB dump with 5,000 hits tied the 4 KB file defining the name
  (caught by the scorer's first unit test). zoekt's default scorer does the same: a
  symbol hit outweighs any number of word hits. **Summing over terms** rewards files that match many of the alternation's names,
  which is where Sourcegraph's BM25F gain came from.
- **IDF** comes free: the search knows how many files each term matched. It only matters
  with several terms, where it lifts the rare names over the common ones.
- **`avg_bytes`** is the mean size of non-noise matched files, so vendored bulk does not
  stretch it.
- **P(f)** is a multiplier, never a filter: a definition in a test file can still beat a
  passing mention in main code.

Starting weights (content scores top out near `k1 + 1` = 2.2 per term, so the structural
bonuses are on that scale; tuning them on the golden set lost in cross-validation, R3):

| weight   | start | weight   | start |
| -------- | ----: | -------- | ----: |
| `w_word` |   1.0 | `k1`     |   1.2 |
| `w_sub`  |   0.3 | `b`      |  0.75 |
| `w_def`  |   1.0 | `L_cap`  |    20 |
| `w_base` |   1.5 | `w_path` |   0.5 |

## Noise classes

A file gets the lowest multiplier of the classes it falls in:

| class              |    P | detected by                                                                                                                                                                                                                                                                                         |
| ------------------ | ---: | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| huge or binary-ish | 0.05 | > 2 MiB, or a line > 4096 bytes in a file of fewer than 50 lines                                                                                                                                                                                                                                    |
| minified           |  0.1 | `.js`/`.mjs`/`.cjs`/`.css` with average line > 110 bytes, or `.min.` in the name                                                                                                                                                                                                                    |
| lockfile           |  0.1 | `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.lock`, `poetry.lock`, `go.sum`, … (18 names)                                                                                                                                                                                             |
| generated          |  0.2 | `@generated`, `DO NOT EDIT`, `<auto-generated`, `autogenerated`, `auto-generated by`, `this file is generated` in the first 1 KiB; `.designer.cs`, `.g.cs`, `.g.i.cs`, `.pb.go`, `.pb.cc`, `.pb.h`, `_pb2.py`, `.map`; a segment `__generated__` or `generated`; `*_*.cs` in a `migrations` segment |
| vendored           |  0.2 | a path segment `node_modules`, `vendor`, `vendors`, `third_party`, `thirdparty`, `3rdparty`, `bower_components`, `Pods`, `.yarn`, `site-packages`, or `wwwroot/lib/`                                                                                                                                |
| build output       |  0.2 | a segment `dist`, `build`, `out`, `bin`, `obj`, `target`, `.next`, `coverage`                                                                                                                                                                                                                       |
| fixtures           |  0.3 | a segment `fixtures`, `__fixtures__`, `__snapshots__`, `testdata`, or a `.snap` file                                                                                                                                                                                                                |
| tests              |  0.5 | a segment `test`, `tests`, `__tests__`, `spec`, `specs`; a stem ending `_test`, `-test`, `_tests`, `_spec`, or capitalised `Test`/`Tests` (`WidgetTest.java`, not `contest.ts`); `*.test.*`, `*.spec.*`, `test_*`                                                                                   |

Segments and names compare case-insensitively, except the capitalised test suffix, where
the case is the signal.

Linguist's `vendor.yml` is deliberately not copied whole: `.d.ts`, `.github/` and `cache/`
are often exactly what an agent is looking for. Duplicate content across repos (copies,
worktrees, archives: this corpus has several) is not a noise class. It is handled by the
order below.

## Order

Files are printed in score order, with ties broken by path. Then every **identical copy**
(the same content hash as a file ranked above it) moves behind all first copies, keeping
relative order. Nothing is dropped, so the set stays rg's, but a fork's or worktree's copy
of a file the agent has already read stops taking a top-10 slot (experiment R2).

## Evaluation

The set lives in `bench/golden/` (a README with the protocol and rubric, `queries.tsv` and
`labels.tsv`) and is not in the public repository: its queries come from the author's own
agent transcripts, and its labels are paths in the author's own projects. The numbers are
in [docs/experiments.md](experiments.md) (R1–R4).

- **Queries** (`queries.tsv`): 79 in total.
  - 62 are real Grep patterns that agents ran in six of the indexed projects, taken from
    Claude Code transcripts and stratified by pattern class.
  - 17 are known-item queries: an identifier that its project defines in exactly one
    file.

  They run over the whole corpus, as the hosted server would.

- **Pooling.** Labels cover the union of the top 10 from these rankers, shuffled per
  query: unranked (path order), noise-only, BM25-only, the scorer, and the scorer with
  copies collapsed. A labeller sees paths and matching lines, never scores, sizes or noise
  classes.
- **Labels** (`labels.tsv`): 1,556 judgments, graded
  0–3 by hand by labelling agents that never saw the scorer. Known-item targets and their
  identical copies are graded 3 automatically. Labels assigned by path-rule scripts were
  thrown away. They would have copied the scorer's own noise rules and made the
  evaluation circular.
- **Frozen index.** Labels and dumps refer to a copy of the bench index that is never
  reindexed and leaves out the scout repo, so the numbers reproduce.
- **Metrics:**
  - nDCG@10, the primary metric. Unjudged files count as 0.
  - MRR.
  - noise@10: grade-0 files plus identical copies of a file ranked above.
  - judged@10: the coverage check for systems that were not pooled.
- **Decisions.** Each decision uses a paired permutation test on per-query values. Tuned
  weights must also win in leave-one-project-out cross-validation, because a gain measured
  on the queries the tuning used is not evidence.

Results (experiments R1–R4):

- The scorer beats unranked output by +0.344 nDCG@10 (0.451 → 0.795, p ≤ 0.0001) and
  BM25 alone by +0.165 (p ≤ 0.0001).
- Collapsing copies cuts noise@10 from 0.129 to 0.090 (p ≤ 0.0001). nDCG@10 is unchanged
  (0.000, p = 0.98).
- Tuning the weights lost in cross-validation (0.795 → 0.774 held out, p = 0.013), so
  the defaults in `Weights::default` stand.
- `--top 20` costs about 5% latency (geomean) and cuts memory on broad queries.
  Printing every file ranked costs about 26%, and 1.1–2.4x the memory on broad queries.

Known weaknesses:

- The definition and basename bonuses match substrings: for `RetailerCategoryMapping`,
  `RetailerCategoryMappingsController.cs` earns both, and the defining file does not
  stand out (golden query q075).
- One labelling pass per candidate, by LLM agents; no second annotator.
- Bare identifiers (0.691) and case-insensitive patterns (0.696) are the weakest classes.
