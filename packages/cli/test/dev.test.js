import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";

import { codegenEnv } from "../dist/dev.js";
import { importRoot } from "../dist/config.js";
import { formatMissing, missingTools, preflightChecks } from "../dist/preflight.js";

describe("codegenEnv", () => {
  test("PYTHONPATH is the configured source's import root", () => {
    // source = "server" → the module `server.app` resolves from appDir,
    // the PARENT of the source dir — not <appDir>/server itself.
    const env = codegenEnv("/app", "server");
    assert.equal(env.PYTHONPATH, importRoot("/app", "server"));
    assert.equal(env.PYTHONPATH, join("/app"));
    assert.notEqual(env.PYTHONPATH, join("/app", "server"));
  });

  test("nested source dirs resolve their parent", () => {
    const env = codegenEnv("/app", "src/backend");
    assert.equal(env.PYTHONPATH, join("/app", "src"));
  });
});

describe("preflightChecks", () => {
  // The fake env answers like a clean machine unless told otherwise —
  // same injectable env shape nativeToolchain already takes.
  const env = (over = {}) => ({
    platform: "linux",
    run: () => "ok 1.0",
    exists: () => true,
    list: () => [],
    env: {},
    ...over,
  });

  test("a clean machine reports node, uv, cargo and the toolchain ok", () => {
    const items = preflightChecks(env());
    assert.deepEqual(
      items.map((i) => i.name),
      ["node", "uv", "rust/cargo", "native toolchain"],
    );
    assert.deepEqual(missingTools(items), []);
  });

  test("missing uv and cargo land in missingTools with their fixes", () => {
    const items = preflightChecks(
      env({ run: (cmd) => (cmd === "uv" || cmd === "cargo" ? null : "ok 1.0") }),
    );
    const missing = missingTools(items);
    assert.deepEqual(
      missing.map((m) => m.name),
      ["uv", "rust/cargo"],
    );
    const lines = formatMissing(missing);
    assert.match(lines[0], /^uv: not found - install uv: https:\/\/docs\.astral\.sh\/uv\//);
    assert.match(lines[1], /^rust\/cargo: not found - install Rust: https:\/\/rustup\.rs/);
  });

  test("a missing native toolchain is reported through the same list", () => {
    // No cc/pkg-config answers on linux → the toolchain item fails with
    // the apt-get fix nativeToolchain produces.
    const items = preflightChecks(env({ run: () => null }));
    const missing = missingTools(items);
    assert.deepEqual(
      missing.map((m) => m.name),
      ["uv", "rust/cargo", "native toolchain"],
    );
    assert.match(
      formatMissing(missing).find((l) => l.startsWith("native toolchain")),
      /apt-get install/,
    );
  });

  test("formatMissing emits one 'name: detail - fix' line per miss", () => {
    const lines = formatMissing([
      { name: "uv", ok: false, detail: "not found", fix: "install uv" },
      { name: "x", ok: false, detail: "absent" },
    ]);
    assert.deepEqual(lines, ["uv: not found - install uv", "x: absent"]);
  });
});

describe("codegen status file (docs/inspector.md)", () => {
  test("writeCodegenStatus writes parseable status under .pylair", async () => {
    const { writeCodegenStatus, codegenStatusPath } = await import("../dist/dev.js");
    const { mkdtempSync, readFileSync } = await import("node:fs");
    const { tmpdir } = await import("node:os");
    const dir = mkdtempSync(join(tmpdir(), "pylair-cgstat-"));
    writeCodegenStatus(dir, { ok: false, error: "boom" });
    const raw = JSON.parse(readFileSync(codegenStatusPath(dir), "utf-8"));
    assert.equal(raw.ok, false);
    assert.equal(raw.error, "boom");
    assert.equal(typeof raw.at, "number");
    writeCodegenStatus(dir, { ok: true, functions: 3 });
    const ok = JSON.parse(readFileSync(codegenStatusPath(dir), "utf-8"));
    assert.equal(ok.ok, true);
    assert.equal(ok.functions, 3);
  });

  test("the path is .pylair/codegen-status.json under the app dir", async () => {
    const { codegenStatusPath } = await import("../dist/dev.js");
    assert.equal(codegenStatusPath("/app"), join("/app", ".pylair", "codegen-status.json"));
  });
});
