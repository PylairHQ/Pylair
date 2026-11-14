import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync, readdirSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import { Readable } from "node:stream";
import { createHash } from "node:crypto";

import { matchGlob } from "../dist/glob.js";
import { stageNotices, download, ensureTarball, uvExportArgs, uvPipInstallArgs, PYC_INVALIDATION_MODE } from "../dist/build.js";
import { DEFAULT_PRUNE, prune } from "../dist/prune.js";
import { loadConfig } from "../dist/config.js";

describe("matchGlob", () => {
  test("literal and single-star", () => {
    assert.ok(matchGlob("python/include", "python/include"));
    assert.ok(matchGlob("python/include", "python/include/pyconfig.h")); // dir prefix
    assert.ok(matchGlob("*.a", "libfoo.a"));
    assert.ok(!matchGlob("*.a", "x/libfoo.a"));
    assert.ok(matchGlob("python/lib/*.a", "python/lib/libssl.a"));
    assert.ok(!matchGlob("python/lib/*.a", "python/lib/x/libssl.a"));
  });

  test("double-star crosses dirs", () => {
    assert.ok(matchGlob("python/lib/**/*.a", "python/lib/x/y/z.a"));
    assert.ok(matchGlob("site-packages/**/__pycache__", "site-packages/a/b/__pycache__"));
    assert.ok(matchGlob("python/lib/python*/test", "python/lib/python3.12/test"));
    assert.ok(matchGlob("python/lib/python*/test", "python/lib/python3.12/test/x.py"));
    assert.ok(!matchGlob("python/lib/python*/test", "python/lib/python3.12/testing"));
  });

  test("question mark is single char", () => {
    assert.ok(matchGlob("a?c", "abc"));
    assert.ok(!matchGlob("a?c", "a/c"));
  });
});

describe("prune", () => {
  function tree(files) {
    const dir = mkdtempSync(join(tmpdir(), "pylair-prune-"));
    for (const f of files) {
      const p = join(dir, f);
      mkdirSync(join(p, ".."), { recursive: true });
      writeFileSync(p, "x".repeat(16));
    }
    return dir;
  }

  test("removes denied dirs, keeps the rest", () => {
    const dir = tree([
      "python/lib/python3.12/test/x.py",
      "python/lib/python3.12/os.py",
      "python/include/Python.h",
      "site-packages/pip/__init__.py",
      "site-packages/numpy/__init__.py",
      "site-packages/numpy/tests/t.py",
      "backend/app.py",
      "backend/__pycache__/app.pyc",
    ]);
    const r = prune(dir, DEFAULT_PRUNE);
    assert.ok(r.removed >= 5);
    assert.ok(!existsSync(join(dir, "python/include")));
    assert.ok(!existsSync(join(dir, "python/lib/python3.12/test")));
    assert.ok(!existsSync(join(dir, "site-packages/pip")));
    assert.ok(!existsSync(join(dir, "site-packages/numpy/tests")));
    assert.ok(existsSync(join(dir, "python/lib/python3.12/os.py")));
    assert.ok(existsSync(join(dir, "site-packages/numpy/__init__.py")));
    assert.ok(existsSync(join(dir, "backend/app.py")));
    rmSync(dir, { recursive: true });
  });

  test("topmost dir wins; nested hits counted once", () => {
    const dir = tree(["site-packages/pip/_internal/x.py", "site-packages/pip/y.py"]);
    const r = prune(dir, ["site-packages/pip"]);
    assert.equal(r.removed, 1);
    rmSync(dir, { recursive: true });
  });
});

describe("compileBytecode", () => {
  const py = (() => {
    for (const c of ["python3", "python"]) {
      const r = spawnSync(c, ["-c", "import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)"]);
      if (r.status === 0) return c;
    }
    return null;
  })();
  const PYC_FLAGS = { timestamp: 0, "unchecked-hash": 1, "checked-hash": 3 };
  const expectedFlags = () => PYC_FLAGS[PYC_INVALIDATION_MODE];

  /** Flags word = bytes 4..8 of the pyc header (u32 LE); bytes 8+ are
   *  mtime+size for timestamp pycs or the siphash for hash-based. */
  const pycFlags = (p) => readFileSync(p).readUInt32LE(4);

  function pyTree() {
    const dir = mkdtempSync(join(tmpdir(), "pylair-pyc-"));
    for (const [f, src] of [
      ["site-packages/numpy/__init__.py", "x = 1"],
      ["site-packages/numpy/tests/t.py", "y = 2"],
      ["site-packages/pip/__init__.py", "z = 3"],
      ["backend/app.py", "a = 4"],
    ]) {
      const p = join(dir, f);
      mkdirSync(join(p, ".."), { recursive: true });
      writeFileSync(p, src);
    }
    return dir;
  }

  test(
    "shipped dirs get checked-hash pycs; pruned dirs get none",
    { skip: !py },
    async () => {
      const { compileBytecode } = await import("../dist/build.js");
      const dir = pyTree();
      // The build order: prune FIRST, then compile what survives.
      prune(dir, DEFAULT_PRUNE);
      compileBytecode(dir, py, ["site-packages", "backend"]);

      // pyc naming carries the interpreter tag — find it by glob-in-dir.
      const cache = join(dir, "site-packages", "numpy", "__pycache__");
      const names = existsSync(cache) ? readdirSync(cache) : [];
      assert.ok(
        names.some((n) => n.startsWith("__init__.cpython-") && n.endsWith(".pyc")),
        `site-packages module must be precompiled, found ${names}`,
      );
      const pyc = join(cache, names[0]);
      assert.equal(
        pycFlags(pyc),
        expectedFlags(),
        `pyc flags must be ${PYC_INVALIDATION_MODE} (${expectedFlags()})`,
      );

      const backendCache = join(dir, "backend", "__pycache__");
      assert.ok(
        existsSync(backendCache) && readdirSync(backendCache).length === 1,
        "backend source must be precompiled too",
      );
      assert.equal(pycFlags(join(backendCache, readdirSync(backendCache)[0])), expectedFlags());

      // Pruned dirs must contain no pycs at all.
      for (const gone of ["site-packages/numpy/tests", "site-packages/pip"])
        assert.ok(!existsSync(join(dir, gone)), `${gone} was pruned, cannot hold pycs`);
      rmSync(dir, { recursive: true, force: true });
    },
  );

  test(
    "precompiled pycs actually load — `python -v` reports 'code object from'",
    { skip: !py },
    async () => {
      const { compileBytecode } = await import("../dist/build.js");
      const dir = mkdtempSync(join(tmpdir(), "pylair-pycload-"));
      const pkg = join(dir, "site-packages", "wibble");
      mkdirSync(pkg, { recursive: true });
      writeFileSync(join(pkg, "__init__.py"), "\n");
      writeFileSync(join(pkg, "mod.py"), "VALUE = 7\n");
      compileBytecode(dir, py, ["site-packages"]);
      // -v prints `# code object from '<pyc>'` when the importer decodes
      // a cached pyc — the proof the bundle's pycs are used, not ignored.
      const r = spawnSync(py, ["-v", "-c", "import wibble.mod"], {
        encoding: "utf-8",
        env: {
          ...process.env,
          PYTHONPATH: join(dir, "site-packages"),
          PYTHONNOUSERSITE: "1",
          PYTHONDONTWRITEBYTECODE: "1",
        },
      });
      assert.equal(r.status, 0, String(r.stderr).slice(-500));
      assert.match(
        r.stderr,
        /code object from '.*wibble.*mod\.cpython-\d+\.pyc'/,
        "importer did not decode the bundled pyc",
      );
      assert.doesNotMatch(
        r.stderr,
        /wrote bytecode '.*wibble/,
        "importer recompiled instead of using the pyc",
      );
      rmSync(dir, { recursive: true, force: true });
    },
  );

  test(
    "checked-hash revalidates: editing the source after build recompiles",
    { skip: !py || PYC_INVALIDATION_MODE !== "checked-hash" },
    async () => {
      const { compileBytecode } = await import("../dist/build.js");
      const dir = mkdtempSync(join(tmpdir(), "pylair-pycedit-"));
      const pkg = join(dir, "site-packages", "wobble");
      mkdirSync(pkg, { recursive: true });
      writeFileSync(join(pkg, "__init__.py"), "\n");
      writeFileSync(join(pkg, "mod.py"), "VALUE = 1\n");
      compileBytecode(dir, py, ["site-packages"]);
      writeFileSync(join(pkg, "mod.py"), "VALUE = 2\n");
      const r = spawnSync(
        py,
        ["-c", "import wobble.mod, sys; sys.exit(0 if wobble.mod.VALUE == 2 else 9)"],
        {
          env: {
            ...process.env,
            PYTHONPATH: join(dir, "site-packages"),
            PYTHONNOUSERSITE: "1",
            PYTHONDONTWRITEBYTECODE: "1",
          },
        },
      );
      assert.equal(r.status, 0, "edited source must win over the stale pyc");
      rmSync(dir, { recursive: true, force: true });
    },
  );

  test(
    "uncompilable files warn, do not fail; valid files still get pycs",
    { skip: !py },
    async () => {
      const { compileBytecode } = await import("../dist/build.js");
      const dir = mkdtempSync(join(tmpdir(), "pylair-pycbad-"));
      const pkg = join(dir, "site-packages", "wangle");
      mkdirSync(pkg, { recursive: true });
      writeFileSync(join(pkg, "__init__.py"), "\n");
      writeFileSync(join(pkg, "good.py"), "VALUE = 1\n");
      // A dependency may ship a file nothing imports — py2-only code, a
      // template, an invalid fixture. compileall exits 1 for it; the
      // build must warn and continue, not fail.
      const bad = join(pkg, "bad.py");
      writeFileSync(bad, "class :\n");

      let failed;
      assert.doesNotThrow(() => {
        failed = compileBytecode(dir, py, ["site-packages"]);
      }, "exit 1 from compileall must be a warning, not a build failure");

      const cache = join(pkg, "__pycache__");
      const names = existsSync(cache) ? readdirSync(cache) : [];
      assert.ok(
        names.some((n) => n.startsWith("good.cpython-")),
        "valid files still get pycs",
      );
      assert.ok(
        !names.some((n) => n.startsWith("bad.cpython-")),
        "the invalid file has no pyc",
      );
      // The warning names the failed paths — the function reports them.
      assert.ok(Array.isArray(failed) && failed.length === 1, `expected [bad.py], got ${failed}`);
      assert.ok(
        String(failed[0]).replaceAll("\\", "/").endsWith("site-packages/wangle/bad.py"),
        `failed path must name the bad file, got ${failed[0]}`,
      );
      rmSync(dir, { recursive: true, force: true });
    },
  );
});

describe("download", () => {
  // A fake ClientRequest: what the injected `get` must return.
  const fakeRequest = () => {
    const req = new EventEmitter();
    req.setTimeout = (ms, fn) => {
      req._t = setTimeout(fn, ms);
      return req;
    };
    req.destroy = (e) => {
      clearTimeout(req._t);
      req.emit("error", e ?? new Error("destroyed"));
    };
    return req;
  };
  const fakeGet = (handler) => (u, cb) => {
    const req = fakeRequest();
    queueMicrotask(() => handler(u, cb, req));
    return req;
  };
  const response = (body, { status = 200, headers = {} } = {}) => {
    const r = Readable.from(body);
    r.statusCode = status;
    r.headers = headers;
    return r;
  };

  test("a mid-body drop leaves nothing at the final path", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    const dropped = Readable.from(
      (async function* () {
        yield Buffer.from("first-half");
        throw new Error("connection reset");
      })(),
    );
    dropped.statusCode = 200;
    dropped.headers = { "content-length": "32" };
    await assert.rejects(
      download("https://x/asset", to, { get: fakeGet((u, cb) => cb(dropped)) }),
      /reset|dropped|incomplete/i,
    );
    assert.ok(!existsSync(to), "partial body must not sit at the final path");
    assert.ok(!existsSync(`${to}.part`), ".part must be cleaned up");

    // The next run succeeds — a partial must never poison the cache.
    await download(
      "https://x/asset",
      to,
      { get: fakeGet((u, cb) => cb(response([Buffer.from("full-body")]))) },
    );
    assert.equal(readFileSync(to, "utf-8"), "full-body");
    rmSync(dir, { recursive: true });
  });

  test("a relative redirect resolves against the request URL", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    const seen = [];
    const get = fakeGet((u, cb) => {
      seen.push(u);
      if (u === "https://x/a")
        return cb(response([], { status: 302, headers: { location: "/cdn/b" } }));
      return cb(response([Buffer.from("ok-body")]));
    });
    await download("https://x/a", to, { get });
    assert.deepEqual(seen, ["https://x/a", "https://x/cdn/b"]);
    assert.equal(readFileSync(to, "utf-8"), "ok-body");
    rmSync(dir, { recursive: true });
  });

  test("an http:// redirect is refused, not followed", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    // https.get throws synchronously on a non-https URL — stand in for
    // it so the pre-fix crash path is exercised, not hidden.
    const get = (u, cb) => {
      if (!u.startsWith("https://")) throw new Error("ERR_INVALID_PROTOCOL");
      const req = fakeRequest();
      queueMicrotask(() =>
        cb(response([], { status: 302, headers: { location: "http://insecure.example/x" } })),
      );
      return req;
    };
    await assert.rejects(download("https://x/a", to, { get }), /non-https|refus/i);
    assert.ok(!existsSync(to));
    assert.ok(!existsSync(`${to}.part`));
    rmSync(dir, { recursive: true });
  });

  test("a redirect loop hits the cap", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    let n = 0;
    const get = fakeGet((u, cb) =>
      cb(response([], { status: 302, headers: { location: `${u}/${++n}` } })),
    );
    await assert.rejects(download("https://x/a", to, { get }), /redirect/i);
    rmSync(dir, { recursive: true });
  });

  test("a failed download never poisons the next open file", async () => {
    // The fd closed during cleanup must never be closed twice — a stale
    // async close lands on whatever file reuses the number (fd reuse).
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    for (let i = 0; i < 25; i++) {
      const to = join(dir, `a${i}.tar.gz`);
      const dropped = Readable.from(
        (async function* () {
          yield Buffer.from("first-half");
          throw new Error("connection reset");
        })(),
      );
      dropped.statusCode = 200;
      dropped.headers = { "content-length": "32" };
      await assert.rejects(
        download("https://x/asset", to, {
          get: fakeGet((u, cb) => cb(dropped)),
        }),
      );
      const to2 = join(dir, `b${i}.tar.gz`);
      await download("https://x/asset", to2, {
        get: fakeGet((u, cb) => cb(response([Buffer.from("ok")]))),
      });
      assert.equal(readFileSync(to2, "utf-8"), "ok");
    }
    rmSync(dir, { recursive: true });
  });

  test("a stalled server hits the idle timeout", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    const stalled = new Readable({ read() {} }); // headers, then nothing
    stalled.statusCode = 200;
    stalled.headers = {};
    await assert.rejects(
      download("https://x/s", to, { get: fakeGet((u, cb) => cb(stalled)), timeoutMs: 50 }),
      /timed out|timeout/i,
    );
    assert.ok(!existsSync(to));
    assert.ok(!existsSync(`${to}.part`));
    rmSync(dir, { recursive: true });
  });
});

describe("ensureTarball", () => {
  const sha = (s) => createHash("sha256").update(s).digest("hex");

  test("a corrupt cached file is deleted and re-fetched once", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cache-"));
    const to = join(dir, "asset.tar.gz");
    writeFileSync(to, "corrupt-bytes");
    const want = sha(Buffer.from("good-bytes"));
    let calls = 0;
    const dl = async (u, dest) => {
      calls++;
      writeFileSync(dest, "good-bytes");
    };
    await ensureTarball("https://x/asset", to, want, dl);
    assert.equal(calls, 1, "re-download exactly once");
    assert.equal(readFileSync(to, "utf-8"), "good-bytes");
    rmSync(dir, { recursive: true });
  });

  test("a second mismatch refuses and names the cache file", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cache-"));
    const to = join(dir, "asset.tar.gz");
    writeFileSync(to, "corrupt-bytes");
    const want = sha(Buffer.from("good-bytes"));
    const bad = async (u, dest) => writeFileSync(dest, "also-bad");
    const err = await ensureTarball("https://x/asset", to, want, bad).then(
      () => null,
      (e) => e,
    );
    assert.ok(err, "expected a refusal");
    assert.match(err.message, /SHA256|mismatch|refusing/i);
    assert.ok(err.message.includes("asset.tar.gz"), "message must name the cache file");
    rmSync(dir, { recursive: true });
  });
});

describe("loadConfig", () => {
  const py = (() => {
    for (const c of ["python3", "python"]) {
      const r = spawnSync(c, ["-c", "import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)"]);
      if (r.status === 0) return c;
    }
    return null;
  })();

  test("parses [tool.pylair]", { skip: !py }, () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cfg-"));
    writeFileSync(
      join(dir, "pyproject.toml"),
      `[project]\nname="app"\n[tool.pylair]\napp-module="app.backend"\npython-version="3.13"\nprune=["extra/dir"]\n`,
    );
    const cfg = loadConfig(dir, py);
    assert.equal(cfg.appModule, "app.backend");
    assert.equal(cfg.pythonVersion, "3.13");
    assert.equal(cfg.source, "backend");
    assert.deepEqual(cfg.prune, ["extra/dir"]);
    rmSync(dir, { recursive: true });
  });

  test("requires app-module", { skip: !py }, () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cfg-"));
    writeFileSync(join(dir, "pyproject.toml"), "[project]\nname='a'\n[tool.pylair]\n");
    assert.throws(() => loadConfig(dir, py), /app-module is required/);
    rmSync(dir, { recursive: true });
  });

  test("errors without pyproject", () => {
    assert.throws(() => loadConfig("/nonexistent", "python3"), /no pyproject.toml/);
  });
});

describe("stageNotices", () => {
  test("collects pylair, interpreter and wheel licenses", () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-notices-"));
    const res = join(dir, "resources");
    // interpreter tree: licenses/ dir + a top-level LICENSE.txt
    mkdirSync(join(res, "python", "licenses"), { recursive: true });
    writeFileSync(join(res, "python", "licenses", "openssl.txt"), "Apache-2.0 openssl text");
    writeFileSync(join(res, "python", "LICENSE.txt"), "PSF-2.0 text");
    // wheels: one with a licenses dir, one with METADATA only
    mkdirSync(join(res, "site-packages", "httpx-0.28.1.dist-info", "licenses"), {
      recursive: true,
    });
    writeFileSync(
      join(res, "site-packages", "httpx-0.28.1.dist-info", "licenses", "LICENSE.md"),
      "BSD-3 httpx text",
    );
    mkdirSync(join(res, "site-packages", "foo-1.0.dist-info"), { recursive: true });
    writeFileSync(
      join(res, "site-packages", "foo-1.0.dist-info", "METADATA"),
      "Metadata-Version: 2.4\nName: foo\nLicense-Expression: MIT\n",
    );
    // pylair's own license files
    const pd = join(dir, "pylair-pkg");
    mkdirSync(pd, { recursive: true });
    writeFileSync(join(pd, "LICENSE-MIT"), "mit");
    writeFileSync(join(pd, "LICENSE-APACHE"), "apache");

    stageNotices(res, pd);

    const out = join(res, "THIRD-PARTY-NOTICES");
    assert.ok(existsSync(join(out, "pylair", "LICENSE-MIT")));
    assert.ok(existsSync(join(out, "python", "licenses", "openssl.txt")));
    assert.ok(existsSync(join(out, "python", "LICENSE.txt")));
    assert.ok(existsSync(join(out, "httpx", "licenses", "LICENSE.md")));
    assert.match(
      String(readFileSync(join(out, "foo", "METADATA-LICENSE.txt"))),
      /License-Expression: MIT/,
    );
    const idx = String(readFileSync(join(out, "INDEX.txt")));
    for (const name of ["pylair", "python", "httpx", "foo"])
      assert.match(idx, new RegExp(`${name}/`));
    rmSync(dir, { recursive: true });
  });

  test("writes a placeholder NOTICE when the interpreter has no licenses", () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-notices-"));
    const res = join(dir, "resources");
    mkdirSync(join(res, "python", "bin"), { recursive: true });
    mkdirSync(join(dir, "pd"), { recursive: true });
    stageNotices(res, join(dir, "pd"));
    assert.ok(existsSync(join(res, "THIRD-PARTY-NOTICES", "python", "NOTICE.txt")));
    rmSync(dir, { recursive: true });
  });
});

describe("stageSource", () => {
  function srcTree() {
    const dir = mkdtempSync(join(tmpdir(), "pylair-stage-"));
    for (const f of [
      "app.py",
      ".env",
      ".env.local",
      "key.pem",
      ".hidden/x.txt",
      "sub/id_rsa",
      "sub/ok.py",
    ]) {
      const p = join(dir, f);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, "x");
    }
    return dir;
  }

  test("stages normal files, skips and names secret-looking paths", async () => {
    const { stageSource } = await import("../dist/build.js");
    const src = srcTree();
    const dest = mkdtempSync(join(tmpdir(), "pylair-staged-"));
    const lines = [];
    const skipped = stageSource(src, dest, [], (m) => lines.push(m));

    for (const ok of ["app.py", join("sub", "ok.py")])
      assert.ok(existsSync(join(dest, ok)), `${ok} should be staged`);
    for (const denied of [".env", ".env.local", "key.pem", join(".hidden", "x.txt"), join("sub", "id_rsa")])
      assert.ok(!existsSync(join(dest, denied)), `${denied} must not ship`);
    for (const named of [".env", ".env.local", "key.pem", ".hidden", "sub/id_rsa"])
      assert.ok(skipped.includes(named), `skipped list missing ${named}`);
    // The warning counts, names, and tells the author how to opt in.
    assert.match(lines[0], /skipped 5/);
    for (const named of [".env", "key.pem", ".hidden", "sub/id_rsa"])
      assert.ok(lines[0].includes(named), `warning names ${named}`);
    assert.match(lines.join("\n"), /\[tool\.pylair\] include/);
    rmSync(src, { recursive: true });
    rmSync(dest, { recursive: true });
  });

  test("include reaches inside a denied directory", async () => {
    const { stageSource } = await import("../dist/build.js");
    const src = srcTree();
    const dest = mkdtempSync(join(tmpdir(), "pylair-staged-"));
    const lines = [];
    const skipped = stageSource(src, dest, [".hidden/x.txt"], (m) => lines.push(m));

    assert.ok(existsSync(join(dest, ".hidden", "x.txt")), "included file must stage");
    assert.ok(!existsSync(join(dest, ".env")), ".env still skipped");
    assert.ok(!skipped.includes(".hidden"), ".hidden not denied — it holds an include");
    assert.ok(!lines.join("\n").includes("x.txt"), "warning must not name the included file");
    rmSync(src, { recursive: true });
    rmSync(dest, { recursive: true });
  });
});

describe("meetsMinPython", () => {
  const cases = [
    ["3.9", false],
    ["3.10", false],
    ["3.11", false],
    ["3.12", true],
    ["3.13", true],
    ["4.0", true],
    ["garbage", false],
    ["", false],
  ];
  for (const [v, want] of cases) {
    test(`meetsMinPython(${JSON.stringify(v)}) → ${want}`, async () => {
      const { meetsMinPython } = await import("../dist/doctor.js");
      assert.equal(meetsMinPython(v, "3.12"), want);
    });
  }
});

describe("uv export / pip-install flags", () => {
  test("uv export drops dev deps and the project entry", () => {
    const args = uvExportArgs("/tmp/out.txt");
    assert.ok(args.includes("--no-dev"));
    assert.ok(args.includes("--no-emit-project"));
    assert.ok(args.includes("--frozen"));
  });

  test("--require-hashes only when the file carries --hash entries", () => {
    const hashed = "flask==3.0.0 \\n    --hash=sha256:abcdef\n";
    const plain = "flask==3.0.0\nrequests>=2\n";
    const a = uvPipInstallArgs("python3", "/t", "/r/r.txt", hashed);
    assert.ok(a.includes("--require-hashes"));
    const b = uvPipInstallArgs("python3", "/t", "/r/r.txt", plain);
    assert.ok(!b.includes("--require-hashes"));
  });
});
