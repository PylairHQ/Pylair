import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const bin = fileURLToPath(new URL("../dist/index.js", import.meta.url));
const REPO = fileURLToPath(new URL("../../..", import.meta.url));

const has = (cmd, args = ["--version"], shell = false) => {
  try {
    return spawnSync(cmd, args, { stdio: "pipe", shell }).status === 0;
  } catch {
    return false;
  }
};
// npm is npm.cmd on Windows — it only spawns through a shell there.
const isWinProbe = process.platform === "win32";
const tools = has("uv") && has("npm", ["--version"], isWinProbe);
const isWin = process.platform === "win32";

/** The DX-batch-1 contract: a fresh clone has no api.generated.ts, yet the
 * frontend typecheck must pass once `pylair build` has run — because the
 * build itself owns codegen. This drives the exact function build() calls
 * (runBuildCodegen, exported from @pylair/cli) against a real scaffold,
 * then runs the scaffold's own typecheck. Heavy on purpose: it is the
 * fresh-clone user journey, end to end. */
test(
  "a freshly scaffolded app typechecks after pylair build's codegen step",
  { skip: !tools, timeout: 600_000 },
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "create-pylair-codegen-"));
    try {
      const res = spawnSync(process.execPath, [bin, "t-app", "--local", REPO], {
        encoding: "utf8",
        cwd: dir,
      });
      assert.equal(res.status, 0, res.stderr);
      const app = join(dir, "t-app");
      const stubs = join(app, "src", "api.generated.ts");

      // The fresh-clone state: no generated file anywhere.
      assert.ok(!existsSync(stubs), "scaffold must not ship api.generated.ts");

      const sync = spawnSync("uv", ["sync"], { cwd: app, encoding: "utf8" });
      assert.equal(sync.status, 0, `uv sync: ${sync.stderr.slice(-400)}`);
      const venvPython = join(app, ".venv", isWin ? "Scripts/python.exe" : "bin/python");
      assert.ok(existsSync(venvPython), "uv sync created no venv python");

      // The exact step pylair build runs, with the venv standing in for the
      // bundled interpreter (the build passes resources/site-packages for
      // deps; the template backend only needs pylair itself, which the
      // venv has — a missing entry in PYTHONPATH is inert).
      const { runBuildCodegen } = await import(
        new URL("../../cli/dist/build.js", import.meta.url).href
      );
      const out = runBuildCodegen(
        app,
        { appModule: "backend.app", source: "backend" },
        venvPython,
        join(app, "src-tauri", "resources"),
      );
      assert.equal(out, stubs);
      assert.ok(existsSync(stubs), "codegen step wrote no stubs");
      const ts = readFileSync(stubs, "utf8");
      assert.match(ts, /export function hello\(/);
      assert.match(ts, /export function count\(.*AsyncGenerator/);
      assert.match(ts, /do not edit/);
      assert.match(ts, /never committed/);

      // The scaffolded typecheck itself: fresh clone + codegen output.
      const inst = spawnSync("npm", ["install", "--no-audit", "--no-fund", "--loglevel=error"], {
        cwd: app,
        encoding: "utf8",
        shell: isWin,
      });
      assert.equal(inst.status, 0, `npm install: ${inst.stderr.slice(-400)}`);
      const tsc = spawnSync(
        process.execPath,
        [join(app, "node_modules", "typescript", "bin", "tsc")],
        { cwd: app, encoding: "utf8" },
      );
      assert.equal(tsc.status, 0, `tsc: ${tsc.stdout.slice(-600)}${tsc.stderr.slice(-300)}`);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  },
);
