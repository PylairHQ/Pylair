import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { deflateRawSync } from "node:zlib";
import { createServer } from "node:http";
import { get as httpGet } from "node:http";
import { create as createTar } from "tar";

import { loadConfig } from "../dist/config.js";
import {
  extractMember,
  extractZipMember,
  installedBinaryName,
  memberPathRejection,
  mergeExternalBin,
  prepareDevBinaries,
  resolveBinaryBytes,
  stageBuildBinaries,
  stageBinaryNotices,
  writeBinariesManifest,
} from "../dist/build.js";
import { isGplFamily } from "../dist/build/notices.js";

const sha = (buf) => createHash("sha256").update(buf).digest("hex");
const tmp = (tag) => mkdtempSync(join(tmpdir(), `pylair-bintest-${tag}-`));

/** Minimal zip writer (stored or deflated entries) for fixtures - the
 * reader under test handles exactly these two methods. */
function buildZip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const { name, data, method = 0, symlink = false } of entries) {
    const nameBuf = Buffer.from(name, "utf-8");
    const content = method === 8 ? deflateRawSync(data) : data;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0, 6);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(0, 14); // crc (not checked by the reader)
    local.writeUInt32LE(content.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    local.writeUInt16LE(0, 28);
    locals.push(local, nameBuf, content);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(content.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBuf.length, 28);
    central.writeUInt32LE(symlink ? ((0o120777 << 16) >>> 0) : 0, 38);
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

async function buildTarGz(dir, files) {
  for (const [name, content] of Object.entries(files)) {
    mkdirSync(join(dir, name, ".."), { recursive: true });
    writeFileSync(join(dir, name), content);
  }
  const out = join(dir, "archive.tar.gz");
  await createTar({ file: out, cwd: dir, gzip: true }, Object.keys(files));
  return readFileSync(out);
}

/** A loopback download server with hit counting; the get-bridge injects
 * node:http's get so the real download stack (redirects, resume, timeout)
 * runs against the real server - never the network. */
async function serve(routes) {
  const hits = {};
  const server = createServer((req, res) => {
    hits[req.url] = (hits[req.url] ?? 0) + 1;
    const route = routes[req.url];
    if (!route) {
      res.writeHead(404);
      res.end();
      return;
    }
    if (route.redirect) {
      res.writeHead(302, { location: route.redirect });
      res.end();
      return;
    }
    res.writeHead(200, { "content-length": route.body.length });
    res.end(route.body);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  return { hits, url: (p) => `http://127.0.0.1:${port}${p}`, close: () => server.close() };
}

function pyproject(dir, binariesTable) {
  writeFileSync(
    join(dir, "pyproject.toml"),
    `[project]\nname="app"\n[tool.pylair]\napp-module="backend.app"\n${binariesTable}\n`,
  );
}

const TRIPLE = process.platform === "win32" ? "x86_64-pc-windows-msvc"
  : process.platform === "darwin" ? "aarch64-apple-darwin"
  : "x86_64-unknown-linux-gnu";

describe("binaries config validation", () => {
  const parse = (table) => {
    const dir = tmp("cfg");
    pyproject(dir, table);
    try {
      return loadConfig(dir).binaries;
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  };

  test("parses a complete entry", () => {
    const bins = parse(`
[tool.pylair.binaries.ffmpeg]
license = "LGPL-2.1-or-later"
license_file = "licenses/ffmpeg.txt"
version = "7.1"
[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
url = "https://example.com/ffmpeg.zip"
sha256 = "${"a".repeat(64)}"
member = "bin/ffmpeg.exe"
[tool.pylair.binaries.ffmpeg.x86_64-unknown-linux-gnu]
path = "vendor/ffmpeg-linux"
sha256 = "${"b".repeat(64)}"
`);
    const f = bins.ffmpeg;
    assert.equal(f.license, "LGPL-2.1-or-later");
    assert.equal(f.licenseFile, "licenses/ffmpeg.txt");
    assert.equal(f.version, "7.1");
    assert.equal(f.targets["x86_64-pc-windows-msvc"].member, "bin/ffmpeg.exe");
    assert.equal(f.targets["x86_64-unknown-linux-gnu"].path, "vendor/ffmpeg-linux");
  });

  test("no binaries table means an empty map", () => {
    assert.deepEqual(parse(""), {});
  });

  test("missing license is a hard error", () => {
    assert.throws(
      () => parse(`
[tool.pylair.binaries.ffmpeg]
[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
path = "x"
sha256 = "${"a".repeat(64)}"
`),
      /license is required/,
    );
  });

  test("unknown key in the binary table", () => {
    assert.throws(
      () => parse(`
[tool.pylair.binaries.ffmpeg]
license = "MIT"
homepage = "https://x"
`),
      /unknown key "homepage"/,
    );
  });

  test("unknown triple names the known set", () => {
    assert.throws(
      () => parse(`
[tool.pylair.binaries.ffmpeg]
license = "MIT"
[tool.pylair.binaries.ffmpeg.riscv64-unknown-linux-gnu]
path = "x"
sha256 = "${"a".repeat(64)}"
`),
      /unknown target triple "riscv64-unknown-linux-gnu"/,
    );
  });

  test("missing sha256", () => {
    assert.throws(
      () => parse(`
[tool.pylair.binaries.ffmpeg]
license = "MIT"
[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
path = "x"
`),
      /sha256 is required/,
    );
  });

  test("non-hex sha256", () => {
    assert.throws(
      () => parse(`
[tool.pylair.binaries.ffmpeg]
license = "MIT"
[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
path = "x"
sha256 = "nothex"
`),
      /64 lowercase hex/,
    );
  });

  test("both path and url rejected; neither rejected", () => {
    for (const [body, re] of [
      ['path = "x"\nurl = "https://y"\nsha256 = "' + "a".repeat(64) + '"', /both path and url/],
      ['sha256 = "' + "a".repeat(64) + '"', /neither path nor url/],
    ]) {
      assert.throws(
        () => parse(`
[tool.pylair.binaries.ffmpeg]
license = "MIT"
[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
${body}
`),
        re,
      );
    }
  });

  test("the executable key fails as a directory unit naming the entry", () => {
    assert.throws(
      () => parse(`
[tool.pylair.binaries.llama-server]
license = "MIT"
[tool.pylair.binaries.llama-server.x86_64-pc-windows-msvc]
url = "https://x/llama.zip"
sha256 = "${"a".repeat(64)}"
executable = "llama-server.exe"
`),
      /directory units land in a later release.*binaries\.llama-server/s,
    );
  });
});

describe("member extraction", () => {
  test("zip stored member", async () => {
    const zip = buildZip([{ name: "bin/tool.exe", data: Buffer.from("MZ-fake") }]);
    const out = await extractMember(zip, "bin/tool.exe");
    assert.equal(out.toString(), "MZ-fake");
  });

  test("zip deflated member", async () => {
    const zip = buildZip([{ name: "tool", data: Buffer.from("x".repeat(5000)), method: 8 }]);
    const out = await extractMember(zip, "tool");
    assert.equal(out.length, 5000);
  });

  test("tar.gz member", async () => {
    const dir = tmp("tar");
    const tgz = await buildTarGz(dir, { "bin/ffmpeg": "elf-bytes" });
    const out = await extractMember(tgz, "bin/ffmpeg");
    assert.equal(out.toString(), "elf-bytes");
    rmSync(dir, { recursive: true, force: true });
  });

  test("missing member names it", async () => {
    const zip = buildZip([{ name: "a", data: Buffer.from("x") }]);
    await assert.rejects(extractMember(zip, "nope"), /does not contain member "nope"/);
  });

  test("each member-path safety rejection", async () => {
    for (const bad of ["/abs/x", "C:/x", "../escape", "a/../../b", "back\\slash"]) {
      assert.ok(memberPathRejection(bad), `${bad} rejected`);
      await assert.rejects(extractMember(buildZip([]), bad));
    }
    assert.equal(memberPathRejection("bin/ok.exe"), null);
  });

  test("zip symlink member rejected", async () => {
    const zip = buildZip([{ name: "link", data: Buffer.from("target"), symlink: true }]);
    await assert.rejects(extractMember(zip, "link"), /symlink/);
  });

  test("non-archive bytes rejected", async () => {
    await assert.rejects(extractMember(Buffer.from("plain exe bytes"), "x"), /neither zip nor tar\.gz/);
  });
});

describe("staging", () => {
  function appFixture(targetBody) {
    const dir = tmp("stage");
    mkdirSync(join(dir, "src-tauri"), { recursive: true });
    writeFileSync(
      join(dir, "src-tauri", "tauri.conf.json"),
      JSON.stringify({ bundle: { resources: {} } }, null, 2),
    );
    const exe = Buffer.from("fake-executable-bytes");
    const hex = sha(exe);
    return { dir, exe, hex };
  }

  test("path source stages with hash verification and the triple name", async () => {
    const { dir, exe, hex } = appFixture();
    mkdirSync(join(dir, "vendor"), { recursive: true });
    writeFileSync(join(dir, "vendor", "tool"), exe);
    const binaries = {
      tool: {
        license: "MIT",
        licenseFile: "vendor/LICENSE",
        targets: { [TRIPLE]: { path: "vendor/tool", sha256: hex } },
      },
    };
    writeFileSync(join(dir, "vendor", "LICENSE"), "mit text");
    const staged = await stageBuildBinaries(dir, join(dir, "src-tauri"), binaries, TRIPLE, join(dir, ".cache"));
    assert.deepEqual(staged, ["tool"]);
    const suffix = TRIPLE.includes("windows") ? ".exe" : "";
    const out = join(dir, "src-tauri", "binaries", `tool-${TRIPLE}${suffix}`);
    assert.equal(readFileSync(out).toString(), exe.toString());
    if (process.platform !== "win32") {
      const mode = (await import("node:fs")).statSync(out).mode;
      assert.ok(mode & 0o111, "executable bit set on POSIX");
    }
    rmSync(dir, { recursive: true, force: true });
  });

  test("hash mismatch fails before staging anything", async () => {
    const { dir, exe } = appFixture();
    mkdirSync(join(dir, "vendor"), { recursive: true });
    writeFileSync(join(dir, "vendor", "tool"), exe);
    const binaries = {
      tool: {
        license: "MIT",
        targets: { [TRIPLE]: { path: "vendor/tool", sha256: "0".repeat(64) } },
      },
    };
    await assert.rejects(
      stageBuildBinaries(dir, join(dir, "src-tauri"), binaries, TRIPLE, join(dir, ".cache")),
      /hash mismatch for binaries\.tool/,
    );
    assert.ok(!existsSync(join(dir, "src-tauri", "binaries", `tool-${TRIPLE}`)), "no binary file staged");
    rmSync(dir, { recursive: true, force: true });
  });

  test("missing triple error names the exact pyproject key", async () => {
    const { dir } = appFixture();
    const binaries = { tool: { license: "MIT", targets: {} } };
    await assert.rejects(
      stageBuildBinaries(dir, join(dir, "src-tauri"), binaries, TRIPLE, join(dir, ".cache")),
      new RegExp(
        `binaries\\.tool has no source for ${TRIPLE.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*\\n\\s*add \\[tool\\.pylair\\.binaries\\.tool\\.${TRIPLE.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\] with path or url \\+ sha256`,
      ),
    );
    rmSync(dir, { recursive: true, force: true });
  });

  test("url source downloads once into the sha256 cache", async () => {
    const { dir, exe, hex } = appFixture();
    const srv = await serve({ "/tool": { body: exe } });
    try {
      const binaries = {
        tool: { license: "MIT", targets: { [TRIPLE]: { url: srv.url("/tool"), sha256: hex } } },
      };
      const cacheDir = join(dir, ".cache");
      const first = await resolveBinaryBytes("tool", binaries.tool.targets[TRIPLE], {
        cacheDir,
        appDir: dir,
        get: httpGet,
      });
      assert.equal(first.toString(), exe.toString());
      assert.equal(srv.hits["/tool"], 1);
      // Second resolution: the cache hit serves it, no refetch.
      const second = await resolveBinaryBytes("tool", binaries.tool.targets[TRIPLE], {
        cacheDir,
        appDir: dir,
        get: httpGet,
      });
      assert.equal(second.toString(), exe.toString());
      assert.equal(srv.hits["/tool"], 1, "cache hit means no second fetch");
    } finally {
      srv.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("externalBin merge", () => {
  function conf(dir, initial) {
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "tauri.conf.json"), JSON.stringify(initial, null, 2));
  }

  test("adds declared names, never duplicates, preserves user entries", () => {
    const dir = tmp("merge");
    conf(dir, { bundle: { externalBin: ["binaries/user-tool"], resources: {} } });
    const first = mergeExternalBin(dir, ["ffmpeg", "llama"]);
    const conf1 = JSON.parse(readFileSync(join(dir, "tauri.conf.json"), "utf-8"));
    assert.deepEqual(conf1.bundle.externalBin, ["binaries/user-tool", "binaries/ffmpeg", "binaries/llama"]);
    assert.ok(first.some((c) => c.includes("added externalBin entry binaries/ffmpeg")));
    // Resources mapping carries the manifest into the payload.
    assert.equal(conf1.bundle.resources["resources/binaries"], "binaries/");

    // Second run: no duplicates, no new diff.
    mergeExternalBin(dir, ["ffmpeg", "llama"]);
    const conf2 = JSON.parse(readFileSync(join(dir, "tauri.conf.json"), "utf-8"));
    assert.deepEqual(conf2.bundle.externalBin, conf1.bundle.externalBin);
    rmSync(dir, { recursive: true, force: true });
  });

  test("duplicate pre-existing entries collapse", () => {
    const dir = tmp("merge");
    conf(dir, { bundle: { externalBin: ["binaries/ffmpeg", "binaries/ffmpeg"], resources: {} } });
    mergeExternalBin(dir, ["ffmpeg"]);
    const conf1 = JSON.parse(readFileSync(join(dir, "tauri.conf.json"), "utf-8"));
    assert.deepEqual(conf1.bundle.externalBin, ["binaries/ffmpeg"]);
    rmSync(dir, { recursive: true, force: true });
  });

  test("array-form bundle.resources is a build error naming the fix", () => {
    const dir = tmp("merge");
    conf(dir, { bundle: { resources: ["assets/logo.png", "binaries/"] } });
    assert.throws(
      () => mergeExternalBin(dir, ["ffmpeg"]),
      /bundle\.resources in src-tauri\/tauri\.conf\.json is an array; convert it to the object form \{ "<src>": "<dest>" \} so pylair can map binaries\/binaries\.json, or remove the binaries table/,
    );
    // The config file is untouched - a failed build never half-edits it.
    assert.deepEqual(JSON.parse(readFileSync(join(dir, "tauri.conf.json"), "utf-8")), {
      bundle: { resources: ["assets/logo.png", "binaries/"] },
    });
    rmSync(dir, { recursive: true, force: true });
  });

  test("object form with the mapping already present is left alone", () => {
    const dir = tmp("merge");
    conf(dir, {
      bundle: {
        externalBin: ["binaries/ffmpeg"],
        resources: { "assets/": "assets/", "resources/binaries": "binaries/" },
      },
    });
    const before = readFileSync(join(dir, "tauri.conf.json"), "utf-8");
    const changes = mergeExternalBin(dir, ["ffmpeg"]);
    const confAfter = JSON.parse(readFileSync(join(dir, "tauri.conf.json"), "utf-8"));
    assert.deepEqual(confAfter.bundle.resources, {
      "assets/": "assets/",
      "resources/binaries": "binaries/",
    });
    assert.deepEqual(confAfter.bundle.externalBin, ["binaries/ffmpeg"]);
    assert.ok(!changes.some((c) => c.includes("resources")), "no resources churn");
    assert.equal(readFileSync(join(dir, "tauri.conf.json"), "utf-8"), before, "byte-identical");
    rmSync(dir, { recursive: true, force: true });
  });

  test("no names means no edit", () => {
    const dir = tmp("merge");
    conf(dir, { bundle: {} });
    mergeExternalBin(dir, []);
    assert.deepEqual(JSON.parse(readFileSync(join(dir, "tauri.conf.json"), "utf-8")), { bundle: {} });
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("binaries.json manifest", () => {
  test("schema exactness, parsed back with the crate's rules", () => {
    const dir = tmp("manifest");
    const out = writeBinariesManifest(dir, ["ffmpeg", "llama-server"], process.platform === "win32");
    const doc = JSON.parse(readFileSync(out, "utf-8"));

    // The crate's validation rules, mirrored exactly (src/binaries.rs).
    assert.equal(doc.version, 1);
    for (const [name, entry] of Object.entries(doc.binaries)) {
      assert.ok(["file", "unit"].includes(entry.shape), `${name} shape`);
      assert.ok(["exe_dir", "payload"].includes(entry.root), `${name} root`);
      assert.equal(typeof entry.path, "string");
      assert.ok(!entry.path.startsWith("/"), "relative");
      assert.ok(!/^[A-Za-z]:/.test(entry.path), "no drive letter");
      assert.ok(!entry.path.includes("\\"), "forward slashes");
      assert.ok(!entry.path.split("/").includes(".."), "no dotdot");
    }
    // File shape, exe_dir root, and the bundler-stripped install name.
    const suffix = process.platform === "win32" ? ".exe" : "";
    assert.deepEqual(doc.binaries.ffmpeg, { shape: "file", root: "exe_dir", path: `ffmpeg${suffix}` });
    assert.deepEqual(doc.binaries["llama-server"], {
      shape: "file",
      root: "exe_dir",
      path: `llama-server${suffix}`,
    });
    // Keys are sorted for deterministic output.
    assert.deepEqual(Object.keys(doc.binaries), [...Object.keys(doc.binaries)].sort());
    rmSync(dir, { recursive: true, force: true });
  });

  test("installed name states the bundler rule", () => {
    // crates/tauri-bundler copies sidecars with "-<target-triple>"
    // string-replaced out of the filename, so the .exe suffix survives on
    // Windows and nothing is suffixed elsewhere.
    assert.equal(installedBinaryName("ffmpeg", true), "ffmpeg.exe");
    assert.equal(installedBinaryName("ffmpeg", false), "ffmpeg");
  });
});

describe("dev binaries", () => {
  test("cache reused on a second run without a refetch", async () => {
    const dir = tmp("devbin");
    const exe = Buffer.from("dev-tool-bytes");
    const hex = sha(exe);
    const srv = await serve({ "/tool": { body: exe } });
    try {
      const binaries = {
        tool: { license: "MIT", targets: { [TRIPLE]: { url: srv.url("/tool"), sha256: hex } } },
      };
      const first = await prepareDevBinaries(dir, binaries, { get: httpGet });
      assert.ok(first.endsWith(join(".pylair", "binaries")));
      const hitsAfterFirst = srv.hits["/tool"];
      assert.ok(hitsAfterFirst >= 1);

      const second = await prepareDevBinaries(dir, binaries, { get: httpGet });
      assert.equal(second, first);
      assert.equal(srv.hits["/tool"], hitsAfterFirst, "second run never refetches");

      // The manifest sits in the dir and names the staged file.
      const doc = JSON.parse(readFileSync(join(first, "binaries.json"), "utf-8"));
      const suffix = process.platform === "win32" ? ".exe" : "";
      assert.equal(doc.binaries.tool.path, `tool${suffix}`);
      assert.equal(readFileSync(join(first, `tool${suffix}`)).toString(), exe.toString());
    } finally {
      srv.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("path source wins over any download", async () => {
    const dir = tmp("devpath");
    mkdirSync(join(dir, "vendor"), { recursive: true });
    const exe = Buffer.from("vendored");
    writeFileSync(join(dir, "vendor", "tool"), exe);
    const binaries = {
      tool: { license: "MIT", targets: { [TRIPLE]: { path: "vendor/tool", sha256: sha(exe) } } },
    };
    const out = await prepareDevBinaries(dir, binaries);
    const suffix = process.platform === "win32" ? ".exe" : "";
    assert.equal(readFileSync(join(out, `tool${suffix}`)).toString(), "vendored");
    rmSync(dir, { recursive: true, force: true });
  });

  test("no binaries declared means no dir and undefined", async () => {
    const dir = tmp("devnone");
    assert.equal(await prepareDevBinaries(dir, {}), undefined);
    assert.ok(!existsSync(join(dir, ".pylair", "binaries")));
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("binary notices", () => {
  test("license_file carried, index and index.json written", async () => {
    const dir = tmp("notices");
    const res = join(dir, "resources");
    mkdirSync(join(res, "THIRD-PARTY-NOTICES"), { recursive: true });
    writeFileSync(join(res, "THIRD-PARTY-NOTICES", "INDEX.txt"), "pylair/ - pylair itself\n");
    mkdirSync(join(dir, "licenses"), { recursive: true });
    writeFileSync(join(dir, "licenses", "ffmpeg.txt"), "LGPL license text");
    const exe = Buffer.from("x");
    const binaries = {
      ffmpeg: {
        license: "LGPL-2.1-or-later",
        licenseFile: "licenses/ffmpeg.txt",
        version: "7.1",
        targets: { [TRIPLE]: { url: "https://example.com/ffmpeg.zip", sha256: sha(exe) } },
      },
    };
    await stageBinaryNotices(res, dir, binaries, TRIPLE, join(dir, ".cache"));
    assert.equal(
      readFileSync(join(res, "THIRD-PARTY-NOTICES", "binaries", "ffmpeg", "ffmpeg.txt"), "utf-8"),
      "LGPL license text",
    );
    const index = readFileSync(join(res, "THIRD-PARTY-NOTICES", "INDEX.txt"), "utf-8");
    assert.ok(index.includes("pylair/"), "prior index preserved");
    assert.ok(index.includes("binaries/ffmpeg/ - ffmpeg 7.1 (LGPL-2.1-or-later), example.com"), index);
    const sbom = JSON.parse(readFileSync(join(res, "THIRD-PARTY-NOTICES", "index.json"), "utf-8"));
    assert.equal(sbom.binaries.length, 1);
    assert.equal(sbom.binaries[0].name, "ffmpeg");
    assert.equal(sbom.binaries[0].version, "7.1");
    assert.equal(sbom.binaries[0].spdx, "LGPL-2.1-or-later");
    assert.equal(sbom.binaries[0].source_url, "https://example.com/ffmpeg.zip");
    assert.equal(sbom.binaries[0].sha256, sha(exe));
    rmSync(dir, { recursive: true, force: true });
  });

  test("missing license file is a build error", async () => {
    const dir = tmp("notices");
    const res = join(dir, "resources");
    mkdirSync(join(res, "THIRD-PARTY-NOTICES"), { recursive: true });
    const binaries = {
      ffmpeg: {
        license: "MIT",
        licenseFile: "licenses/absent.txt",
        targets: {},
      },
    };
    await assert.rejects(
      stageBinaryNotices(res, dir, binaries, TRIPLE, join(dir, ".cache")),
      /license_file licenses\/absent\.txt does not exist/,
    );
    rmSync(dir, { recursive: true, force: true });
  });

  test("neither license_file nor license_url is a hard error", async () => {
    const dir = tmp("notices");
    const res = join(dir, "resources");
    mkdirSync(join(res, "THIRD-PARTY-NOTICES"), { recursive: true });
    await assert.rejects(
      stageBinaryNotices(res, dir, { t: { license: "MIT", targets: {} } }, TRIPLE, join(dir, ".cache")),
      /license_file or license_url is required/,
    );
    rmSync(dir, { recursive: true, force: true });
  });

  test("GPL family detection", () => {
    assert.ok(isGplFamily("GPL-2.0-only"));
    assert.ok(isGplFamily("GPL-3.0-or-later"));
    assert.ok(isGplFamily("AGPL-3.0-only"));
    assert.ok(!isGplFamily("LGPL-2.1-or-later"));
    assert.ok(!isGplFamily("MIT"));
  });
});
