// Public API of the shared kernel. Other workspaces may only import from here
// (enforced by `exports` in package.json and by lint).
export { invariant, InvariantViolation } from "./invariant.ts";
