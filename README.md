# scout

A codebase search tool. Yarn 4 workspaces, TypeScript 7, oxlint + oxfmt.

## Commands

| Command                               | What it does                                                   |
| ------------------------------------- | -------------------------------------------------------------- |
| `yarn check`                          | Format check, lint, typecheck, tests — run before every commit |
| `yarn lint` / `yarn lint:fix`         | oxlint (type-aware), warnings fail the run                     |
| `yarn format` / `yarn format:check`   | oxfmt                                                          |
| `yarn typecheck`                      | `tsc --build` across all workspaces and tests                  |
| `yarn test`                           | `node --test` over every `src/**/*.test.ts`                    |
| `yarn workspace @scout/app-cli start` | Run the CLI                                                    |

There is no build step for running: Node 24 executes the `.ts` sources directly
(type stripping), which is why only erasable TS syntax is allowed and relative
imports use the `.ts` extension.

## Layout

```
packages/kernel      @scout/kernel           shared primitives (bottom layer)
domains/indexing     @scout/domain-indexing  codebase -> index
domains/search       @scout/domain-search    query -> results against an index
apps/cli             @scout/app-cli          composition root, all real I/O
```

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
