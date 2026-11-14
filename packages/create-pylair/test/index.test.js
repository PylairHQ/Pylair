import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const bin = fileURLToPath(new URL("../dist/index.js", import.meta.url));

function scaffold(args, cwd) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8", cwd });
}

test("scaffolds a complete app skeleton", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold(["my-app"], dir);
  assert.equal(res.status, 0, res.stderr);
  const app = join(dir, "my-app");
  for (const f of [
    "pyproject.toml",
    "requirements.txt",
    "package.json",
    "backend/app.py",
    "src-tauri/Cargo.toml",
    "src-tauri/Cargo.lock",
    "src-tauri/tauri.conf.json",
    "src-tauri/src/lib.rs",
    "src-tauri/capabilities/default.json",
    "src-tauri/icons/icon.icns",
    "src/App.tsx",
    "index.html",
    // npm strips .gitignore from tarballs — the template ships it as
    // _gitignore and the scaffolder renames it. Pin the rename.
    ".gitignore",
  ]) {
    assert.ok(existsSync(join(app, f)), `missing ${f}`);
  }
  const conf = JSON.parse(readFileSync(join(app, "src-tauri/tauri.conf.json"), "utf8"));
  assert.equal(conf.productName, "my-app");
  assert.equal(conf.identifier, "com.example.myapp");
  // The shipped .gitignore must keep real env files out of git while
  // still tracking an .env.example.
  const gi = readFileSync(join(app, ".gitignore"), "utf8").split("\n").map((l) => l.trim());
  for (const line of [".env", ".env.*", "!.env.example"])
    assert.ok(gi.includes(line), `.gitignore missing "${line}"`);
  const py = readFileSync(join(app, "pyproject.toml"), "utf8");
  assert.match(py, /app-module = "backend\.app"/);
  assert.match(py, /pylair==0\.1\.0rc9/);
  const pkg = readFileSync(join(app, "package.json"), "utf8");
  assert.match(pkg, /"@pylair\/client": "0\.1\.0-rc\.15"/);
  assert.match(pkg, /"@pylair\/cli": "0\.1\.0-rc\.15"/);
  const lock = readFileSync(join(app, "src-tauri/Cargo.lock"), "utf8");
  assert.match(lock, /name = "my-app"/);
  assert.match(lock, /name = "tauri-plugin-pylair"\nversion = "0\.1\.0-rc\.11"/);
  // Webviews may listen but must not inject events — a forged
  // pylair://stream terminal resolves another window's call.
  const caps = readFileSync(join(app, "src-tauri/capabilities/default.json"), "utf8");
  assert.match(caps, /core:event:deny-emit"/);
  assert.match(caps, /core:event:deny-emit-to"/);
  rmSync(dir, { recursive: true });
});

test("--local wires all three deps at the checkout", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold(["local-app", "--local", "/fake/repo"], dir);
  assert.equal(res.status, 0, res.stderr);
  const app = join(dir, "local-app");
  // On Windows "/fake/repo" resolves under a drive letter; the file URI
  // must still have three slashes (file:///C:/fake/...).
  assert.match(readFileSync(join(app, "pyproject.toml"), "utf8"), /file:\/\/\/(?:[A-Za-z]:\/)?fake\/repo\/python\/pylair/);
  assert.match(readFileSync(join(app, "package.json"), "utf8"), /file:(?:[A-Za-z]:)?\/fake\/repo\/packages\/client/);
  assert.match(
    readFileSync(join(app, "src-tauri/Cargo.toml"), "utf8"),
    /tauri-plugin-pylair = \{ path = "(?:[A-Za-z]:)?\/fake\/repo\/crates\/tauri-plugin-pylair" \}/,
  );
  rmSync(dir, { recursive: true });
});

test("rejects bad names and non-empty dirs", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  assert.notEqual(scaffold(["Bad Name"], dir).status, 0);
  assert.equal(scaffold(["ok-app"], dir).status, 0);
  assert.notEqual(scaffold(["ok-app"], dir).status, 0); // now non-empty
  rmSync(dir, { recursive: true });
});

test("no args fails loudly when stdin is not a TTY", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold([], dir);
  assert.equal(res.status, 1);
  assert.match(res.stderr, /app name required/);
  rmSync(dir, { recursive: true });
});
