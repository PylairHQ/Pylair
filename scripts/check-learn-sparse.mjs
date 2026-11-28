#!/usr/bin/env node
/* Smoke-test the sparse-checkout command documented in docs/learn/README.md.
 *
 * The documentation clones main. Before the learning path reaches main, CI
 * overrides the ref with the pull-request branch through GITHUB_HEAD_REF;
 * after merge GITHUB_REF_NAME is main, exactly as documented. The sparse
 * checkout itself is the behavior under test.
 */

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const url = process.env.LEARN_SPARSE_URL ?? "https://github.com/PylairHQ/Pylair.git";
const localBranch = spawnSync("git", ["branch", "--show-current"], { encoding: "utf8" })
  .stdout.trim();
const ref =
  process.env.LEARN_SPARSE_REF ??
  process.env.GITHUB_HEAD_REF ??
  process.env.GITHUB_REF_NAME ??
  (localBranch || "main");
const step = process.env.LEARN_SPARSE_STEP ?? "01-first-app";
const parent = mkdtempSync(join(tmpdir(), "pylair-learn-sparse-"));
const checkout = join(parent, "pylair-learn");

function git(args, cwd) {
  const res = spawnSync("git", args, { cwd, encoding: "utf8" });
  if (res.error) throw res.error;
  if (res.status !== 0) {
    throw new Error(
      `git ${args.join(" ")} failed with exit ${res.status}\n${res.stdout}${res.stderr}`,
    );
  }
}

try {
  const args = ["clone", "--depth", "1", "--filter=blob:none", "--sparse"];
  if (ref !== "main") args.push("--branch", ref);
  args.push(url, checkout);
  git(args, parent);
  git(["sparse-checkout", "set", `examples/learn/${step}`], checkout);
  const root = join(checkout, "examples", "learn", step);
  for (const rel of ["package.json", "pyproject.toml", "backend/app.py", "src/App.tsx"]) {
    assert.ok(existsSync(join(root, rel)), `${step}/${rel} missing after sparse checkout`);
  }
  console.log(`check-learn-sparse: ${step} fetched from ${url} at ${ref}`);
} finally {
  rmSync(parent, { recursive: true, force: true });
}
