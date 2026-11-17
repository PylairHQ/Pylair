import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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
  assert.match(py, /pylair==0\.1\.0rc10/);
  const pkg = readFileSync(join(app, "package.json"), "utf8");
  assert.match(pkg, /"@pylair\/client": "0\.1\.0-rc\.20"/);
  assert.match(pkg, /"@pylair\/cli": "0\.1\.0-rc\.20"/);
  const lock = readFileSync(join(app, "src-tauri/Cargo.lock"), "utf8");
  assert.match(lock, /name = "my-app"/);
  assert.match(lock, /name = "tauri-plugin-pylair"\nversion = "0\.1\.0-rc\.15"/);
  // Tripwire: the shipped lock must carry exactly the crate version the
  // scaffold pins in Cargo.toml, or a registry-mode build re-resolves.
  const cargo = readFileSync(join(app, "src-tauri/Cargo.toml"), "utf8");
  const pinned = cargo.match(/tauri-plugin-pylair = "=([^"]+)"/);
  assert.ok(pinned, "scaffolded Cargo.toml has no exact tauri-plugin-pylair pin");
  assert.ok(
    lock.includes(`name = "tauri-plugin-pylair"\nversion = "${pinned[1]}"\n`),
    `template Cargo.lock does not carry the pinned crate ${pinned[1]}`,
  );
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

const REPO = fileURLToPath(new URL("../../..", import.meta.url));

test("--mode wrap needs --local until wrap mode is published", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold(["w-app", "--mode", "wrap"], dir);
  assert.equal(res.status, 1);
  assert.match(res.stderr, /wrap mode is not in the published Pylair packages yet/);
  assert.match(res.stderr, /--local/);
  assert.ok(!existsSync(join(dir, "w-app")), "a refused scaffold must create nothing");
  rmSync(dir, { recursive: true });
});

test("--mode wrap --local scaffolds a wrap app with no React and no remote capability", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold(["w-app", "--mode", "wrap", "--local", REPO], dir);
  assert.equal(res.status, 0, res.stderr);
  const app = join(dir, "w-app");
  for (const f of [
    "pyproject.toml",
    "app/main.py",
    "ui/index.html",
    "ui/restarting.html",
    "ui/status.js",
    "src-tauri/Cargo.toml",
    "src-tauri/tauri.conf.json",
    "src-tauri/capabilities/default.json",
    "src-tauri/src/lib.rs",
    ".gitignore",
  ])
    assert.ok(existsSync(join(app, f)), `missing ${f}`);
  for (const f of ["src/App.tsx", "vite.config.ts", "index.html"])
    assert.ok(!existsSync(join(app, f)), `wrap template must not ship ${f}`);
  const py = readFileSync(join(app, "pyproject.toml"), "utf8");
  assert.match(py, /mode = "wrap"/);
  assert.match(py, /framework = "command"/);
  const conf = JSON.parse(readFileSync(join(app, "src-tauri/tauri.conf.json"), "utf8"));
  assert.equal(conf.app.windows[0].create, false, "the plugin builds the window");
  // Permanent: the wrapped page is a remote origin and must get no IPC, so
  // no capability may name a remote url.
  const caps = JSON.parse(readFileSync(join(app, "src-tauri/capabilities/default.json"), "utf8"));
  assert.equal(caps.remote, undefined);
  assert.deepEqual(caps.windows, ["main"]);
  rmSync(dir, { recursive: true });
});

test("--mode must be bridge or wrap, and the bridge template is untouched", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const bad = scaffold(["x-app", "--mode", "hybrid"], dir);
  assert.equal(bad.status, 1);
  assert.match(bad.stderr, /--mode must be "bridge" or "wrap"/);
  const ok = scaffold(["b-app", "--mode", "bridge"], dir);
  assert.equal(ok.status, 0, ok.stderr);
  assert.ok(existsSync(join(dir, "b-app/src/App.tsx")));
  assert.ok(!existsSync(join(dir, "b-app/ui")));
  rmSync(dir, { recursive: true });
});

test("--into adds the wrap shell to an existing project without overwriting anything", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  writeFileSync(join(dir, "pyproject.toml"), "# mine\n");
  writeFileSync(join(dir, "package.json"), "{}\n");
  const res = scaffold(["--mode", "wrap", "--into", dir, "--name", "mine", "--local", REPO], dir);
  assert.equal(res.status, 0, res.stderr);
  const out = JSON.parse(res.stdout.trim().split("\n").pop());
  assert.ok(out.kept.includes("package.json"));
  assert.ok(out.written.includes("src-tauri/tauri.conf.json"));
  assert.equal(readFileSync(join(dir, "pyproject.toml"), "utf8"), "# mine\n");
  assert.equal(readFileSync(join(dir, "package.json"), "utf8"), "{}\n");
  assert.ok(!existsSync(join(dir, "app")), "no sample app in an existing project");
  assert.notEqual(scaffold(["--mode", "wrap", "--into", dir, "--name", "Bad Name", "--local", REPO], dir).status, 0);
  rmSync(dir, { recursive: true });
});
