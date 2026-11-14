#!/usr/bin/env node
// Decides whether the `latest` dist-tag may move to `version` for a
// package, then applies it.
//
// The rule:
//   - While NO stable (non-prerelease) version exists on the registry,
//     `latest` points at the newest RC — `npm create pylair@latest` and
//     `npm i @pylair/client` must resolve during the RC train.
//   - Once ANY stable version exists, a prerelease publish must NOT
//     move `latest` — only `next`.
//   - A stable publish always moves `latest`.
//
// Usage: npm-latest-guard.mjs <package-name> <version>
//   Queries `npm view <name> versions --json` itself. `--plan` prints
//   the decision without touching the registry.

import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export function mayMoveLatest(registryVersions, version) {
  const isPrerelease = version.includes("-");
  const stableExists = registryVersions.some((v) => !v.includes("-"));
  return !isPrerelease || !stableExists;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [name, version, ...rest] = process.argv.slice(2);
  if (!name || !version) {
    console.error("usage: npm-latest-guard.mjs <name> <version> [--plan]");
    process.exit(2);
  }
  const out = execFileSync("npm", ["view", name, "versions", "--json"], {
    encoding: "utf8",
  });
  const parsed = JSON.parse(out);
  const versions = Array.isArray(parsed) ? parsed : [parsed];
  if (mayMoveLatest(versions, version)) {
    console.log(`moving latest -> ${name}@${version}`);
    if (!rest.includes("--plan"))
      execFileSync("npm", ["dist-tag", "add", `${name}@${version}`, "latest"], {
        stdio: "inherit",
      });
  } else {
    console.log(
      `registry has a stable release — prerelease ${version} leaves ` +
        `latest untouched for ${name} (next only)`,
    );
  }
}
