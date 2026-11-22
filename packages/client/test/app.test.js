// App readiness PR 3 (docs/decisions/0007 §5): health().app returns
// the typed components block — null when the runtime never reports,
// absent entirely against an older host — and onApp subscribes to
// pylair://app for phase transitions only.
import { test, afterEach } from "node:test";
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "dist");
const HEALTH_CMD = "plugin:pylair|python_health";
const APP_EVT = "pylair://app";

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
  app: {
    components: {
      model: { phase: "ready", progress: null, detail: null, ms_ago: 3200 },
      index: { phase: "loading", progress: 0.4, detail: "warming", ms_ago: 90 },
    },
  },
};

/** Records invokes; answers python_health; exposes its listeners. */
function appBridge(overrides = {}) {
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

test("health().app reads the typed components block", async () => {
  const b = appBridge();
  m.setBridge(b);
  const s = await m.health();
  assert.equal(b.calls[0].cmd, HEALTH_CMD);
  const model = s.app.components.model;
  assert.equal(model.phase, "ready");
  assert.equal(model.progress, null);
  assert.equal(model.detail, null);
  assert.equal(typeof model.ms_ago, "number");
  const index = s.app.components.index;
  assert.equal(index.phase, "loading");
  assert.equal(index.progress, 0.4);
  assert.equal(index.detail, "warming");
});

test("a missing app block is tolerated (older host, or never reports)", async () => {
  for (const app of [undefined, null]) {
    const b = appBridge({
      async invoke(cmd, args) {
        this.calls.push({ cmd, args });
        if (cmd !== HEALTH_CMD) return null;
        const s = { ...SNAPSHOT };
        if (app === undefined) delete s.app;
        else s.app = null;
        return s;
      },
    });
    m.setBridge(b);
    const s = await m.health();
    assert.ok(s.app == null, `expected app ${app}, got ${s.app}`);
  }
});

test("onApp listens on pylair://app, delivers transitions, unlistens", async () => {
  const b = appBridge();
  m.setBridge(b);
  const seen = [];
  const unlisten = await m.onApp((c) => seen.push(c));
  assert.equal(typeof unlisten, "function");
  const cb = b.listeners.get(APP_EVT);
  assert.ok(cb, "the bridge saw a pylair://app listener");
  const flip = { version: 1, generation: 0, name: "model", phase: "ready" };
  cb(flip);
  assert.equal(seen.length, 1);
  assert.equal(seen[0].name, "model");
  assert.equal(seen[0].phase, "ready");
  assert.equal(seen[0].generation, 0);
  // Progress ticks are poll-only — the event never carries them.
  assert.equal(seen[0].progress, undefined);
  unlisten();
  assert.equal(
    b.listeners.has(APP_EVT),
    false,
    "unlisten removes the bridge-side subscription",
  );
});

test("python.onApp rides the default export", async () => {
  const b = appBridge();
  m.setBridge(b);
  assert.equal(typeof m.python.onApp, "function");
  const unlisten = await m.python.onApp(() => {});
  assert.equal(typeof unlisten, "function");
  unlisten();
});
