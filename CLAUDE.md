# scout

See README.md for layout and commands, docs/design.md for the architecture, and
docs/experiments.md for every performance change and its numbers.

## Rust (crates/)

- `cargo clippy --workspace --all-targets --release` and `cargo test --workspace --release`
  must both pass before work is declared done.
- `unsafe` lives only in `crates/scout-source-fs/src/win.rs` (all OS FFI), plus the one
  shard `mmap` in `crates/scout-index-native/src/lib.rs` (`memmap2::Mmap::map` has no safe
  form and the index is mmapped on every platform). Every block has a SAFETY comment.
- A performance change is an experiment. Benchmark it with `scout-bench` (`--runs 100` for
  p99), keep it only if it wins, and log it in docs/experiments.md either way.
- The bench parity gate (scout output == rg output) must stay green.

## TypeScript (packages/, future domains/ and apps/)

- Run `yarn check` (format, lint, typecheck, tests); it must pass with zero warnings.
- Never import across layers against the rules in README.md, and never deep-import a
  workspace. Fix the design (port, kernel, or app wiring) instead of disabling the lint.
- Relative imports use the `.ts` extension; only erasable TS syntax (no enums, namespaces,
  parameter properties).
