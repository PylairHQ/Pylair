import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { mayMoveLatest } from "../../../scripts/npm-latest-guard.mjs";

describe("npm-latest-guard", () => {
  // While no stable exists on the registry, `latest` follows the newest
  // RC so `npm create pylair@latest` resolves during the RC train.
  test("rc while registry has only rcs → latest moves", () => {
    assert.ok(mayMoveLatest(["0.1.0-rc.13", "0.1.0-rc.14"], "0.1.0-rc.15"));
  });

  test("first ever rc (empty registry) → latest moves", () => {
    assert.ok(mayMoveLatest([], "0.1.0-rc.1"));
  });

  test("rc while a stable exists → latest must NOT move", () => {
    assert.ok(!mayMoveLatest(["0.1.0-rc.15", "0.1.0"], "0.1.0-rc.16"));
    assert.ok(!mayMoveLatest(["0.9.0", "1.0.0-rc.1"], "1.0.0-rc.2"));
  });

  test("a stable publish always moves latest", () => {
    assert.ok(mayMoveLatest(["0.1.0-rc.15"], "0.1.0"));
    assert.ok(mayMoveLatest(["0.1.0"], "0.2.0"));
    // Even if the newest registry entry is an rc, the stable publish wins.
    assert.ok(mayMoveLatest(["0.1.0", "1.1.0-rc.1"], "1.1.0"));
  });
});
