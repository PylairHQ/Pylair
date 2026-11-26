// Vendor the shared sources from packages/cli into src/ before tsc.
//
// create-pylair must keep zero runtime dependencies (it runs via `npx
// create-pylair`), so it cannot import @pylair/cli. Copying at build time
// keeps one source of truth: edit packages/cli/src/{ui/*,preflight.ts},
// and the prebuild hook here refreshes the copies. test/parity.test.js
// asserts each copy is byte-identical, so the two can never drift
// silently.

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

// preflight.ts is the toolchain check `pylair dev` gates the Rust build
// on and this package warns about after scaffolding — one file at src
// root, vendored like the ui module above.
const SRC_TOP = ["preflight.ts", "compat.ts", "compat.json"];
const srcTop = new URL("../../cli/src/", import.meta.url);
const dstTop = new URL("../src/", import.meta.url);
for (const f of SRC_TOP) copyFileSync(new URL(f, srcTop), new URL(f, dstTop));

console.log(
  `sync-ui: copied ${FILES.length} ui files + ${SRC_TOP.join(", ")} from packages/cli/src`,
);
