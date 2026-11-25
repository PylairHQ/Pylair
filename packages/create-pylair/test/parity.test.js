import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const SRC = fileURLToPath(new URL("../src/", import.meta.url));
const CLI_UI = fileURLToPath(new URL("../../cli/src/ui/", import.meta.url));

/** The ui module is vendored at build time (scripts/sync-ui.mjs), not
 * imported from @pylair/cli: create-pylair must stay dependency-free for
 * `npx`. This is the drift tripwire: if packages/cli/src/ui changes and the
 * vendored copy is stale, the copy was not regenerated. `pretest` builds,
 * so a red run here means the sync hook did not run. */
test("the vendored ui module is byte-identical to packages/cli/src/ui", () => {
  for (const f of ["caps.ts", "style.ts", "index.ts", "prompts.ts"]) {
    assert.equal(
      readFileSync(join(SRC, "ui", f), "utf8"),
      readFileSync(join(CLI_UI, f), "utf8"),
      `${f} drifted from packages/cli/src/ui; run pnpm build to re-sync`,
    );
  }
});

const CLI_SRC = fileURLToPath(new URL("../../cli/src/", import.meta.url));

/** preflight.ts is vendored the same way (sync-ui.mjs, single file at
 * src root): create-pylair's post-scaffold tool check is literally
 * `pylair dev`'s pre-build gate. */
test("the vendored preflight module is byte-identical to packages/cli/src", () => {
  assert.equal(
    readFileSync(join(SRC, "preflight.ts"), "utf8"),
    readFileSync(join(CLI_SRC, "preflight.ts"), "utf8"),
    "preflight.ts drifted from packages/cli/src; run pnpm build to re-sync",
  );
});

/** The character policy: printed strings are ASCII except the shared
 * symbol set (vendored, guarded by the CLI's own tests) and the wordmark's
 * box-drawing art (fancy+unicode only). No em dashes, no stray Unicode. */
test("create-pylair sources contain no non-ASCII outside the wordmark", () => {
  const WORDMARK_CHARS = "┌─┐┬│├┘└┤┴";
  for (const f of readdirSync(SRC).filter((f) => f.endsWith(".ts"))) {
    const nonAscii = [...readFileSync(join(SRC, f), "utf8")].filter((c) => c.charCodeAt(0) > 0x7f);
    if (f === "wordmark.ts") {
      for (const c of nonAscii)
        assert.ok(WORDMARK_CHARS.includes(c), `wordmark.ts: '${c}' is outside the box-drawing set`);
    } else {
      assert.deepEqual(nonAscii, [], `${f} contains non-ASCII characters: ${nonAscii.join("")}`);
    }
  }
});
