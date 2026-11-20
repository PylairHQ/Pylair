// Vendor the shared ui sources from packages/cli into src/ui/ before tsc.
//
// create-pylair must keep zero runtime dependencies (it runs via `npx
// create-pylair`), so it cannot import @pylair/cli. Copying at build time
// keeps one source of truth: edit packages/cli/src/ui/*, and the prebuild
// hook here refreshes the copy. test/parity.test.js asserts the copy is
// byte-identical, so the two can never drift silently.

import { copyFileSync, mkdirSync, writeFileSync } from "node:fs";

const FILES = ["caps.ts", "style.ts", "index.ts", "prompts.ts"];
const src = new URL("../../cli/src/ui/", import.meta.url);
const dst = new URL("../src/ui/", import.meta.url);

mkdirSync(dst, { recursive: true });
for (const f of FILES) copyFileSync(new URL(f, src), new URL(f, dst));
writeFileSync(
  new URL(".synced", dst),
  "vendored from packages/cli/src/ui by scripts/sync-ui.mjs; do not edit\n",
);
console.log(`sync-ui: copied ${FILES.length} files from packages/cli/src/ui`);
