import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after } from "node:test";
import { c as tarCreate } from "tar";

import {
  assertUpdaterVersionSync,
  canonArch,
  findUpdaterArtifacts,
  updateManifest,
} from "../dist/update.js";
import { loadTauriConfig } from "../dist/config.js";

const dirs = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), "pylair-update-"));
  dirs.push(d);
  return d;
};
after(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

const sha = (s) => createHash("sha256").update(s).digest("hex");

/** A real gzipped tar holding `<app>.app/Contents/Info.plist` with
 * CFBundleShortVersionString = plistVersion - what tauri bundle emits. */
function makeAppTar(dest, plistVersion) {
  const app = dest.split(/[\\/]/).pop().replace(/\.tar\.gz$/, "");
  const stage = tmp();
  mkdirSync(join(stage, app, "Contents"), { recursive: true });
  writeFileSync(
    join(stage, app, "Contents", "Info.plist"),
    `<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0">\n<dict>\n` +
      `<key>CFBundleShortVersionString</key>\n<string>${plistVersion}</string>\n` +
      `</dict>\n</plist>\n`,
  );
  tarCreate({ file: dest, cwd: stage, gzip: true, sync: true }, [app]);
}

/** A minimal updater-enabled app: conf (+ optionally Cargo.toml) and a
 * bundle tree with artifacts. `artifacts` = [dir, filename, body]; a
 * `.app.tar.gz` filename gets a REAL tarball (plist version plistVersion,
 * else the conf version) since `update manifest` reads it. */
function fakeApp({
  version = "0.2.0",
  cargoVersion,
  artifacts = [],
  endpoints,
  plistVersion,
} = {}) {
  const dir = tmp();
  const tauri = join(dir, "src-tauri");
  mkdirSync(tauri, { recursive: true });
  const conf = { productName: "up-app", version, identifier: "com.example.up" };
  if (endpoints !== undefined) conf.plugins = { updater: { endpoints } };
  writeFileSync(join(tauri, "tauri.conf.json"), JSON.stringify(conf, null, 2));
  if (cargoVersion !== undefined)
    writeFileSync(join(tauri, "Cargo.toml"), `[package]\nversion = "${cargoVersion}"\n`);
  for (const [kind, file, body] of artifacts) {
    const d = join(tauri, "target", "release", "bundle", kind);
    mkdirSync(d, { recursive: true });
    if (file.endsWith(".app.tar.gz")) makeAppTar(join(d, file), plistVersion ?? version);
    else writeFileSync(join(d, file), body);
  }
  return dir;
}

const TRIPLE_ART = (triple, kind, file, body) => ({ triple, kind, file, body });

function withTripleArtifacts(dir, artifacts) {
  for (const { triple, kind, file, body } of artifacts) {
    const d = join(dir, "src-tauri", "target", triple, "release", "bundle", kind);
    mkdirSync(d, { recursive: true });
    writeFileSync(join(d, file), body);
  }
}

const ALL_KINDS = [
  ["appimage", "up-app_0.2.0_amd64.AppImage", "linux-bytes"],
  ["macos", "up-app.app.tar.gz", "mac-bytes"],
  ["nsis", "up-app_0.2.0_x64-setup.exe", "win-bytes"],
];

// The .app.tar.gz name carries no arch - its platform key falls back to the
// host arch, so expectations differ on arm64 (macOS CI) vs x64.
const DARWIN = `darwin-${canonArch(process.arch)}`;

function addSigs(dir, skip = -1) {
  for (const [i, [kind, file]] of ALL_KINDS.entries()) {
    if (i === skip) continue;
    writeFileSync(join(dir, "src-tauri/target/release/bundle", kind, `${file}.sig`), `SIG-${kind}`);
  }
}

describe("findUpdaterArtifacts", () => {
  test("maps bundle kinds to platforms and parses versioned names", () => {
    const dir = fakeApp({ artifacts: ALL_KINDS });
    const got = findUpdaterArtifacts(join(dir, "src-tauri"));
    assert.deepEqual(
      got.map((a) => a.platform),
      [DARWIN, "linux-x86_64", "windows-x86_64"],
    );
    assert.equal(got.find((a) => a.platform === "linux-x86_64").version, "0.2.0");
    assert.equal(got.find((a) => a.platform === "windows-x86_64").version, "0.2.0");
    assert.equal(got.find((a) => a.platform === DARWIN).version, undefined);
  });

  test("arch comes from a --target triple dir when present", () => {
    const dir = fakeApp();
    withTripleArtifacts(dir, [
      TRIPLE_ART("aarch64-apple-darwin", "macos", "up-app.app.tar.gz", "b"),
    ]);
    const got = findUpdaterArtifacts(join(dir, "src-tauri"));
    assert.deepEqual(got.map((a) => a.platform), ["darwin-aarch64"]);
  });

  test("a universal-apple-darwin build registers under BOTH mac arches", () => {
    const dir = fakeApp();
    withTripleArtifacts(dir, [
      TRIPLE_ART("universal-apple-darwin", "macos", "up-app.app.tar.gz", "b"),
    ]);
    const got = findUpdaterArtifacts(join(dir, "src-tauri"));
    assert.deepEqual(got.map((a) => a.platform), ["darwin-aarch64", "darwin-x86_64"]);
    // Same payload under both keys.
    assert.equal(got[0].path, got[1].path);
  });

  test("a `_universal` arch token in the filename does the same", () => {
    const dir = fakeApp();
    withTripleArtifacts(dir, [
      TRIPLE_ART("any-triple", "macos", "up-app_universal.app.tar.gz", "b"),
    ]);
    const got = findUpdaterArtifacts(join(dir, "src-tauri"));
    assert.deepEqual(got.map((a) => a.platform), ["darwin-aarch64", "darwin-x86_64"]);
  });

  test("two artifacts for one platform throw", () => {
    const dir = fakeApp({
      artifacts: [
        ["appimage", "up-app_0.2.0_amd64.AppImage", "a"],
        ["appimage", "up-app_0.2.0_x86_64.AppImage", "b"],
      ],
    });
    assert.throws(
      () => findUpdaterArtifacts(join(dir, "src-tauri")),
      /multiple updater artifacts for linux-x86_64/,
    );
  });
});

describe("updateManifest", () => {
  test("happy path: platforms, signatures, urls and sha256 land in latest.json", () => {
    const dir = fakeApp({ artifacts: ALL_KINDS });
    addSigs(dir);
    const now = () => new Date("2026-11-24T12:00:00Z");
    const { file, platforms } = updateManifest({
      appDir: dir,
      baseUrl: "https://github.com/me/app/releases/latest/download/",
      notes: "hello",
      now,
    });
    assert.deepEqual(platforms, [DARWIN, "linux-x86_64", "windows-x86_64"]);
    const m = JSON.parse(readFileSync(file, "utf-8"));
    assert.equal(file, join(dir, "src-tauri/target/release/bundle/latest.json"));
    assert.equal(m.version, "0.2.0");
    assert.equal(m.notes, "hello");
    assert.equal(m.pub_date, "2026-11-24T12:00:00.000Z");
    const linux = m.platforms["linux-x86_64"];
    assert.equal(linux.signature, "SIG-appimage");
    assert.equal(
      linux.url,
      "https://github.com/me/app/releases/latest/download/up-app_0.2.0_amd64.AppImage",
    );
    assert.equal(linux.sha256, sha("linux-bytes"));
    assert.equal(m.platforms[DARWIN].url.endsWith("up-app.app.tar.gz"), true);
  });

  test("a universal macOS artifact lands under both darwin keys", () => {
    const dir = fakeApp();
    withTripleArtifacts(dir, [
      { triple: "universal-apple-darwin", kind: "macos", file: "up-app.app.tar.gz", body: "x" },
    ]);
    // Universal artifact: real tarball + sig, matching the conf version.
    const tarDir = join(dir, "src-tauri/target/universal-apple-darwin/release/bundle/macos");
    rmSync(join(tarDir, "up-app.app.tar.gz"));
    makeAppTar(join(tarDir, "up-app.app.tar.gz"), "0.2.0");
    writeFileSync(join(tarDir, "up-app.app.tar.gz.sig"), "SIG-mac");
    const { file, platforms } = updateManifest({ appDir: dir, baseUrl: "https://x/dl" });
    assert.deepEqual(platforms, ["darwin-aarch64", "darwin-x86_64"]);
    const m = JSON.parse(readFileSync(file, "utf-8"));
    for (const p of ["darwin-aarch64", "darwin-x86_64"]) {
      assert.equal(m.platforms[p].signature, "SIG-mac");
      assert.ok(m.platforms[p].url.endsWith("up-app.app.tar.gz"));
      assert.equal(m.platforms[p].sha256, m.platforms["darwin-aarch64"].sha256);
    }
  });

  test("--channel writes latest-<channel>.json", () => {
    const dir = fakeApp({ artifacts: ALL_KINDS });
    addSigs(dir);
    const { file } = updateManifest({
      appDir: dir,
      baseUrl: "https://x/download",
      channel: "beta",
    });
    assert.ok(file.endsWith("latest-beta.json"));
  });

  test("base url falls back to plugins.updater.endpoints[0] minus the filename", () => {
    const dir = fakeApp({
      artifacts: ALL_KINDS,
      endpoints: ["https://hosted.example/releases/latest/download/latest.json"],
    });
    addSigs(dir);
    const { file } = updateManifest({ appDir: dir });
    const m = JSON.parse(readFileSync(file, "utf-8"));
    assert.equal(
      m.platforms["linux-x86_64"].url,
      "https://hosted.example/releases/latest/download/up-app_0.2.0_amd64.AppImage",
    );
  });

  test("no artifacts fails naming the fix", () => {
    const dir = fakeApp();
    assert.throws(() => updateManifest({ appDir: dir, baseUrl: "https://x" }), /no updater artifacts/);
  });

  test("a missing .sig fails naming the artifact", () => {
    const dir = fakeApp({ artifacts: ALL_KINDS });
    addSigs(dir, 1); // macos unsigned
    assert.throws(
      () => updateManifest({ appDir: dir, baseUrl: "https://x" }),
      /unsigned updater artifact: up-app\.app\.tar\.gz/,
    );
  });

  test("an artifact whose version disagrees with tauri.conf.json fails", () => {
    const dir = fakeApp({
      version: "0.3.0",
      artifacts: [["appimage", "up-app_0.2.0_amd64.AppImage", "b"]],
    });
    writeFileSync(
      join(dir, "src-tauri/target/release/bundle/appimage/up-app_0.2.0_amd64.AppImage.sig"),
      "S",
    );
    assert.throws(
      () => updateManifest({ appDir: dir, baseUrl: "https://x" }),
      /artifact version mismatch: up-app_0\.2\.0_amd64\.AppImage is 0\.2\.0 but tauri\.conf\.json says 0\.3\.0/,
    );
  });

  test("a macOS tarball whose Info.plist version disagrees fails naming the file", () => {
    const dir = fakeApp({
      version: "0.2.0",
      plistVersion: "0.1.0",
      artifacts: [["macos", "up-app.app.tar.gz", null]],
    });
    writeFileSync(
      join(dir, "src-tauri/target/release/bundle/macos/up-app.app.tar.gz.sig"),
      "S",
    );
    assert.throws(
      () => updateManifest({ appDir: dir, baseUrl: "https://x" }),
      /artifact version mismatch: up-app\.app\.tar\.gz bundles CFBundleShortVersionString "0\.1\.0" but tauri\.conf\.json says 0\.2\.0/,
    );
  });

  test("a macOS artifact that isn't a tarball fails naming the file", () => {
    const dir = fakeApp();
    const macd = join(dir, "src-tauri/target/release/bundle/macos");
    mkdirSync(macd, { recursive: true });
    writeFileSync(join(macd, "up-app.app.tar.gz"), "not-a-tarball");
    writeFileSync(join(macd, "up-app.app.tar.gz.sig"), "S");
    assert.throws(
      () => updateManifest({ appDir: dir, baseUrl: "https://x" }),
      /up-app\.app\.tar\.gz: not a readable \.app\.tar\.gz/,
    );
  });

  test("http base url fails without --insecure-dev and passes with it", () => {
    const dir = fakeApp({ artifacts: ALL_KINDS });
    addSigs(dir);
    assert.throws(
      () => updateManifest({ appDir: dir, baseUrl: "http://127.0.0.1:8377" }),
      /--base-url must be https/,
    );
    const { file } = updateManifest({
      appDir: dir,
      baseUrl: "http://127.0.0.1:8377",
      insecureDev: true,
    });
    const m = JSON.parse(readFileSync(file, "utf-8"));
    assert.ok(m.platforms["linux-x86_64"].url.startsWith("http://127.0.0.1:8377/"));
  });

  test("no base url and no endpoint fails naming --base-url", () => {
    const dir = fakeApp({ artifacts: ALL_KINDS });
    addSigs(dir);
    assert.throws(() => updateManifest({ appDir: dir }), /--base-url/);
  });
});

describe("assertUpdaterVersionSync + loadTauriConfig.updater", () => {
  test("loadTauriConfig exposes plugins.updater only when configured", () => {
    const withU = fakeApp({ endpoints: ["https://x/latest.json"] });
    assert.ok(loadTauriConfig(withU).updater);
    const without = fakeApp();
    assert.equal(loadTauriConfig(without).updater, undefined);
  });

  test("matching conf and cargo versions pass", () => {
    const dir = fakeApp({ version: "0.2.0", cargoVersion: "0.2.0" });
    assertUpdaterVersionSync(join(dir, "src-tauri"), "0.2.0");
  });

  test("a mismatch throws naming both files and values", () => {
    const dir = fakeApp({ version: "0.2.0", cargoVersion: "0.1.0" });
    assert.throws(
      () => assertUpdaterVersionSync(join(dir, "src-tauri"), "0.2.0"),
      /version mismatch: src-tauri\/tauri\.conf\.json says "0\.2\.0" but src-tauri\/Cargo\.toml \[package\] says "0\.1\.0"/,
    );
  });
});
