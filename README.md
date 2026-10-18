# scout

Index many repos once, then grep all of them in milliseconds. Output matches ripgrep's
(`rg --no-heading --with-filename`). By default results are never stale: files that changed
since indexing are searched from disk.

```bash
scout add ~/source/*
scout index
scout search -n useLayoutEffect
scout search --repo api -w 'fn\s+parse_\w+'
```

The index lives in `--index-dir`, defaulting to `$SCOUT_INDEX_DIR`, else the platform data
directory. Search flags follow rg (`-i -S -F -w -n -l -c -m`). Scout adds:

- `--freshness strict|off`: `strict` (the default) checks the file tree against the index
  on every search; `off` trusts the index.
- `--change-detection journal|scan`: how strict mode finds changes. `journal` reads the
  NTFS change journal where there is one; `scan` re-lists every indexed directory.
- `--repo NAME` (repeatable) limits the search to the named repos, and `--stats` prints
  timings and candidate counts to stderr.

### Hosted: mirror remotes, sync on a schedule, serve over HTTP

```bash
scout sync --config sync.toml          # one cycle: clone/fetch, reindex what moved, publish
scout serve --config sync.toml --listen 127.0.0.1:7700
curl "http://127.0.0.1:7700/search?q=useLayoutEffect&n=1"
curl http://127.0.0.1:7700/status
```

The config lists `[[repo]]` URLs and `[[github]]` orgs or users. The GitHub token is read from the env var it names (`GITHUB_TOKEN` by default) and is never logged. `/search` takes `q` plus rg-style flags (`F i S w n l c m`, `repo` repeatable) and returns rg-identical text, or with `rank=1&top=N` only the N best files ([docs/ranking.md](docs/ranking.md)), 422 past `--max-output-mb` (default 16) or 503 past `--search-timeout-secs` (default 30). The server needs `git` 2.31 or newer. Format and reasoning are in [docs/design.md](docs/design.md), "Hosted sync".

### Hardware

scout adapts to the machine it runs on: threads follow the CPUs (cgroup CPU quotas
included), the indexing budget is a quarter of usable RAM (capped at 4 GiB; a container's
or systemd unit's cgroup memory limit counts, not the host's RAM; where scout cannot see its
own cgroup, as in a cgroup v1 container without its own namespace, it stops and asks for
`--memory-mb`), and the index is mmapped, so it does not have to
fit in memory. Measured with 137 repos (54k files, a 1.0 GiB / 1.07 GB index):

- **1 CPU / 2 GiB** works: indexing 16 s at a 130 MiB peak, selective searches 2–7 ms
  served (12–24 ms as a fresh CLI process), about 29 served searches a second.
- **2–4 CPUs / 4 GiB** is the sweet spot: at 4 CPUs a p99 of 26 ms with one client and
  about 85 searches a second under load.
- **More cores** keep paying for broad scans and concurrent clients (8 CPUs: ~103 served
  searches a second); a single selective search is as fast at 4 CPUs as at 12.

Memory is the number that matters most: give the page cache room for the **whole index**
(about 1.2x the size of the files it indexes; 1.07 GB for 854 MiB here). With the index
cached, served searches stay at a 36–40 ms geomean at 1 CPU; with ~94% of it cached they are
5.6x slower, with ~70% 27x. A query that finds the index cold costs 1–4 s on any CPU count,
because it waits on one or two random disk reads per repo shard, so put the index on a disk
with low read latency (local NVMe or EBS gp3, not a burstable instance's baseline EBS
bandwidth). Full tables: [docs/design.md](docs/design.md), "Measured on small and large
machines", and [docs/experiments.md](docs/experiments.md), E21.

How it works and why: [docs/design.md](docs/design.md). Every optimisation, with numbers:
[docs/experiments.md](docs/experiments.md).

## Layout

```
crates/scout-core          domain model, ports, query planner, search pipeline (no I/O)
crates/scout-index-native  mmap shard format (corpus bin, unit trigram postings, trigram filter)
crates/scout-index-sqlite  the same contract on SQLite
crates/scout-source-fs     local repos: rg-identical walking, snapshots, change journal
crates/scout-store-fs      shards, snapshots and the catalog in a local directory
crates/scout-sync          hosted ingestion: sync config, GitHub listing, git working copies
crates/scout-cli           the `scout` binary (composition root): index, search, sync, serve
crates/scout-bench         benchmark and parity gate against rg; scout-rank-eval (ranking evaluation)
vendor/ripgrep             upstream ripgrep (git subtree)
packages/kernel            TypeScript scaffold, reserved for the MCP server
```

## Commands

| Command                                            | What it does                                                                                                                       |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `cargo clippy --workspace --all-targets --release` | Lints (pedantic, no `unwrap`; `unsafe` only in `win.rs` and the shard mmap); must be clean                                         |
| `cargo test --workspace --release`                 | Unit and end-to-end tests                                                                                                          |
| `cargo build --release`                            | Builds `scout`, `scout-bench` and `scout-rank-eval`                                                                                |
| `yarn check`                                       | Format every file but `vendor/` (upstream) and `bench/results/` (bench output); lint, typecheck and test the TypeScript workspaces |

### Benchmark

```bash
target/release/scout-bench --scout target/release/scout --index-dir <dir> --queries bench/queries.tsv --runs 100 --rg-runs 1 --scout-flags="--freshness strict" --label E7-strict --json bench/results/E7-strict.json
```

For each query it reports the median, p90 and p99 wall time, rg's time, candidates, peak
memory, and a parity check of scout's output against rg's. Tails are reported only with
`--runs` of 100 or more. Reindex before benchmarking `--freshness off`: files edited since
indexing make parity fail, as they should. A file in a parity diff that holds a NUL byte is
re-checked against rg searching it alone, because rg's output for such a file depends on what
its thread searched before (E14); the bench says so on stderr each time. A text file is never
re-checked.

### Ranking evaluation

`scout search --rank --top N` prints the N best files first ([docs/ranking.md](docs/ranking.md)).
Its quality is measured against hand-labelled judgments in `bench/golden/`, which is not
published: its labels are paths in the author's own projects ([docs/ranking.md](docs/ranking.md),
"Evaluation", describes the set):

```bash
target/release/scout-rank-eval --dumps <dir> dump --scout target/release/scout --index-dir <dir>
target/release/scout-rank-eval --dumps <dir> eval --compare full,full+dedup
target/release/scout-rank-eval --dumps <dir> crossval
```

## TypeScript workspaces

Yarn 4 workspaces, TypeScript 7, oxlint + oxfmt, reserved for the MCP server, which will
call the same core as the CLI. Node 24 runs the `.ts` sources directly (type stripping), so
only erasable TS syntax is allowed and relative imports use the `.ts` extension. Only
`packages/kernel` exists today; the layer rules below apply as `domains/*` and `apps/*`
workspaces are added.

| Command                             | What it does                                  |
| ----------------------------------- | --------------------------------------------- |
| `yarn lint` / `yarn lint:fix`       | oxlint (type-aware), warnings fail the run    |
| `yarn format` / `yarn format:check` | oxfmt                                         |
| `yarn typecheck`                    | `tsc --build` across all workspaces and tests |
| `yarn test`                         | `node --test` over every `src/**/*.test.ts`   |

## Dependency rules

Dependencies only point inward:

```
apps  ──►  domains  ──►  packages (kernel)
```

| Layer                              | May import         | May not import      | Runtime                                                |
| ---------------------------------- | ------------------ | ------------------- | ------------------------------------------------------ |
| `packages/*` (kernel, shared libs) | nothing internal   | any `@scout/*`      | pure (no `node:*`, no `process.env`, no `@types/node`) |
| domains                            | `@scout/kernel`    | other domains, apps | pure (same as packages)                                |
| apps                               | kernel, any domain | other apps          | Node                                                   |

- **Public API only.** Each workspace exposes exactly one entry point, `src/index.ts`
  (`exports` in its `package.json`). Deep imports like `@scout/kernel/src/x.ts` are an error.
- **No relative escapes.** A relative import may not leave its workspace
  (`../../../packages/kernel/src/...`); import the other workspace by package name.
- **Domains don't talk to each other.** If search needs something from indexing, the app
  wires them together (search declares a port/interface, the app passes in an indexing
  implementation), or the shared concept moves into the kernel.
- **Domains are pure.** Filesystem, process, env and stdout live in apps. A domain gets
  what it needs through function parameters / ports, which keeps it testable without I/O.
- **Package naming encodes the layer** (`domain-*`, `app-*`). The lint rules match on
  those prefixes, so new workspaces must follow the convention to be covered.

### What enforces what

| Rule                                          | Enforced by                                                                                                                                                                                                                          |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Layer direction, no domain↔domain, no app↔app | oxlint `no-restricted-imports` overrides (by package name). `tsc` does not catch these under TS 7.                                                                                                                                   |
| No deep imports                               | oxlint, and `tsc` (package `exports` only expose `.`)                                                                                                                                                                                |
| No relative escapes                           | oxlint regex. `tsc` also fails when the target project is not referenced (TS6059).                                                                                                                                                   |
| Pure layers use no Node APIs                  | `types: []` in their tsconfig (`process`, `Buffer` fail to type-check), oxlint `import/no-nodejs-modules` and `node/no-process-env`, and `triple-slash-reference` so a `/// <reference types="node" />` cannot bring Node types back |

**Known gap:** nothing checks that an imported npm package is declared in that
workspace's `package.json`. oxlint has no `no-extraneous-dependencies`, and hoisting makes
every installed package resolvable. Review new third-party imports in domains by hand.

## Tests

Tests sit next to the code as `*.test.ts` and use `node:test` + `node:assert`. They are
type-checked by [`tsconfig.test.json`](tsconfig.test.json), which includes `@types/node`,
instead of by their workspace's project. That way tests in pure layers can use Node while
the code under test cannot.

## Adding a workspace

1. Create `domains/<name>` (or `apps/<name>`, `packages/<name>`) with `package.json` named
   `@scout/domain-<name>` / `@scout/app-<name>` / `@scout/<name>`, and
   `exports: { ".": "./src/index.ts" }`.
2. Add a `tsconfig.json` extending `../../tsconfig.base.json` with `references` to the
   workspaces it depends on. Add it to the `references` of both the root `tsconfig.json`
   and `tsconfig.test.json`.
3. Declare internal deps as `"@scout/kernel": "workspace:*"` and run `yarn install`.

## Lint strictness, briefly

Categories `correctness`, `suspicious`, `perf`, `pedantic` are all errors, plus:

- **Coupling:** no cycles, at most 10 value imports per module (type-only imports are not
  counted), no default exports, type-only imports marked as such, explicit types on
  exported functions.
- **Cohesion / size:** 300 lines per file, 60 per function, cyclomatic complexity 10,
  nesting depth 3, max 3 params (use an options object), 1 class per file.
- **Type safety:** no `any`, no `!`, no `@ts-ignore`, no floating promises, exhaustive
  `switch`, conditions must be real booleans (no truthiness checks on strings, numbers or
  nullable objects), no unsafe `any` flow.
- **Errors surface:** no empty `catch`, promises must be handled, no `process.exit` (set
  `process.exitCode` so output is flushed). Use `invariant()` from the kernel to fail fast
  at boundaries.
