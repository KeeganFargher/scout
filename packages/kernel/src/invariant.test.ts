import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { invariant, InvariantViolation } from "./invariant.ts";

describe("invariant", () => {
  it("returns when the condition holds", () => {
    assert.doesNotThrow(() => {
      invariant(true, "unreachable");
    });
  });

  it("throws InvariantViolation with the given message when the condition fails", () => {
    assert.throws(
      () => {
        invariant(false, "index must be built before searching");
      },
      { name: "InvariantViolation", message: "index must be built before searching" },
    );
    assert.throws(() => {
      invariant(false, "x");
    }, InvariantViolation);
  });
});
