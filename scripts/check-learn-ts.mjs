#!/usr/bin/env node
/* TypeScript and sparse-checkout checks for the learning-path examples.
 *
 * The examples are standalone apps, deliberately outside the pnpm workspace.
 * This script reproduces the user-facing setup in each one: npm install,
 * uv sync, codegen, then a strict tsc pass. It never touches the network
 * beyond package resolution; the Python runtime harness lives in the Python
 * lane.
 */

import { readdirSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import http from "node:http";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const learn = join(root, "examples", "learn");
const shell = process.platform === "win32";

function quoted(command) {
  return command.map((x, i) => (i === 0 ? x : `"${x.replaceAll('"', '\\"')}"`)).join(" ");
}

function run(cmd, args, cwd) {
  const command = [cmd, ...args].join(" ");
  const res = shell
    ? spawnSync(quoted([cmd, ...args]), { cwd, stdio: "inherit", shell: true })
    : spawnSync(cmd, args, { cwd, stdio: "inherit" });
  if (res.error) throw res.error;
  if (res.status !== 0) {
    throw new Error(`${command} failed in ${cwd} with exit ${res.status}`);
  }
}

function capture(cmd, args, cwd) {
  const res = shell
    ? spawnSync(quoted([cmd, ...args]), { cwd, encoding: "utf8", shell: true })
    : spawnSync(cmd, args, { cwd, encoding: "utf8" });
  if (res.error) throw res.error;
  if (res.status !== 0) {
    throw new Error(`${[cmd, ...args].join(" ")} failed in ${cwd}: ${res.stderr}`);
  }
  return res.stdout;
}

const steps = readdirSync(learn)
  .filter((name) => /^\d\d-/.test(name) && existsSync(join(learn, name, "package.json")))
  .sort();

if (steps.length === 0) {
  console.error("check-learn-ts: no learning examples found");
  process.exit(1);
}

run("pnpm", ["--filter", "@pylair/client", "build"], root);
run("pnpm", ["--filter", "@pylair/cli", "build"], root);

const packDir = mkdtempSync(join(tmpdir(), "pylair-learn-packs-"));
const tarballs = new Map();
for (const pkg of ["client", "cli"]) {
  const out = capture(
    "npm",
    ["pack", "--pack-destination", packDir, "--json"],
    join(root, "packages", pkg),
  );
  const info = JSON.parse(out)[0];
  const [scope, short] = info.name.split("/");
  tarballs.set(`/${scope}/${short}/-/${short}-${info.version}.tgz`, join(packDir, info.filename));
}

const registryServer = http.createServer(async (req, res) => {
  try {
    const path = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
    const local = tarballs.get(path);
    if (local) {
      res.writeHead(200, { "content-type": "application/octet-stream" });
      res.end(readFileSync(local));
      return;
    }
    // Everything else belongs to the real registry. Redirect rather than
    // proxying bytes through this process: npm follows the redirect, while
    // the shim stays limited to the two unpublished tarballs.
    res.writeHead(302, { location: `https://registry.npmjs.org${req.url}` });
    res.end();
  } catch (error) {
    res.writeHead(500);
    res.end(String(error?.stack ?? error));
  }
});
await new Promise((resolve) => registryServer.listen(0, "127.0.0.1", resolve));
const registry = `http://127.0.0.1:${registryServer.address().port}`;

try {
  for (const step of steps) {
    const cwd = join(learn, step);
    console.log(`check-learn-ts: ${step}`);
    // npm's replace-registry-host rewriting is not reliable across the npm
    // majors in use, so point the lock's resolved tarball URLs at the shim
    // for exactly the duration of npm ci and restore the committed bytes
    // immediately afterwards. The integrity hashes themselves do not move.
    const lockPath = join(cwd, "package-lock.json");
    const originalLock = readFileSync(lockPath, "utf8");
    writeFileSync(lockPath, originalLock.replaceAll("https://registry.npmjs.org", registry));
    try {
      run(
        "npm",
        ["ci", "--ignore-scripts", "--no-audit", "--no-fund", "--prefer-offline", "--registry", registry],
        cwd,
      );
    } finally {
      writeFileSync(lockPath, originalLock);
    }
    run("uv", ["sync", "--frozen"], cwd);
    run("npx", ["pylair", "codegen", "--module", "backend.app", "--out", "src/api.generated.ts"], cwd);
    run("npx", ["tsc", "--noEmit"], cwd);
    // Shipping and binary declarations are validated without building
    // installers or requiring the host GUI toolchain in the JS lane.
    if (/^0[45]-/.test(step)) run("node", [join(root, "scripts", "check-learn-build-config.mjs"), cwd], root);
  }
  run("node", [join(root, "scripts", "check-learn-sparse.mjs")], root);
} finally {
  registryServer.close();
  registryServer.closeAllConnections?.();
  rmSync(packDir, { recursive: true, force: true });
}
console.log(`check-learn-ts: ${steps.length} step(s) typechecked`);
