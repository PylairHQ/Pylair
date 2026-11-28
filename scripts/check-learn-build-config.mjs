#!/usr/bin/env node
/* Validate the learning examples' shipping configuration without requiring
 * the host's native GUI toolchain. This intentionally reuses the CLI's own
 * config, binary and version checks; it does not download binaries or build
 * installers. */

import { join, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const { loadConfig, loadTauriConfig } = await import(pathToFileURL(join(root, "packages/cli/dist/config.js")));
const { binaryChecks, versionChecks } = await import(pathToFileURL(join(root, "packages/cli/dist/doctor.js")));

let failed = false;
for (const arg of process.argv.slice(2)) {
  const dir = resolve(arg);
  const checks = [];
  const add = (name, ok, detail, hint) => checks.push({ name, ok, detail, hint });
  try {
    const cfg = loadConfig(dir);
    loadTauriConfig(dir);
    binaryChecks(dir, cfg, add);
    versionChecks(dir, add);
  } catch (error) {
    add("build configuration", false, error.message);
  }
  for (const check of checks) {
    if (!check.ok) failed = true;
    console.log(`${check.ok ? "ok" : "FAIL"} ${check.name}: ${check.detail}`);
  }
}
if (failed) process.exit(1);
