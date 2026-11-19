import { test } from "node:test";
import assert from "node:assert/strict";
import { diffRange, isDocsPath, needsCode } from "./ci-changes.mjs";

test("documentation paths", () => {
  for (const p of [
    "docs/security.md",
    "docs/decisions/0003-wrap-mode.md",
    "docs/anything.png",
    "README.md",
    "HANDOFF.md",
    "AGENTS.md",
    "packages/cli/README.md",
    "python/pylair/README.md",
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "packages/cli/LICENSE-MIT",
    "docs\\build.md",
  ])
    assert.ok(isDocsPath(p), p);
});

test("everything else is code, including look-alikes", () => {
  for (const p of [
    "packages/cli/src/cli.ts",
    ".github/workflows/ci.yml",
    "crates/tauri-plugin-pylair/src/lib.rs",
    "docs.txt",
    "mydocs/a.py",
    "scripts/ci-changes.mjs",
    "llms.txt",
    "Cargo.lock",
    "notes.md.py",
    "templates/README.md.tmpl",
    "xLICENSE",
  ])
    assert.ok(!isDocsPath(p), p);
});

test("a change needs the full suite unless EVERY file is documentation", () => {
  assert.equal(needsCode(["docs/a.md", "README.md"]), false);
  assert.equal(needsCode(["docs/a.md", "packages/cli/src/cli.ts"]), true);
  assert.equal(needsCode(["docs/a.md", ".github/workflows/ci.yml"]), true);
  assert.equal(needsCode([]), true, "an empty diff is not provably docs-only");
});

test("the diff range per event, and when there is none to trust", () => {
  assert.equal(diffRange({ EVENT: "pull_request", BASE_SHA: "b", HEAD_SHA: "h" }), "b...h");
  assert.equal(diffRange({ EVENT: "pull_request", BASE_SHA: "b" }), undefined);
  assert.equal(diffRange({ EVENT: "push", BEFORE_SHA: "a", AFTER_SHA: "z" }), "a..z");
  assert.equal(diffRange({ EVENT: "push", BEFORE_SHA: "0000000000", AFTER_SHA: "z" }), undefined);
  assert.equal(diffRange({ EVENT: "push", AFTER_SHA: "z" }), undefined);
  assert.equal(diffRange({ EVENT: "workflow_dispatch", BEFORE_SHA: "a", AFTER_SHA: "z" }), undefined);
  assert.equal(diffRange({ EVENT: "schedule" }), undefined);
  assert.equal(diffRange({}), undefined);
});
