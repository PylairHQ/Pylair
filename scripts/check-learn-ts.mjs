#!/usr/bin/env node
/* TypeScript and sparse-checkout checks for the learning-path examples.
 *
 * The examples are standalone apps, deliberately outside the pnpm workspace.
 * This script reproduces the user-facing setup in each one: npm install,
 * uv sync, codegen, then a strict tsc pass. It never touches the network
 * beyond package resolution; the Python runtime harness lives in the Python
 * lane.
 */

import { readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const learn = join(root, "examples", "learn");
const shell = process.platform === "win32";

function run(cmd, args, cwd) {
  const command = [cmd, ...args].join(" ");
  const res = shell
    ? spawnSync(command, { cwd, stdio: "inherit", shell: true })
    : spawnSync(cmd, args, { cwd, stdio: "inherit" });
  if (res.error) throw res.error;
  if (res.status !== 0) {
    throw new Error(`${command} failed in ${cwd} with exit ${res.status}`);
  }
}

const steps = readdirSync(learn)
  .filter((name) => /^\d\d-/.test(name) && existsSync(join(learn, name, "package.json")))
  .sort();

if (steps.length === 0) {
  console.error("check-learn-ts: no learning examples found");
  process.exit(1);
}

for (const step of steps) {
  const cwd = join(learn, step);
  console.log(`check-learn-ts: ${step}`);
  run("npm", ["ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd);
  run("uv", ["sync", "--frozen"], cwd);
  run("npx", ["pylair", "codegen", "--module", "backend.app", "--out", "src/api.generated.ts"], cwd);
  run("npx", ["tsc", "--noEmit"], cwd);
  // Shipping and binary declarations are validated without building
  // installers or requiring the host GUI toolchain in the JS lane.
  if (/^0[45]-/.test(step)) run("node", [join(root, "scripts", "check-learn-build-config.mjs"), cwd], root);
}
run("node", [join(root, "scripts", "check-learn-sparse.mjs")], root);
console.log(`check-learn-ts: ${steps.length} step(s) typechecked`);
