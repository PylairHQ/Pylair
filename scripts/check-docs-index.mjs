// docs/README.md is the documentation index — CI fails when it drifts:
// every docs/decisions/NNNN-*.md number must be covered by it, and every
// docs/*.md guide must be linked from it. Runs from the repo root.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const docsDir = join(root, "docs");
const index = readFileSync(join(docsDir, "README.md"), "utf-8");

// Decision numbers the index claims to cover: every NNNN token plus every
// "NNNN-NNNN" range (the index writes "numbered records (0000-0011)").
const covered = new Set();
for (const m of index.matchAll(/\b(\d{4})-(\d{4})\b/g))
  for (let n = Number(m[1]); n <= Number(m[2]); n++) covered.add(n);
for (const m of index.matchAll(/\b(\d{4})\b/g)) covered.add(Number(m[1]));

const decisions = readdirSync(join(docsDir, "decisions"))
  .filter((f) => /^\d{4}-.*\.md$/.test(f))
  .sort();
const guides = readdirSync(docsDir).filter(
  (f) => f.endsWith(".md") && f !== "README.md",
);
const learnDir = join(docsDir, "learn");
const learnIndex = readFileSync(join(learnDir, "README.md"), "utf-8");
const learnGuides = readdirSync(learnDir).filter(
  (f) => f.endsWith(".md") && f !== "README.md",
);

const problems = [];
for (const f of decisions) {
  const n = Number(f.slice(0, 4));
  if (!covered.has(n))
    problems.push(
      `docs/decisions/${f} is not covered by docs/README.md (it claims ${summarizeCovered(covered)})`,
    );
}
for (const f of guides)
  if (!index.includes(`](${f})`))
    problems.push(`docs/${f} is not linked from docs/README.md`);
if (!index.includes("](learn/README.md)"))
  problems.push("docs/learn/README.md is not linked from docs/README.md");
for (const f of learnGuides)
  if (!learnIndex.includes(`](${f})`))
    problems.push(`docs/learn/${f} is not linked from docs/learn/README.md`);

function summarizeCovered(set) {
  const nums = [...set].sort((a, b) => a - b);
  if (!nums.length) return "no decision numbers";
  return `0000-${String(Math.max(...nums)).padStart(4, "0")}`;
}

if (problems.length) {
  console.error(`docs/README.md is stale:\n  ${problems.join("\n  ")}`);
  process.exit(1);
}
console.log(
  `docs index ok: ${decisions.length} decision record(s) covered, ${guides.length} guide(s) linked, ${learnGuides.length} learning guide(s) linked`,
);
