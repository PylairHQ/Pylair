import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync, readdirSync, rmSync, existsSync, statSync } from "node:fs";
import fsModule from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import { Readable } from "node:stream";
import { createHash } from "node:crypto";

import { matchGlob } from "../dist/glob.js";
import { stageNotices, download, downloadWithRetry, downloadTimeoutMs, ensureTarball, uvExportArgs, uvPipInstallArgs, requirementsHaveHashes, PYC_INVALIDATION_MODE } from "../dist/build.js";
import { DEFAULT_PRUNE, prune } from "../dist/prune.js";
import { loadConfig, windowsSigningConfigured } from "../dist/config.js";

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

describe("downloadWithRetry", () => {
  const BODY = Buffer.from(Array.from({ length: 100 }, (_, i) => i));
  const sha = createHash("sha256").update(BODY).digest("hex");
  const noSleep = async () => {};

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
  // Handles both get(url, cb) and get(url, { headers }, cb); the handler
  // sees the request headers so tests can assert on Range.
  const rangeGet = (handler) => (u, a, b) => {
    const [o, cb] = typeof a === "function" ? [{}, a] : [a, b];
    const req = fakeRequest();
    queueMicrotask(() => handler(u, o?.headers ?? {}, cb, req));
    return req;
  };
  const reply = (chunks, { status = 200, headers = {}, error } = {}) => {
    const r = Readable.from(
      (async function* () {
        for (const c of chunks) yield c;
        if (error) throw new Error(error);
      })(),
    );
    r.statusCode = status;
    r.headers = headers;
    return r;
  };
  const full = () => reply([BODY], { headers: { "content-length": String(BODY.length) } });
  // First `n` bytes, then the connection resets.
  const cutAt = (n) =>
    reply([BODY.subarray(0, n)], {
      headers: { "content-length": String(BODY.length) },
      error: "ECONNRESET",
    });
  const stalled = () => {
    const r = new Readable({ read() {} });
    r.statusCode = 200;
    r.headers = {};
    return r;
  };
  const tmp = () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-retry-"));
    return { dir, to: join(dir, "asset.tar.gz") };
  };
  const opts = (get, extra = {}) => ({ get, sleep: noSleep, log: () => {}, ...extra });

  test("a stall on attempt 1 is retried and attempt 2 succeeds", async () => {
    const { dir, to } = tmp();
    let calls = 0;
    const lines = [];
    const get = rangeGet((u, h, cb) => cb(++calls === 1 ? stalled() : full()));
    await downloadWithRetry("https://x/a", to, opts(get, { timeoutMs: 50, log: (l) => lines.push(l) }));
    assert.equal(calls, 2);
    assert.deepEqual(readFileSync(to), BODY);
    assert.equal(lines.length, 1, "one line per retry");
    assert.match(lines[0], /^retrying \(2 of 4\) in \d+s: .*timed out/);
    rmSync(dir, { recursive: true });
  });

  test("a request error between 'finish' and 'close' does not let the stale close handler touch the next attempt's .part", async () => {
    // Forces the interleaving without sleeps or load. The write stream
    // closes its fd through fs.close, so holding that one callback keeps
    // attempt 1 between 'finish' and 'close'. While it is held, the request
    // errors (fail() settles attempt 1 with out === undefined, keeping the
    // resumable .part) and attempt 2 starts, reopening the same .part. Then
    // the held close is released: attempt 1's close handler must not rename
    // attempt 2's in-progress .part into place, and attempt 2 must complete.
    const { dir, to } = tmp();
    const uncaught = [];
    const onUncaught = (e) => uncaught.push(e);
    process.on("uncaughtException", onUncaught);
    const realClose = fsModule.close;
    let held; // () => the real fs.close of attempt 1's stream
    fsModule.close = (fd, cb) => {
      if (held === undefined) held = () => realClose(fd, cb);
      else realClose(fd, cb);
    };
    let calls = 0;
    let req1;
    let gated;
    let attempt2Started;
    const attempt2 = new Promise((r) => (attempt2Started = r));
    const get = rangeGet((u, h, cb, req) => {
      calls++;
      if (calls === 1) {
        req1 = req;
        return cb(full());
      }
      // Attempt 2 answers, then waits for the test to let its body flow.
      gated = new Readable({ read() {} });
      gated.statusCode = 200;
      gated.headers = { "content-length": String(BODY.length) };
      cb(gated);
      attempt2Started();
    });
    const lines = [];
    try {
      const done = downloadWithRetry("https://x/a", to, opts(get, { timeoutMs: 60000, log: (l) => lines.push(l) }));
      // Attempt 1 has finished writing and is waiting on the held close.
      for (let i = 0; held === undefined; i++) {
        assert.ok(i < 2000, "fs.close of attempt 1's stream was never reached");
        await new Promise((r) => setImmediate(r));
      }
      // The request errors now: fail() runs between 'finish' and 'close'.
      req1.emit("error", new Error("ECONNRESET"));
      await attempt2; // attempt 2 has opened .part with "w" and is in flight
      await new Promise((r) => setImmediate(r));
      held(); // attempt 1's stale close handler runs now
      await new Promise((r) => setImmediate(r));
      await new Promise((r) => setImmediate(r));
      assert.ok(existsSync(`${to}.part`), "the stale handler must not take attempt 2's .part");
      assert.ok(!existsSync(to), "nothing may reach the final path before attempt 2 completes");
      gated.push(BODY);
      gated.push(null);
      await Promise.race([
        done,
        new Promise((_, rej) => setTimeout(() => rej(new Error("download never settled")), 5000)),
      ]);
      assert.deepEqual(uncaught.map(String), []);
      assert.equal(calls, 2);
      assert.deepEqual(readFileSync(to), BODY);
      assert.ok(!existsSync(`${to}.part`));
      assert.equal(lines.length, 1, "one retry line");
    } finally {
      fsModule.close = realClose;
      process.removeListener("uncaughtException", onUncaught);
      if (held) held = undefined;
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("a rename that fails rejects the download instead of throwing inside an event handler", async () => {
    // `to` is a non-empty directory, so renaming the finished .part onto it
    // fails (EISDIR / EPERM). That error must reach the caller as a
    // rejection; thrown inside the stream's 'close' handler it would be an
    // uncaught exception that kills the process.
    const { dir, to } = tmp();
    mkdirSync(to);
    writeFileSync(join(to, "keep"), "x");
    const uncaught = [];
    const onUncaught = (e) => uncaught.push(e);
    process.on("uncaughtException", onUncaught);
    try {
      const get = rangeGet((u, h, cb) => cb(full()));
      await assert.rejects(
        Promise.race([
          downloadWithRetry("https://x/a", to, opts(get)),
          new Promise((_, rej) => setTimeout(() => rej(new Error("download never settled")), 5000)),
        ]),
        (e) => !/never settled/.test(e.message) && ["EISDIR", "EPERM", "EEXIST", "ENOTEMPTY"].includes(e.code),
      );
      assert.deepEqual(uncaught.map(String), []);
      assert.ok(existsSync(join(to, "keep")), "the existing directory is untouched");
    } finally {
      process.removeListener("uncaughtException", onUncaught);
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("resume: Range is sent and a 206 with a matching Content-Range appends", async () => {
    const { dir, to } = tmp();
    const seen = [];
    const get = rangeGet((u, h, cb) => {
      seen.push(h.Range);
      if (seen.length === 1) return cb(cutAt(40));
      const n = Number(/^bytes=(\d+)-$/.exec(h.Range)[1]);
      assert.ok(n > 0, "the kept .part must have been continued");
      cb(
        reply([BODY.subarray(n)], {
          status: 206,
          headers: {
            "content-length": String(BODY.length - n),
            "content-range": `bytes ${n}-${BODY.length - 1}/${BODY.length}`,
          },
        }),
      );
    });
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.equal(seen[0], undefined, "first attempt has no Range");
    assert.match(seen[1], /^bytes=\d+-$/);
    assert.deepEqual(readFileSync(to), BODY);
    assert.equal(createHash("sha256").update(readFileSync(to)).digest("hex"), sha);
    rmSync(dir, { recursive: true });
  });

  test("a server that answers 200 to a Range request is restarted, not appended", async () => {
    const { dir, to } = tmp();
    let calls = 0;
    const get = rangeGet((u, h, cb) => cb(++calls === 1 ? cutAt(40) : full()));
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.equal(calls, 2);
    assert.equal(statSync(to).size, BODY.length, "no duplicated prefix");
    assert.deepEqual(readFileSync(to), BODY);
    rmSync(dir, { recursive: true });
  });

  test("a 416 deletes the .part and restarts from zero", async () => {
    const { dir, to } = tmp();
    const seen = [];
    const get = rangeGet((u, h, cb) => {
      seen.push(h.Range);
      if (seen.length === 1) return cb(cutAt(40));
      if (h.Range) return cb(reply([], { status: 416 }));
      cb(full());
    });
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.equal(seen.length, 3);
    assert.equal(seen[0], undefined);
    assert.match(seen[1], /^bytes=\d+-$/);
    assert.equal(seen[2], undefined, "restart carries no Range");
    assert.deepEqual(readFileSync(to), BODY);
    rmSync(dir, { recursive: true });
  });

  test("a 206 whose Content-Range starts elsewhere is restarted, never appended", async () => {
    const { dir, to } = tmp();
    const seen = [];
    const get = rangeGet((u, h, cb) => {
      seen.push(h.Range);
      if (seen.length === 1) return cb(cutAt(40));
      if (h.Range)
        return cb(
          reply([BODY.subarray(10)], {
            status: 206,
            headers: { "content-range": "bytes 10-99/100", "content-length": "90" },
          }),
        );
      cb(full());
    });
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.deepEqual(readFileSync(to), BODY);
    rmSync(dir, { recursive: true });
  });

  test("the Range header is carried through a redirect", async () => {
    const { dir, to } = tmp();
    writeFileSync(`${to}.part`, "hello"); // left behind by an earlier run
    const seen = [];
    const get = rangeGet((u, h, cb) => {
      seen.push([u, h.Range]);
      if (u === "https://x/a")
        return cb(reply([], { status: 302, headers: { location: "https://cdn.example/obj" } }));
      cb(
        reply([Buffer.from("world")], {
          status: 206,
          headers: { "content-range": "bytes 5-9/10", "content-length": "5" },
        }),
      );
    });
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.deepEqual(seen, [
      ["https://x/a", "bytes=5-"],
      ["https://cdn.example/obj", "bytes=5-"],
    ]);
    assert.equal(readFileSync(to, "utf-8"), "helloworld");
    rmSync(dir, { recursive: true });
  });

  test("a 404 is not retried", async () => {
    const { dir, to } = tmp();
    let calls = 0;
    let slept = 0;
    const get = rangeGet((u, h, cb) => {
      calls++;
      cb(reply([], { status: 404 }));
    });
    await assert.rejects(
      downloadWithRetry("https://x/a", to, opts(get, { sleep: async () => void slept++ })),
      /404/,
    );
    assert.equal(calls, 1);
    assert.equal(slept, 0);
    rmSync(dir, { recursive: true });
  });

  test("5xx and 429 are retried", async () => {
    const { dir, to } = tmp();
    const statuses = [503, 429, 200];
    let i = 0;
    const get = rangeGet((u, h, cb) => {
      const st = statuses[i++];
      cb(st === 200 ? full() : reply([], { status: st }));
    });
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.equal(i, 3);
    assert.deepEqual(readFileSync(to), BODY);
    rmSync(dir, { recursive: true });
  });

  test("a refused redirect and a redirect loop are not retried", async () => {
    const { dir, to } = tmp();
    let calls = 0;
    const insecure = rangeGet((u, h, cb) => {
      calls++;
      cb(reply([], { status: 302, headers: { location: "http://insecure.example/x" } }));
    });
    await assert.rejects(downloadWithRetry("https://x/a", to, opts(insecure)), /non-https|refus/i);
    assert.equal(calls, 1);
    calls = 0;
    const loop = rangeGet((u, h, cb) => {
      calls++;
      cb(reply([], { status: 302, headers: { location: `${u}/n` } }));
    });
    await assert.rejects(downloadWithRetry("https://x/a", to, opts(loop)), /redirect/i);
    assert.equal(calls, 6, "one attempt only: the first request plus 5 followed hops");
    rmSync(dir, { recursive: true });
  });

  test("an incomplete body is retried", async () => {
    const { dir, to } = tmp();
    let calls = 0;
    const get = rangeGet((u, h, cb) => {
      // Ends cleanly but short of its Content-Length.
      cb(
        ++calls === 1
          ? reply([BODY.subarray(0, 10)], { headers: { "content-length": "100" } })
          : full(),
      );
    });
    await downloadWithRetry("https://x/a", to, opts(get));
    assert.equal(calls, 2);
    assert.deepEqual(readFileSync(to), BODY);
    rmSync(dir, { recursive: true });
  });

  test("gives up after 4 attempts and names the cache path, sizes and hash", async () => {
    const { dir, to } = tmp();
    let calls = 0;
    const lines = [];
    const get = rangeGet((u, h, cb) => {
      calls++;
      // 200 each time (Range ignored), 10 bytes of a 32 byte file, then reset.
      cb(
        reply([Buffer.alloc(10, 1)], {
          headers: { "content-length": "32" },
          error: "ECONNRESET",
        }),
      );
    });
    const err = await downloadWithRetry(
      "https://x/asset",
      to,
      opts(get, { sha256: sha, log: (l) => lines.push(l) }),
    ).then(
      () => null,
      (e) => e,
    );
    assert.ok(err, "expected a failure");
    assert.equal(calls, 4);
    assert.equal(lines.length, 3);
    assert.match(lines[2], /^retrying \(4 of 4\)/);
    assert.ok(err.message.includes("https://x/asset"), err.message);
    assert.ok(err.message.includes(`place it at ${resolve(to)}`), err.message);
    assert.match(err.message, /received 10 of 32 bytes/);
    assert.match(err.message, /Expected size 32 bytes, SHA-256 [0-9a-f]{64}/);
    assert.ok(err.message.includes(sha));
    assert.match(err.message, /verifies it and re-downloads if it does not match/);
    assert.ok(!err.message.includes(String.fromCharCode(0x2014)), "no em dash in the message");
    rmSync(dir, { recursive: true });
  });

  test("backoff delays grow roughly 1s, 3s, 8s", async () => {
    const { dir, to } = tmp();
    const waits = [];
    const get = rangeGet((u, h, cb) => cb(reply([], { status: 503 })));
    await assert.rejects(
      downloadWithRetry("https://x/a", to, opts(get, { sleep: async (ms) => void waits.push(ms) })),
    );
    assert.equal(waits.length, 3);
    for (const [w, base] of [[waits[0], 1000], [waits[1], 3000], [waits[2], 8000]])
      assert.ok(w >= base * 0.75 && w <= base * 1.25, `${w} not within 25% of ${base}`);
    rmSync(dir, { recursive: true });
  });

  test("a slow download prints progress, a fast one stays quiet", async () => {
    const { dir, to } = tmp();
    // Slow producer so the (injected, short) progress interval fires.
    const slowBody = new Readable({ read() {} });
    slowBody.statusCode = 200;
    slowBody.headers = { "content-length": "60" };
    let n = 0;
    const iv = setInterval(() => {
      slowBody.push(BODY.subarray(n * 10, n * 10 + 10));
      if (++n === 6) {
        slowBody.push(null);
        clearInterval(iv);
      }
    }, 30);
    const lines = [];
    await download("https://x/a", to, {
      get: rangeGet((u, h, cb) => cb(slowBody)),
      progress: (l) => lines.push(l),
      progressMs: 40,
    });
    assert.ok(lines.length >= 1, "expected at least one progress line");
    assert.match(lines[0], /^downloaded \d+\.\d of \d+\.\d MB \(\d+%\)$/);
    assert.ok(!/[\u001b\r]/.test(lines.join("")), "no terminal control codes");

    const quiet = [];
    await download("https://x/b", join(dir, "b.tar.gz"), {
      get: rangeGet((u, h, cb) => cb(full())),
      progress: (l) => quiet.push(l),
    });
    assert.deepEqual(quiet, [], "a fast download prints nothing");
    rmSync(dir, { recursive: true });
  });

  test("PYLAIR_DOWNLOAD_TIMEOUT_MS: default, override, clamp, bad value", () => {
    const t = (v) => downloadTimeoutMs(v === undefined ? {} : { PYLAIR_DOWNLOAD_TIMEOUT_MS: v });
    assert.equal(t(undefined), 30_000);
    assert.equal(t(""), 30_000);
    assert.equal(t("45000"), 45_000);
    assert.equal(t("1000"), 5_000, "clamped up to 5s");
    assert.equal(t("999999999"), 600_000, "clamped down to 600s");
    assert.throws(() => t("abc"), /PYLAIR_DOWNLOAD_TIMEOUT_MS.*"abc"/);
    assert.throws(() => t("1.5"), /PYLAIR_DOWNLOAD_TIMEOUT_MS/);
    assert.throws(() => t("-5"), /PYLAIR_DOWNLOAD_TIMEOUT_MS/);
  });

  test("a bad PYLAIR_DOWNLOAD_TIMEOUT_MS rejects the download with a clear error", async () => {
    const { dir, to } = tmp();
    const prev = process.env.PYLAIR_DOWNLOAD_TIMEOUT_MS;
    process.env.PYLAIR_DOWNLOAD_TIMEOUT_MS = "soon";
    try {
      await assert.rejects(
        downloadWithRetry("https://x/a", to, opts(rangeGet((u, h, cb) => cb(full())))),
        /PYLAIR_DOWNLOAD_TIMEOUT_MS/,
      );
    } finally {
      if (prev === undefined) delete process.env.PYLAIR_DOWNLOAD_TIMEOUT_MS;
      else process.env.PYLAIR_DOWNLOAD_TIMEOUT_MS = prev;
    }
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
    assert.ok(requirementsHaveHashes(hashed));
    assert.ok(!requirementsHaveHashes(plain));
    // A file's hash mention inside a comment does not count.
    assert.ok(!requirementsHaveHashes("flask==3.0.0  # pinned, see --hash\n"));
    // The exported-lock path mixes hashed deps with hash-less file://
    // path deps — require-hashes=false keeps them installable.
    const exported = uvPipInstallArgs("python3", "/t", "/r/r.exported.txt", false);
    assert.ok(!exported.includes("--require-hashes"));
    const fallback = uvPipInstallArgs("python3", "/t", "/r/r.txt", true);
    assert.ok(fallback.includes("--require-hashes"));
  });
});

describe("windowsSigningConfigured", () => {
  const saved = process.env.WINDOWS_CERTIFICATE_THUMBPRINT;
  const clear = () => delete process.env.WINDOWS_CERTIFICATE_THUMBPRINT;
  const restore = () => {
    if (saved === undefined) clear();
    else process.env.WINDOWS_CERTIFICATE_THUMBPRINT = saved;
  };
  const appWithConf = (conf) => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-sign-"));
    mkdirSync(join(dir, "src-tauri"), { recursive: true });
    if (conf !== null)
      writeFileSync(join(dir, "src-tauri", "tauri.conf.json"), conf);
    return dir;
  };

  test("env alone counts as configured", () => {
    process.env.WINDOWS_CERTIFICATE_THUMBPRINT = "abc123";
    try {
      assert.ok(windowsSigningConfigured(mkdtempSync(join(tmpdir(), "x-"))));
    } finally {
      restore();
    }
  });

  test("config certificateThumbprint alone counts", () => {
    clear();
    const dir = appWithConf(
      JSON.stringify({ bundle: { windows: { certificateThumbprint: "abc" } } }),
    );
    assert.ok(windowsSigningConfigured(dir));
  });

  test("config signCommand alone counts", () => {
    clear();
    const dir = appWithConf(
      JSON.stringify({ bundle: { windows: { signCommand: "signtool %1" } } }),
    );
    assert.ok(windowsSigningConfigured(dir));
  });

  test("neither env nor config warns (not configured)", () => {
    clear();
    assert.ok(!windowsSigningConfigured(appWithConf(JSON.stringify({ bundle: {} }))));
    assert.ok(!windowsSigningConfigured(appWithConf("not json")));
    assert.ok(!windowsSigningConfigured(mkdtempSync(join(tmpdir(), "x-")))); // no file
  });
});
