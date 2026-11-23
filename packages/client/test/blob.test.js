import { test, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import {
  call,
  stream,
  setBridge,
  BlobHandle,
  _resetForTests,
} from "../dist/index.js";

/**
 * Blob handles (decision 0010): a {"$blob": {id,mime,size}} value in a
 * result or stream chunk becomes a BlobHandle; the bytes ride back
 * through the pylair-blob: scheme (mocked fetch) and release rides
 * python_release_blob (mocked invoke).
 */

function fakeBridge() {
  const calls = [];
  const channels = new Map();
  let nextTask = 100;
  return {
    calls,
    emitTask(task, ev) {
      const cb = channels.get(task);
      assert.ok(cb, `no channel registered for task ${task}`);
      cb(ev);
    },
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      return true;
    },
    async invokeChannel(cmd, args, onEvent) {
      calls.push({ cmd, args });
      const task = nextTask++;
      channels.set(task, onEvent);
      return { task, protocol: 2 };
    },
    async listen() {
      return () => {};
    },
  };
}

const streamDone = (task, result) => ({ task, done: true, result });
const streamChunk = (task, seq, value) => ({ task, seq, value, done: false });
const tick = () => new Promise((r) => setTimeout(r, 0));

const TAG = { id: "blobid_0123456789ab", mime: "image/png", size: 5 };

let savedFetch;
let savedNavigator;

beforeEach(() => {
  _resetForTests();
  savedFetch = globalThis.fetch;
  savedNavigator = Object.getOwnPropertyDescriptor(globalThis, "navigator");
});

afterEach(() => {
  globalThis.fetch = savedFetch;
  if (savedNavigator)
    Object.defineProperty(globalThis, "navigator", savedNavigator);
  else delete globalThis.navigator;
});

/** A fetch stub that serves `data` capped at `cap` bytes per request. */
function cappedServer(data, cap, requests = []) {
  return async (url, init = {}) => {
    const m = /bytes=(\d+)-(\d*)/.exec(init.headers?.Range ?? "");
    const start = m ? Number(m[1]) : 0;
    const end = Math.min(data.length, start + cap);
    requests.push({ url, range: init.headers?.Range ?? null, start, end });
    return {
      ok: true,
      status: 206,
      arrayBuffer: async () => data.slice(start, end).buffer,
    };
  };
}

function withUserAgent(ua, fn) {
  Object.defineProperty(globalThis, "navigator", {
    value: { userAgent: ua },
    configurable: true,
  });
  return fn();
}

// ---- decode ------------------------------------------------------------------

test("a $blob tag in a call result decodes to a BlobHandle", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("make_blob");
  await tick();
  b.emitTask(100, streamDone(100, { $blob: TAG }));
  const h = await p;
  assert.ok(h instanceof BlobHandle);
  assert.equal(h.id, TAG.id);
  assert.equal(h.mime, "image/png");
  assert.equal(h.size, 5);
});

test("a $blob tag inside a stream chunk decodes", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("blob_stream")[Symbol.asyncIterator]();
  const first = it.next(); // starts the generator — registers the channel
  await tick();
  b.emitTask(100, streamChunk(100, 0, { $blob: TAG }));
  const { value } = await first;
  assert.ok(value instanceof BlobHandle);
  const done = it.next();
  await tick();
  b.emitTask(100, streamDone(100, null));
  await done;
});

test("$blob decodes inside nested values", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("nested");
  await tick();
  b.emitTask(
    100,
    streamDone(100, {
      outer: { list: [1, { $blob: TAG }], named: { $blob: TAG } },
    }),
  );
  const r = await p;
  assert.ok(r.outer.list[1] instanceof BlobHandle);
  assert.ok(r.outer.named instanceof BlobHandle);
  assert.equal(r.outer.list[0], 1);
});

test("$b64 still decodes to Uint8Array (capability-off fallback)", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("legacy_bytes");
  await tick();
  b.emitTask(100, streamDone(100, { $b64: btoa("hi") }));
  const r = await p;
  assert.ok(r instanceof Uint8Array);
  assert.deepEqual([...r], [104, 105]);
});

for (const bad of [null, 5, { id: 1 }]) {
  test(`malformed $blob ${JSON.stringify(bad)} stays plain data`, async () => {
    const b = fakeBridge();
    setBridge(b);
    const p = call("x");
    await tick();
    b.emitTask(100, streamDone(100, { $blob: bad }));
    assert.deepEqual(await p, { $blob: bad });
  });
}

// ---- url ---------------------------------------------------------------------

test(".url uses the Windows form under a Windows UA", () => {
  withUserAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) WebView2", () => {
    const h = new BlobHandle("abc", "image/png", 1);
    assert.equal(h.url, `http://pylair-blob.localhost/b/abc`);
  });
});

test(".url uses the scheme form off Windows", () => {
  withUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X) WebKit", () => {
    const h = new BlobHandle("abc", "image/png", 1);
    assert.equal(h.url, `pylair-blob://localhost/b/abc`);
  });
});

// ---- stream() ------------------------------------------------------------------

test("stream() advances by bytes received across short 206s", async () => {
  const data = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
  const requests = [];
  globalThis.fetch = cappedServer(data, 3, requests); // server caps at 3 bytes
  const h = new BlobHandle("x", "application/octet-stream", data.length);
  const chunks = [];
  for await (const c of h.stream()) chunks.push(c);
  assert.equal(
    new Uint8Array(await new Blob(chunks).arrayBuffer()).join(","),
    data.join(","),
  );
  // 10 bytes at 3/request: 4 range requests, positions from bytes received.
  assert.deepEqual(
    requests.map((r) => r.range),
    ["bytes=0-9", "bytes=3-9", "bytes=6-9", "bytes=9-9"],
  );
  assert.equal(requests.length, 4, "stream must stop at size — no 5th read");
});

for (const status of [403, 404, 410, 416]) {
  test(`stream() surfaces HTTP ${status} as a stream error`, async () => {
    globalThis.fetch = async () => ({
      ok: false,
      status,
      arrayBuffer: async () => new ArrayBuffer(0),
    });
    const h = new BlobHandle("x", "application/octet-stream", 10);
    await assert.rejects(
      async () => {
        for await (const _ of h.stream()) void _;
      },
      new RegExp(`${status}`),
    );
  });
}

test("a zero-size blob streams nothing", async () => {
  globalThis.fetch = async () => assert.fail("fetch must not run");
  const h = new BlobHandle("x", "application/octet-stream", 0);
  for await (const _ of h.stream()) assert.fail("no chunks expected");
});

// ---- mediaUrl() ----------------------------------------------------------------

test("mediaUrl() builds a blob: object URL and release() revokes it", async () => {
  const data = new Uint8Array([9, 8, 7]);
  globalThis.fetch = cappedServer(data, 64);
  const revoked = [];
  const savedRevoke = URL.revokeObjectURL;
  URL.revokeObjectURL = (u) => revoked.push(u);
  try {
    const b = fakeBridge();
    setBridge(b);
    const h = new BlobHandle("m", "audio/wav", data.length);
    const u = await h.mediaUrl();
    assert.match(u, /^blob:/);
    await h.release();
    assert.deepEqual(revoked, [u]);
    assert.equal(
      b.calls.filter((c) => c.cmd === "plugin:pylair|python_release_blob").length,
      1,
    );
  } finally {
    URL.revokeObjectURL = savedRevoke;
  }
});

test("concurrent mediaUrl() calls share one object URL", async () => {
  globalThis.fetch = cappedServer(new Uint8Array([1, 2, 3]), 64);
  const savedCreate = URL.createObjectURL;
  let made = 0;
  URL.createObjectURL = () => `blob:shared-${++made}`;
  try {
    const h = new BlobHandle("m", "audio/wav", 3);
    const [u1, u2] = await Promise.all([h.mediaUrl(), h.mediaUrl()]);
    assert.equal(u1, u2);
    assert.equal(made, 1, "two callers, one object URL");
  } finally {
    URL.createObjectURL = savedCreate;
  }
});

test("release() during an in-flight mediaUrl() leaves no unrevoked URL", async () => {
  let releaseFetch;
  globalThis.fetch = () => new Promise((r) => (releaseFetch = r));
  const created = [];
  const revoked = [];
  const savedCreate = URL.createObjectURL;
  const savedRevoke = URL.revokeObjectURL;
  URL.createObjectURL = () => {
    const u = `blob:flight-${created.length}`;
    created.push(u);
    return u;
  };
  URL.revokeObjectURL = (u) => revoked.push(u);
  try {
    const b = fakeBridge();
    setBridge(b);
    const h = new BlobHandle("m", "audio/wav", 10);
    const p = h.mediaUrl();
    await h.release(); // lands while the ranged read is still pending
    releaseFetch({
      ok: true,
      status: 206,
      arrayBuffer: async () => new Uint8Array(10).buffer,
    });
    await assert.rejects(p, /released/);
    assert.deepEqual(revoked, created, "the URL built after release is revoked");
  } finally {
    URL.createObjectURL = savedCreate;
    URL.revokeObjectURL = savedRevoke;
  }
});

test("mediaUrl() over the 256 MiB cap throws naming .url/stream()", async () => {
  const h = new BlobHandle("big", "video/mp4", 256 * 1024 * 1024 + 1);
  await assert.rejects(h.mediaUrl(), /too large for mediaUrl.*\.url.*stream\(\)/s);
});

// ---- release() -------------------------------------------------------------------

test("release() is idempotent and sends the id once", async () => {
  const b = fakeBridge();
  setBridge(b);
  const h = new BlobHandle("r1", "image/png", 1);
  await h.release();
  await h.release();
  const rel = b.calls.filter((c) => c.cmd === "plugin:pylair|python_release_blob");
  assert.equal(rel.length, 1);
  assert.deepEqual(rel[0].args, { id: "r1" });
});

test("a released handle's readers fail with a clear error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const h = new BlobHandle("r2", "image/png", 1);
  await h.release();
  assert.throws(() => h.url, /released/);
  assert.throws(() => h.stream(), /released/);
  await assert.rejects(h.mediaUrl(), /released/);
  await assert.rejects(h.arrayBuffer(), /released/);
});
