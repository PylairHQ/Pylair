// Runtime health PR 4 (docs/decisions/0006 §4+§5): health() invokes
// python_health and returns the typed snapshot; onHealth subscribes to
// pylair://health and delivers verdict-change payloads; against a
// plugin that predates the command the invoke rejects — never hangs.
import { test, afterEach } from "node:test";
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "dist");
const HEALTH_CMD = "plugin:pylair|python_health";
const HEALTH_EVT = "pylair://health";

const m = await import(pathToFileURL(join(dist, "index.js")).href);

afterEach(() => m.setBridge(null));

const SNAPSHOT = {
  version: 1,
  kind: "bridge",
  state: "ready",
  in_state_ms: 12000,
  generation: 0,
  uptime_ms: 11800,
  restarts: {
    crashes_in_window: 0,
    breaker: { crashes: 5, window_ms: 60000 },
    backoff_ms: 500,
    last_exit: null,
  },
  liveness: {
    last_pong_age_ms: 40,
    rtt_ms: 2,
    missed_pongs: 0,
    missed_pongs_to_kill: 3,
    loop_stale_ms: 0,
    transport: "ok",
    loop: "ok",
  },
  load: { tasks: 1, streams: 0, isolated: 0, queued: 0, stream_bytes_total: 0 },
  self: null,
};

/** Records invokes; answers python_health; exposes its listeners. */
function healthBridge(overrides = {}) {
  const calls = [];
  const listeners = new Map();
  const b = {
    calls,
    listeners,
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === HEALTH_CMD) return SNAPSHOT;
      return null;
    },
    async invokeChannel() {
      throw new Error("no calls expected");
    },
    async listen(event, cb) {
      listeners.set(event, cb);
      return () => listeners.delete(event);
    },
    ...overrides,
  };
  return b;
}

test("health() invokes python_health and returns the typed snapshot", async () => {
  const b = healthBridge();
  m.setBridge(b);
  const s = await m.health();
  assert.equal(b.calls.length, 1);
  assert.equal(b.calls[0].cmd, HEALTH_CMD);
  assert.deepEqual(b.calls[0].args, undefined);
  assert.equal(s.version, 1);
  assert.equal(s.kind, "bridge");
  assert.equal(s.state, "ready");
  assert.equal(s.liveness.transport, "ok");
  assert.equal(s.liveness.loop, "ok");
  assert.equal(s.self, null);
});

test("onHealth listens on pylair://health, delivers, and unlistens", async () => {
  const b = healthBridge();
  m.setBridge(b);
  const seen = [];
  const unlisten = await m.onHealth((c) => seen.push(c));
  assert.equal(typeof unlisten, "function");
  const cb = b.listeners.get(HEALTH_EVT);
  assert.ok(cb, "the bridge saw a pylair://health listener");
  const flip = {
    version: 1,
    generation: 0,
    liveness: {
      last_pong_age_ms: 40,
      rtt_ms: 2,
      missed_pongs: 0,
      missed_pongs_to_kill: 3,
      loop_stale_ms: 2600,
      transport: "ok",
      loop: "busy",
    },
  };
  cb(flip);
  assert.equal(seen.length, 1);
  assert.equal(seen[0].liveness.loop, "busy");
  assert.equal(seen[0].generation, 0);
  unlisten();
  assert.equal(
    b.listeners.has(HEALTH_EVT),
    false,
    "unlisten removes the bridge-side subscription",
  );
});

test("health() against an old plugin rejects, never hangs", { timeout: 5_000 }, async () => {
  // A plugin that predates python_health fails the invoke — Tauri's
  // command-not-found / ACL error, NOT a {code} protocol error.
  const b = healthBridge({
    async invoke(cmd, args) {
      this.calls.push({ cmd, args });
      throw new Error(`command ${cmd} not found`);
    },
  });
  m.setBridge(b);
  await assert.rejects(m.health(), /python_health not found/);
});

test("python.health and python.onHealth ride the default export", async () => {
  const b = healthBridge();
  m.setBridge(b);
  assert.equal(typeof m.python.health, "function");
  assert.equal(typeof m.python.onHealth, "function");
  const s = await m.python.health();
  assert.equal(s.version, 1);
});
