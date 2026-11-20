import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseBuildArgs, resolveAppDir } from "../dist/cli.js";
import { nativeToolchain } from "../dist/doctor.js";

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

describe("doctor: native toolchain", () => {
  const VSWHERE = join("C:\\Program Files (x86)", "Microsoft Visual Studio", "Installer", "vswhere.exe");
  const check = (platform, run, exists = () => false, list = () => []) =>
    nativeToolchain({
      platform,
      run,
      exists,
      list,
      env: { "ProgramFiles(x86)": "C:\\Program Files (x86)" },
    });

  test("windows: vswhere (not PATH) locates the MSVC C++ workload", () => {
    const r = check(
      "win32",
      (cmd, args) => {
        assert.equal(cmd, VSWHERE, "must probe the installed-vswhere path, not PATH");
        assert.ok(args.includes("Microsoft.VisualStudio.Component.VC.Tools.x86.x64"), args.join(" "));
        return "C:\\Program Files\\Microsoft Visual Studio\\2022\\BuildTools\n";
      },
      (p) => p === VSWHERE,
    );
    assert.equal(r.ok, true);
    assert.match(r.detail, /MSVC build tools at/);
  });

  test("windows: no Visual Studio Installer or no C++ workload fails with the fix", () => {
    const noInstaller = check("win32", () => null, () => false);
    assert.equal(noInstaller.ok, false);
    assert.match(noInstaller.fix, /Desktop development with C\+\+/);
    assert.match(noInstaller.fix, /Visual Studio Installer/);
    const noWorkload = check("win32", () => "", () => true);
    assert.equal(noWorkload.ok, false);
    assert.match(noWorkload.detail, /no MSVC C\+\+ build tools/);
    assert.match(noWorkload.fix, /Desktop development with C\+\+/);
  });

  test("windows: an install vswhere does not see is a broken install, not a toolchain", () => {
    // join() computes the same path the source builds on any host.
    const MSVC_DIR = join("C:\\Program Files", "Microsoft Visual Studio", "2022", "Community", "VC", "Tools", "MSVC");
    const LINK = join(MSVC_DIR, "14.44.35207", "bin", "Hostx64", "x64", "link.exe");
    const r = check(
      "win32",
      () => "",
      (p) => p === VSWHERE || p === LINK,
      (d) => (d === MSVC_DIR ? ["14.44.35207"] : []),
    );
    assert.equal(r.ok, false);
    assert.match(r.detail, /Community/);
    assert.match(r.detail, /does not list the install/);
    assert.match(r.fix, /Visual Studio Installer \(More > Repair\)/);
  });

  test("macos: xcode-select -p decides", () => {
    assert.equal(check("darwin", () => "/Library/Developer/CommandLineTools").ok, true);
    const r = check("darwin", () => null);
    assert.equal(r.ok, false);
    assert.match(r.fix, /xcode-select --install/);
  });

  test("linux: a C compiler AND the webkit2gtk-4.1 dev package are required", () => {
    const ok = check("linux", (cmd, args) => {
      if (args[0] === "--exists") return "";
      if (args[0] === "--modversion") return "2.46.5";
      if (cmd === "cc") return "cc (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0";
      return "1.8.1";
    });
    assert.equal(ok.ok, true);
    assert.match(ok.detail, /webkit2gtk-4\.1 2\.46\.5/);

    const noGtk = check("linux", (cmd, args) =>
      args[0] === "--exists" ? null : cmd === "cc" ? "cc 13.3.0" : "1.8.1",
    );
    assert.equal(noGtk.ok, false);
    assert.match(noGtk.detail, /webkit2gtk-4\.1/);
    assert.match(noGtk.fix, /libwebkit2gtk-4\.1-dev/);

    const noCc = check("linux", (cmd, args) =>
      ["cc", "gcc", "clang"].includes(cmd) ? null : args[0] === "--modversion" ? "2.46.5" : "",
    );
    assert.equal(noCc.ok, false);
    assert.match(noCc.detail, /C compiler/);
    assert.match(noCc.fix, /build-essential/);
  });

  test("linux: missing pkg-config cannot prove webkit2gtk, so both land in the fix", () => {
    const r = check("linux", (cmd) => (cmd === "pkg-config" ? null : "cc 13.3.0"));
    assert.equal(r.ok, false);
    assert.match(r.detail, /pkg-config/);
    assert.match(r.fix, /pkg-config.*libwebkit2gtk-4\.1-dev/);
  });
});
