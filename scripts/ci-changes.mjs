#!/usr/bin/env node
// CI change filter: decide whether a push or pull request touches anything
// beyond documentation, so ci.yml can skip its heavy jobs for docs-only
// changes. Writes `code=true|false` to $GITHUB_OUTPUT (and prints it).
//
// Docs-only means EVERY changed path is under docs/, is a *.md or LICENSE*
// file at the repository root, or is a *.md file under .github/. Anything
// else, an empty diff, a manual or scheduled run, or any trouble working
// the diff out, is `code=true`: when in doubt the full suite runs.
// Markdown elsewhere is NOT docs-only: package READMEs ship in the npm
// tarballs, python/pylair/README.md is the PyPI long description, and
// files under packages/create-pylair/templates/ are copied verbatim into scaffolds.
//
// Environment (set by the workflow): EVENT, BASE_SHA, HEAD_SHA (pull
// requests), BEFORE_SHA, AFTER_SHA (pushes).

import { execFileSync } from "node:child_process";
import { appendFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

/** Is this repo-relative path documentation only? */
export function isDocsPath(path) {
  const p = path.replace(/\\/g, "/");
  const base = p.slice(p.lastIndexOf("/") + 1);
  if (p.startsWith("docs/")) return true;
  if (!p.includes("/")) return /\.md$/i.test(base) || /^LICENSE/.test(base);
  return p.startsWith(".github/") && /\.md$/i.test(base);
}

/** `true` when the change needs the full suite, `false` when it is docs only. */
export function needsCode(files) {
  return files.length === 0 || files.some((f) => !isDocsPath(f));
}

const ZERO = /^0+$/;

/** The git revision range for this event, or undefined when there is none
 * to trust (then everything runs). */
export function diffRange(env) {
  switch (env.EVENT) {
    case "pull_request":
      return env.BASE_SHA && env.HEAD_SHA ? `${env.BASE_SHA}...${env.HEAD_SHA}` : undefined;
    case "push":
      return env.BEFORE_SHA && !ZERO.test(env.BEFORE_SHA) && env.AFTER_SHA
        ? `${env.BEFORE_SHA}..${env.AFTER_SHA}`
        : undefined;
    default:
      return undefined; // workflow_dispatch, schedule: always everything
  }
}

function changedFiles(range) {
  const out = execFileSync("git", ["diff", "--name-only", "-z", range], { encoding: "utf8" });
  return out.split("\0").filter(Boolean);
}

function main() {
  const range = diffRange(process.env);
  let code = true;
  let why = `no trusted diff range for event ${process.env.EVENT ?? "?"}: running everything`;
  if (range) {
    try {
      const files = changedFiles(range);
      code = needsCode(files);
      why = `${files.length} changed file(s) in ${range}: ${code ? "code changed" : "docs only"}`;
      if (!code) console.log(files.map((f) => `  ${f}`).join("\n"));
    } catch (e) {
      why = `could not diff ${range} (${e.message.split("\n")[0]}): running everything`;
    }
  }
  console.log(`changes: ${why}`);
  console.log(`code=${code}`);
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `code=${code}\n`);
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) main();
