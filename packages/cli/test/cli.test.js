import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseBuildArgs, resolveAppDir } from "../dist/cli.js";

const cli = fileURLToPath(new URL("../dist/bin.js", import.meta.url));
const pkgVersion = JSON.parse(
  readFileSync(new URL("../package.json", import.meta.url), "utf8"),
).version;

test("--version prints version", () => {
  // From an unrelated cwd: the version comes from the package's own
  // package.json next to dist/, not from wherever the user stands.
  const out = execFileSync(process.execPath, [cli, "--version"], {
    encoding: "utf8",
    cwd: mkdtempSync(join(tmpdir(), "pylair-ver-")),
  });
  assert.equal(out.trim(), pkgVersion);
});

// A node_modules/.bin/pylair symlink is how pack-mode apps invoke the
// CLI — argv[1] is the symlink, not bin.js. Guards the bin entry.
test("bin runs through a .bin-style symlink", { skip: process.platform === "win32" }, () => {
  const shim = join(mkdtempSync(join(tmpdir(), "pylair-bin-")), "pylair");
  symlinkSync(cli, shim);
  const out = execFileSync(process.execPath, [shim, "--version"], { encoding: "utf8" });
  assert.equal(out.trim(), pkgVersion);
});

test("--help lists commands and exits 0", () => {
  const out = execFileSync(process.execPath, [cli, "--help"], { encoding: "utf8" });
  for (const cmd of ["dev", "build", "doctor", "codegen"]) {
    assert.ok(out.includes(cmd), `help output should list '${cmd}'`);
  }
});

describe("parseBuildArgs", () => {
  test("a pass-through --flag forwards its value", () => {
    const p = parseBuildArgs(["--config", "tauri.alt.conf.json"]);
    assert.deepEqual(p.tauriArgs, ["--config", "tauri.alt.conf.json"]);
  });

  test("--flag=value forwards unchanged", () => {
    const p = parseBuildArgs(["--config=tauri.alt.conf.json"]);
    assert.deepEqual(p.tauriArgs, ["--config=tauri.alt.conf.json"]);
  });

  test("everything after -- is forwarded verbatim", () => {
    const p = parseBuildArgs(["--verbose", "--", "--literal", "v", "-x"]);
    assert.deepEqual(p.tauriArgs, ["--verbose", "--literal", "v", "-x"]);
  });

  test("a flag followed by another flag takes no value", () => {
    const p = parseBuildArgs(["--verbose", "--debug"]);
    assert.deepEqual(p.tauriArgs, ["--verbose", "--debug"]);
  });

  test("own flags still parse and are not forwarded", () => {
    const p = parseBuildArgs([
      "--dir", "/some/app",
      "--config", "c.json",
      "--bundles", "msi,nsis",
      "--skip-fetch",
    ]);
    assert.equal(p.dir, "/some/app");
    assert.equal(p.bundles, "msi,nsis");
    assert.equal(p.skipFetch, true);
    assert.deepEqual(p.tauriArgs, ["--config", "c.json", "--bundles", "msi,nsis"]);
  });
});

describe("resolveAppDir", () => {
  const base = mkdtempSync(join(tmpdir(), "pylair-base-"));

  test("a relative --dir resolves against the current directory", () => {
    assert.equal(resolveAppDir("some-app", base), join(base, "some-app"));
    assert.equal(resolveAppDir("./some-app", base), join(base, "some-app"));
  });

  test("an absolute --dir is unchanged", () => {
    const abs = join(tmpdir(), "elsewhere", "app");
    assert.equal(resolveAppDir(abs, base), abs);
  });

  test("no --dir falls back to the current directory", () => {
    assert.equal(resolveAppDir(undefined, base), base);
    assert.equal(resolveAppDir(undefined), process.cwd());
  });

  test("a trailing slash and a .. segment normalise", () => {
    assert.equal(resolveAppDir("a/b/../c/", base), join(base, "a", "c"));
  });
});

test("doctor --dir ./some-app from the parent reports the same app as running inside it", () => {
  const parent = mkdtempSync(join(tmpdir(), "pylair-dir-"));
  const app = join(parent, "some-app");
  mkdirSync(join(app, "backend"), { recursive: true });
  mkdirSync(join(app, "src-tauri"), { recursive: true });
  writeFileSync(join(app, "pyproject.toml"), '[tool.pylair]\napp-module = "backend.app"\n');
  writeFileSync(join(app, "requirements.txt"), "");
  writeFileSync(
    join(app, "src-tauri", "tauri.conf.json"),
    JSON.stringify({ productName: "some-app", identifier: "com.example.someapp" }),
  );
  // doctor exits 1 here (no venv, maybe no uv/cargo): compare what it
  // says about the app, not the exit code.
  const appLines = (cwd, args) => {
    const r = spawnSync(process.execPath, [cli, "doctor", ...args], { cwd, encoding: "utf8" });
    return r.stdout
      .split(/\r?\n/)
      .filter((l) => /(\[tool\.pylair\] config|source dir|requirements|tauri\.conf\.json|venv|app dir)/.test(l));
  };
  try {
    const fromParent = appLines(parent, ["--dir", "./some-app"]);
    const fromInside = appLines(app, []);
    const sourceLine = fromParent.find((l) => l.includes("source dir"));
    assert.ok(sourceLine, `no source dir line in:\n${fromParent.join("\n")}`);
    assert.match(sourceLine, /^ ok /, "the app's source dir must be found");
    // Compare shapes, not the literal temp path: macOS reports /var as
    // /private/var once a process has chdir'd into it.
    const reported = sourceLine.slice(sourceLine.indexOf("source dir") + "source dir".length).trim();
    assert.ok(isAbsolute(reported), `source dir must be an absolute path: ${reported}`);
    assert.ok(reported.endsWith(join("some-app", "backend")), `unexpected source dir: ${reported}`);
    assert.deepEqual(fromParent, fromInside);
  } finally {
    rmSync(parent, { recursive: true, force: true });
  }
});
