#!/usr/bin/env node
// CI check: every `uses:` in .github/workflows must be pinned to a
// 40-hex commit SHA, never a tag/branch name. With --remote, each pin
// is additionally verified to resolve to a COMMIT (not an annotated
// tag object — git/refs/tags returns the tag object's sha for those,
// which does not pin the code at all).
//
//   node scripts/check-action-pins.mjs           # offline: shape only
//   node scripts/check-action-pins.mjs --remote  # + ls-remote deref

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { execFileSync } from "node:child_process";

const workflowsDir = join(import.meta.dirname, "..", ".github", "workflows");
const remote = process.argv.includes("--remote");

const uses = []; // {file, line, repo, ref}
for (const f of readdirSync(workflowsDir).filter((f) => /\.ya?ml$/.test(f))) {
  readFileSync(join(workflowsDir, f), "utf-8")
    .split("\n")
    .forEach((line, i) => {
      // `uses:` in step (or job-level reusable-workflow) position only —
      // comments and prose mentioning `uses:` must not match.
      const m = line.match(/^\s*(?:-\s*)?uses:\s*([^\s#]+)/);
      if (m) uses.push({ file: f, line: i + 1, ref: m[1], repo: m[1].split("@")[0] });
    });
}

let bad = 0;
const fail = (u, msg) => {
  bad++;
  console.error(`${u.file}:${u.line}: ${u.ref} — ${msg}`);
};

const byRepo = new Map();
for (const u of uses) {
  const at = u.ref.lastIndexOf("@");
  u.repo = u.ref.slice(0, at);
  u.sha = u.ref.slice(at + 1);
  // Local actions (./...) are repo-relative — no pin required.
  if (u.repo.startsWith("./") || u.repo.startsWith("docker://")) continue;
  if (!/^[0-9a-f]{40}$/.test(u.sha)) fail(u, "not a 40-hex commit sha");
  else if (remote) {
    // Reusable-workflow refs carry a path after owner/repo — ls-remote
    // the repo, not the workflow path.
    const repo = u.repo.split("/").slice(0, 2).join("/");
    if (!byRepo.has(repo)) byRepo.set(repo, []);
    byRepo.get(repo).push(u);
  }
}

if (remote) {
  for (const [repo, pins] of byRepo) {
    // A sha is a commit when it appears as: a refs/tags/*^{} deref
    // target (annotated tag → commit), a refs/tags/* line with NO ^{}
    // counterpart (lightweight tag → the commit itself), or any
    // non-tag ref head. A refs/tags/* line that HAS a ^{} counterpart
    // is the tag object — pinning it pins nothing.
    const out = execFileSync(
      "git",
      ["ls-remote", `https://github.com/${repo}.git`],
      { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 },
    );
    const derefed = new Set(); // tag refs that have a ^{} line
    const lines = [];
    for (const l of out.split("\n")) {
      const [sha, ref] = l.split("\t");
      if (!ref) continue;
      if (ref.endsWith("^{}")) derefed.add(ref.slice(0, -3));
      else lines.push([sha, ref]);
    }
    const commits = new Set();
    for (const l of out.split("\n")) {
      const [sha, ref] = l.split("\t");
      if (ref?.endsWith("^{}")) commits.add(sha);
    }
    for (const [sha, ref] of lines)
      if (!ref.startsWith("refs/tags/") || !derefed.has(ref)) commits.add(sha);
    for (const u of pins)
      if (!commits.has(u.sha))
        fail(u, `not a commit in ${repo} — tag object or stale sha? (check refs/tags/<tag>^{})`);
  }
}

if (bad) {
  console.error(`\n${bad} unpinned or non-commit action ref(s)`);
  process.exit(1);
}
console.log(`${uses.length} uses: refs checked${remote ? " (remote-verified commits)" : ""} — all pinned to commit shas`);
