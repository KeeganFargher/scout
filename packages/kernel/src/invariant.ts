/**
 * Thrown when code reaches a state its author declared impossible.
 *
 * A dedicated class (rather than a bare `Error`) lets callers and logs distinguish
 * "a programmer assumption broke" from expected, domain-level failures.
 */
export class InvariantViolation extends Error {
  public override readonly name = "InvariantViolation";
}

/**
 * Fail fast at a boundary instead of letting a bad value travel downstream.
 *
 * Validate once at the edge, then let everything below rely on the narrowed type.
 * There is deliberately no "soft" variant: a broken invariant should crash loudly.
 */
export function invariant(condition: boolean, message: string): asserts condition {
  if (!condition) {
    throw new InvariantViolation(message);
  }
}
