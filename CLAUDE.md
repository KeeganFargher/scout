# scout

See README.md for layout, commands and the dependency rules between layers.

- Run `yarn check` before declaring work done (format, lint, typecheck, tests); it must
  pass with zero warnings.
- Never import across layers against the rules in README.md, and never deep-import a
  workspace — fix the design (port, kernel, or app wiring) instead of disabling the lint.
- Relative imports use the `.ts` extension; only erasable TS syntax (no enums, namespaces,
  parameter properties).
