import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir, platform } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { deflateRawSync, gzipSync } from "node:zlib";
import { execFileSync } from "node:child_process";

import {
  computeLoaderRpaths,
  copyDirectoryUnit,
  extractUnitArchive,
  inventoryArchive,
  isElfBytes,
  isMachOBytes,
  rewriteUnitLoaders,
  verifyDirectoryManifest,
} from "../dist/build/units.js";
import { prepareDevBinaries, stageBuildBinaries, writeBinariesManifest } from "../dist/build.js";
import { binaryChecks } from "../dist/doctor.js";

const sha = (buf) => createHash("sha256").update(buf).digest("hex");
const tmp = (tag) => mkdtempSync(join(tmpdir(), `pylair-unittest-${tag}-`));

function buildZip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const { name, data = Buffer.alloc(0), method = 0, mode = 0o100644, link } of entries) {
    const payload = link !== undefined ? Buffer.from(link) : data;
    const nameBuf = Buffer.from(name, "utf-8");
    const content = method === 8 ? deflateRawSync(payload) : payload;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(content.length, 18);
    local.writeUInt32LE(payload.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    locals.push(local, nameBuf, content);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(content.length, 20);
    central.writeUInt32LE(payload.length, 24);
    central.writeUInt16LE(nameBuf.length, 28);
    central.writeUInt32LE(((link !== undefined ? 0o120777 : mode) << 16) >>> 0, 38);
    central.writeUInt32LE(offset, 42);
    centrals.push(Buffer.concat([central, nameBuf]));
    offset += 30 + nameBuf.length + content.length;
  }
  const cd = Buffer.concat(centrals);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(cd.length, 12);
  eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cd, eocd]);
}

function tarHeader(name, size, type, mode = 0o755, linkname = "") {
  const h = Buffer.alloc(512);
  h.write(name, 0, "utf-8");
  h.write(mode.toString(8).padStart(7, "0"), 100, "ascii");
  h.write("0000000", 108, "ascii");
  h.write("0000000", 116, "ascii");
  h.write(size.toString(8).padStart(11, "0"), 124, "ascii");
  h.write("00000000000", 136, "ascii");
  h.write("        ", 148, "ascii");
  h.write(type, 156, "ascii");
  h.write(linkname, 157, "utf-8");
  h.write("ustar\0", 257, "ascii");
  h.write("00", 263, "ascii");
  let sum = 0;
  for (const b of h) sum += b;
  h.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, "ascii");
  return h;
}

function buildTarGz(entries) {
  const chunks = [];
  for (const e of entries) {
    const data = e.data ?? Buffer.alloc(0);
    chunks.push(tarHeader(e.name, data.length, e.type ?? "0", e.mode, e.linkname ?? ""));
    chunks.push(data);
    const pad = (512 - (data.length % 512)) % 512;
    if (pad) chunks.push(Buffer.alloc(pad));
  }
  chunks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(chunks));
}

function manifestFor(files) {
  return Object.fromEntries(
    Object.entries(files).map(([name, content]) => [name, sha(Buffer.from(content))]),
  );
}

describe("unit archive extraction safety", () => {
  const exactText = (name) =>
    `error: archive for binaries.unit contains unsafe entry ${JSON.stringify(name)}
` +
    `  archives may only carry relative paths inside the unit tree`;
  const exact = (name) => (e) => e.message === exactText(name);


  for (const bad of ["/abs/x", "C:/x", "../escape", "a/../../b", "back\\slash"]) {
    test(`rejects ${bad}`, () => {
      const dir = tmp("unsafe");
      assert.throws(
        () => extractUnitArchive(buildZip([{ name: bad, data: Buffer.from("x") }]), dir, "unit"),
        exact(bad),
      );
      assert.equal(existsSync(join(dir, "x")), false, "nothing staged");
      rmSync(dir, { recursive: true, force: true });
    });
  }

  test("rejects escaping symlinks and hard links, naming the entry", () => {
    for (const [type, entry] of [
      ["2", "linkout"],
      ["1", "hardout"],
    ]) {
      const tgz = buildTarGz([
        { name: "ok", data: Buffer.from("ok") },
        { name: entry, type, linkname: "../evil" },
      ]);
      const dir = tmp("links");
      assert.throws(() => extractUnitArchive(tgz, dir, "unit"), exact(entry));
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("preserves an in-unit symlink and executable bits", () => {
    const tgz = buildTarGz([
      { name: "bin/tool", data: Buffer.from("#!/bin/sh\n"), mode: 0o755 },
      { name: "bin/alias", type: "2", linkname: "tool" },
    ]);
    const dir = tmp("inunit");
    extractUnitArchive(tgz, dir, "unit");
    assert.equal(readlinkSync(join(dir, "bin", "alias")), "tool");
    if (platform() !== "win32") assert.ok(statSync(join(dir, "bin", "tool")).mode & 0o111);
    rmSync(dir, { recursive: true, force: true });
  });

  test("zip extraction keeps layout and re-staging removes stale files", () => {
    const dir = tmp("zipunit");
    extractUnitArchive(
      buildZip([
        { name: "bin/tool", data: Buffer.from("one"), mode: 0o100755 },
        { name: "lib/libx.so", data: Buffer.from("lib") },
      ]),
      dir,
      "unit",
    );
    assert.equal(readFileSync(join(dir, "bin", "tool"), "utf-8"), "one");
    writeFileSync(join(dir, "stale"), "stale");
    extractUnitArchive(
      buildZip([{ name: "bin/tool", data: Buffer.from("two"), mode: 0o100755 }]),
      dir,
      "unit",
    );
    assert.equal(readFileSync(join(dir, "bin", "tool"), "utf-8"), "two");
    assert.equal(existsSync(join(dir, "stale")), false, "idempotent clean re-stage");
    rmSync(dir, { recursive: true, force: true });
  });

  test("inventory counts files and uncompressed bytes", () => {
    const zip = buildZip([
      { name: "a", data: Buffer.from("1234") },
      { name: "d/b", data: Buffer.from("123456") },
    ]);
    assert.deepEqual(inventoryArchive(zip, "unit"), { fileCount: 2, totalBytes: 10 });
  });
});

describe("manifest-verified directory units", () => {
  function sourceFixture() {
    const dir = tmp("dirunit");
    mkdirSync(join(dir, "src", "bin"), { recursive: true });
    writeFileSync(join(dir, "src", "unit-helper"), "exe");
    writeFileSync(join(dir, "src", "bin", "libx.so"), "lib");
    return dir;
  }
  const files = { "unit-helper": "exe", "bin/libx.so": "lib" };

  test("copies the tree and verifies every file", () => {
    const dir = sourceFixture();
    const mf = join(dir, "manifest.json");
    writeFileSync(mf, JSON.stringify(manifestFor(files)));
    const inv = copyDirectoryUnit(join(dir, "src"), join(dir, "out"), mf, "unit");
    assert.equal(inv.fileCount, 2);
    assert.equal(readFileSync(join(dir, "out", "bin", "libx.so"), "utf-8"), "lib");
    rmSync(dir, { recursive: true, force: true });
  });

  test("mismatch, extra and missing files are errors naming them", () => {
    const dir = sourceFixture();
    const mf = join(dir, "manifest.json");
    writeFileSync(mf, JSON.stringify(manifestFor(files)));
    writeFileSync(join(dir, "src", "extra"), "extra");
    assert.throws(
      () => copyDirectoryUnit(join(dir, "src"), join(dir, "out"), mf, "unit"),
      /extra files: extra/,
    );
    rmSync(join(dir, "src", "extra"));
    writeFileSync(join(dir, "src", "unit-helper"), "changed");
    assert.throws(
      () => copyDirectoryUnit(join(dir, "src"), join(dir, "out"), mf, "unit"),
      /hash mismatch for: unit-helper/,
    );
    writeFileSync(mf, JSON.stringify({ ...manifestFor(files), missing: sha(Buffer.from("x")) }));
    assert.throws(
      () => verifyDirectoryManifest(join(dir, "src"), mf, "unit"),
      /missing files: missing/,
    );
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("loader rewriting", () => {
  test("Mach-O and ELF detection is by magic bytes", () => {
    assert.equal(isMachOBytes(Buffer.from([0xfe, 0xed, 0xfa, 0xcf])), true);
    assert.equal(isMachOBytes(Buffer.from([0xca, 0xfe, 0xba, 0xbe])), true);
    assert.equal(isElfBytes(Buffer.from([0x7f, 0x45, 0x4c, 0x46])), true);
    assert.equal(isElfBytes(Buffer.from("MZ90")), false);
    assert.equal(isMachOBytes(Buffer.from("libx.so")), false);
  });

  test("computed rpaths: flat and nested trees", () => {
    assert.deepEqual(computeLoaderRpaths("", [""], "@loader_path"), ["@loader_path"]);
    assert.deepEqual(computeLoaderRpaths("", ["", "lib"], "@loader_path"), [
      "@loader_path",
      "@loader_path/lib",
    ]);
    assert.deepEqual(computeLoaderRpaths("bin", ["bin", "lib"], "$ORIGIN"), [
      "$ORIGIN",
      "$ORIGIN/../lib",
    ]);
    assert.deepEqual(computeLoaderRpaths("bin/deep", ["lib"], "@loader_path"), [
      "@loader_path/../../lib",
    ]);
  });

  test("Windows needs no rewriting", () => {
    if (platform() !== "win32") return;
    const dir = tmp("winrewrite");
    writeFileSync(join(dir, "tool.exe"), "MZ");
    writeFileSync(join(dir, "lib.dll"), "MZ");
    assert.deepEqual(rewriteUnitLoaders(dir), { kind: "none", rewritten: 0 });
    rmSync(dir, { recursive: true, force: true });
  });

  test("Linux patchelf rewrites a real ELF tree", () => {
    if (platform() !== "linux") return;
    const dir = tmp("elfrewrite");
    mkdirSync(join(dir, "bin"), { recursive: true });
    mkdirSync(join(dir, "lib"), { recursive: true });
    cpSync("/bin/true", join(dir, "bin", "tool"));
    cpSync("/bin/true", join(dir, "lib", "libx.so"));
    const out = rewriteUnitLoaders(dir);
    assert.equal(out.kind, "elf");
    assert.equal(out.rewritten, 2);
    assert.equal(execFileSync("patchelf", ["--print-rpath", join(dir, "bin", "tool")], { encoding: "utf-8" }).trim(), "$ORIGIN:$ORIGIN/../lib");
    assert.equal(execFileSync("patchelf", ["--print-rpath", join(dir, "lib", "libx.so")], { encoding: "utf-8" }).trim(), "$ORIGIN/../bin:$ORIGIN");
    rmSync(dir, { recursive: true, force: true });
  });

  test("macOS install_name_tool rewrites a real Mach-O tree", () => {
    if (platform() !== "darwin") return;
    const dir = tmp("machrewrite");
    mkdirSync(join(dir, "bin"), { recursive: true });
    mkdirSync(join(dir, "lib"), { recursive: true });
    cpSync("/bin/echo", join(dir, "bin", "tool"));
    cpSync("/bin/echo", join(dir, "lib", "libx.dylib"));
    const out = rewriteUnitLoaders(dir);
    assert.equal(out.kind, "mach-o");
    assert.equal(out.rewritten, 2);
    const rpaths = execFileSync("otool", ["-l", join(dir, "bin", "tool")], { encoding: "utf-8" });
    assert.ok(rpaths.includes("@loader_path/../lib"), rpaths);
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("unit manifest entries", () => {
  test("binaries.json uses shape unit, root payload and the executable path", () => {
    const dir = tmp("unitmanifest");
    const out = writeBinariesManifest(
      dir,
      [{ name: "llama", shape: "unit", executable: "bin/llama-server" }],
      false,
    );
    const doc = JSON.parse(readFileSync(out, "utf-8"));
    assert.deepEqual(doc.binaries.llama, {
      shape: "unit",
      root: "payload",
      path: "llama/bin/llama-server",
    });
    rmSync(dir, { recursive: true, force: true });
  });
});


describe("unit staging placement", () => {
  test("build and dev stage the same tree layout and manifest entry", async () => {
    const dir = tmp("placement");
    mkdirSync(join(dir, "src-tauri"), { recursive: true });
    mkdirSync(join(dir, "vendor", "u", "bin"), { recursive: true });
    writeFileSync(join(dir, "vendor", "u", "tool"), "exe");
    writeFileSync(join(dir, "vendor", "u", "bin", "libx.so"), "lib");
    writeFileSync(
      join(dir, "vendor", "manifest.json"),
      JSON.stringify({ tool: sha(Buffer.from("exe")), "bin/libx.so": sha(Buffer.from("lib")) }),
    );
    const triple = process.platform === "win32" ? "x86_64-pc-windows-msvc"
      : process.platform === "darwin" ? "aarch64-apple-darwin"
      : "x86_64-unknown-linux-gnu";
    const binaries = {
      u: {
        license: "MIT",
        targets: { [triple]: { path: "vendor/u", manifest: "vendor/manifest.json", executable: "tool" } },
      },
    };
    const first = await stageBuildBinaries(dir, join(dir, "src-tauri"), binaries, triple, join(dir, ".cache"));
    assert.deepEqual(first, [{ name: "u", shape: "unit", executable: "tool" }]);
    assert.equal(readFileSync(join(dir, "src-tauri", "resources", "binaries", "u", "bin", "libx.so"), "utf-8"), "lib");
    writeFileSync(join(dir, "src-tauri", "resources", "binaries", "u", "stale"), "stale");
    await stageBuildBinaries(dir, join(dir, "src-tauri"), binaries, triple, join(dir, ".cache"));
    assert.equal(existsSync(join(dir, "src-tauri", "resources", "binaries", "u", "stale")), false);
    const dev = await prepareDevBinaries(dir, binaries);
    assert.equal(readFileSync(join(dev, "u", "bin", "libx.so"), "utf-8"), "lib");
    const manifest = JSON.parse(readFileSync(join(dev, "binaries.json"), "utf-8"));
    assert.deepEqual(manifest.binaries.u, { shape: "unit", root: "payload", path: "u/tool" });
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("doctor unit reporting", () => {
  test("reports file count and size for a manifest-verified unit", () => {
    const dir = tmp("doctor");
    mkdirSync(join(dir, "vendor", "u", "bin"), { recursive: true });
    writeFileSync(join(dir, "vendor", "u", "tool"), "1234");
    writeFileSync(join(dir, "vendor", "u", "bin", "libx.so"), "123456");
    const files = { tool: sha(Buffer.from("1234")), "bin/libx.so": sha(Buffer.from("123456")) };
    writeFileSync(join(dir, "vendor", "manifest.json"), JSON.stringify(files));
    const triple = process.platform === "win32" ? "x86_64-pc-windows-msvc"
      : process.platform === "darwin" ? "aarch64-apple-darwin"
      : "x86_64-unknown-linux-gnu";
    const checks = [];
    binaryChecks(dir, {
      binaries: {
        u: {
          license: "MIT",
          targets: { [triple]: { path: "vendor/u", manifest: "vendor/manifest.json", executable: "tool" } },
        },
      },
    }, (name, ok, detail) => checks.push({ name, ok, detail }));
    assert.equal(checks.length, 1);
    assert.equal(checks[0].name, "binary unit u");
    assert.equal(checks[0].ok, true);
    assert.match(checks[0].detail, /2 files, 0\.0 MiB/);
    assert.match(checks[0].detail, /directory manifest verified/);
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("staged binary signing helpers", () => {
  test("windows signCommand uses %1 or appends the quoted file", async () => {
    const { windowsSignCommandLine } = await import("../dist/build/sign.js");
    assert.equal(windowsSignCommandLine("signer /x %1 /y", "C:\a b\tool.dll"), 'signer /x "C:\a b\tool.dll" /y');
    assert.equal(windowsSignCommandLine("signer", "tool.exe"), 'signer "tool.exe"');
  });
});
