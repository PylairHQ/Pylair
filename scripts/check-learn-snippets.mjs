#!/usr/bin/env node
/* Snippet-drift check for the learning path.
 *
 * A marked snippet in docs/learn/*.md looks like:
 *
 *   <!-- snippet: 01-first-app/backend/app.py -->
 *   ```python
 *   ...
 *   ```
 *
 * The fenced body must appear byte-for-byte (LF-normalized, trailing
 * whitespace ignored) inside examples/learn/<path>. This keeps the guides
 * honest: paste real code, then notice when the example changes.
 */

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const docs = join(root, "docs", "learn");
const examples = join(root, "examples", "learn");

if (!existsSync(docs)) {
  console.error("check-learn-snippets: docs/learn does not exist");
  process.exit(1);
}

const pattern =
  /<!--\s*snippet:\s*([^\s>]+)\s*-->\s*\n```[^\n]*\n([\s\S]*?)\n```/g;
const failures = [];
let count = 0;

for (const file of readdirSync(docs).filter((f) => f.endsWith(".md"))) {
  const text = readFileSync(join(docs, file), "utf8");
  for (const match of text.matchAll(pattern)) {
    count += 1;
    const rel = match[1];
    const snippet = match[2].replace(/\r\n/g, "\n").trimEnd();
    const target = join(examples, rel);
    if (!existsSync(target)) {
      failures.push(`${file}: ${rel}: example file does not exist`);
      continue;
    }
    const body = readFileSync(target, "utf8").replace(/\r\n/g, "\n");
    if (!body.includes(snippet)) {
      failures.push(`${file}: ${rel}: marked snippet is not verbatim in the example`);
    }
  }
}

if (count === 0) {
  failures.push("no marked learning-path snippets found");
}

if (failures.length) {
  console.error(`check-learn-snippets: ${failures.length} failure(s)`);
  for (const failure of failures) console.error(`  ${failure}`);
  process.exit(1);
}
console.log(`check-learn-snippets: ${count} snippets verified`);
