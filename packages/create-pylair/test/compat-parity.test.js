import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const bin = fileURLToPath(new URL("../dist/index.js", import.meta.url));

/** The parity tripwire: what a scaffold pins must be exactly the
 * compatibility manifest's current set, read from the SAME vendored copy
 * the scaffolder uses. The release-bump PR edits packages/cli/src/
 * compat.json and nothing else - if substitution, vendoring or the
 * template ever drift from it, this test goes red. */

const compat = JSON.parse(
  readFileSync(fileURLToPath(new URL("../src/compat.json", import.meta.url)), "utf8"),
);
const current = compat.releases.find((r) => r.npm === compat.current);

function scaffold(args, cwd) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8", cwd });
}

const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

test("the manifest is well-formed: current exists, newest first, every entry complete", () => {
  assert.ok(current, `current ${compat.current} has no release entry`);
  assert.equal(compat.releases[0].npm, compat.current, "newest first");
  for (const r of compat.releases) {
    assert.ok(r.npm && r.crate && r.pylair && r.date, `incomplete entry: ${JSON.stringify(r)}`);
    assert.ok(Array.isArray(r.notes), `notes must be an array in ${r.npm}`);
  }
});

for (const mode of ["bridge", "wrap"]) {
  test(`scaffolded ${mode} app pins exactly the manifest's current set`, () => {
    const dir = mkdtempSync(join(tmpdir(), "create-pylair-parity-"));
    const args = mode === "wrap" ? ["p-app", "--mode", "wrap"] : ["p-app"];
    const res = scaffold(args, dir);
    assert.equal(res.status, 0, res.stderr);
    const app = join(dir, "p-app");

    const py = readFileSync(join(app, "pyproject.toml"), "utf8");
    assert.match(
      py,
      new RegExp(`pylair==${esc(current.pylair)}`),
      `pyproject pins pylair ${current.pylair} (manifest current), got:\n${py}`,
    );

    const pkg = readFileSync(join(app, "package.json"), "utf8");
    // Wrap apps have no typed bridge - they pin the CLI but no client.
    const npmNames = mode === "wrap" ? ["@pylair/cli"] : ["@pylair/client", "@pylair/cli"];
    for (const name of npmNames) {
      assert.match(
        pkg,
        new RegExp(`"${esc(name)}": "${esc(current.npm)}"`),
        `package.json pins ${name} ${current.npm}`,
      );
    }

    const cargo = readFileSync(join(app, "src-tauri/Cargo.toml"), "utf8");
    assert.match(
      cargo,
      new RegExp(`tauri-plugin-pylair = "=${esc(current.crate)}"`),
      `Cargo.toml pins crate ${current.crate}`,
    );

    // The shipped lock must carry the same crate version, or a registry
    // build re-resolves to something the pins never vetted.
    const lock = readFileSync(join(app, "src-tauri/Cargo.lock"), "utf8");
    assert.ok(
      lock.includes(`name = "tauri-plugin-pylair"\nversion = "${current.crate}"`),
      `template Cargo.lock carries crate ${current.crate}`,
    );
    rmSync(dir, { recursive: true, force: true });
  });
}
