import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import test from "node:test";

const repo = join(import.meta.dirname, "..", "..", "..");

test("learning-path snippets match the example files verbatim", () => {
  const run = spawnSync(process.execPath, ["scripts/check-learn-snippets.mjs"], {
    cwd: repo,
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr || run.stdout);
  assert.match(run.stdout, /snippets verified/);
});
