// R2b: the installed-webview self-test is loaded on demand. Outside the
// PYLAIR_SELF_TEST_WEBVIEW flag nothing may load dist/selftest.js; under
// the flag a missing chunk must end in a failing verdict, not a hang; and
// with the chunk present the call still reaches the real owner logic.
import { test, after } from "node:test";
import assert from "node:assert/strict";
import { cpSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "dist");
const REPORT = "plugin:pylair|python_selftest_report";

const temps = [];
after(() => {
  for (const d of temps) rmSync(d, { recursive: true, force: true });
});

/** A copy of dist/ with the self-test chunk removed, as a module root. */
function distWithoutSelftest() {
  const dir = mkdtempSync(join(tmpdir(), "pylair-lazy-"));
  temps.push(dir);
  cpSync(dist, dir, { recursive: true });
  for (const f of ["selftest.js", "selftest.js.map", "selftest.d.ts", "selftest.d.ts.map"])
    rmSync(join(dir, f), { force: true });
  // The copy sits outside the package, so it needs its own ESM marker.
  writeFileSync(join(dir, "package.json"), '{"type":"module"}');
  assert.ok(!existsSync(join(dir, "selftest.js")));
  return dir;
}

const setFlag = (on) => {
  if (on) globalThis.__PYLAIR_SELF_TEST__ = true;
  else delete globalThis.__PYLAIR_SELF_TEST__;
};

/** Records invokes; answers nothing. Enough for the import-failure path. */
function recordingBridge() {
  const calls = [];
  return {
    calls,
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      return null;
    },
    async invokeChannel() {
      throw new Error("no calls expected");
    },
    async listen() {
      return () => {};
    },
  };
}

test("flag unset: maybeSelfTest is a no-op and never loads selftest.js", { timeout: 10_000 }, async () => {
  setFlag(false);
  const dir = distWithoutSelftest();
  const m = await import(pathToFileURL(join(dir, "index.js")).href);
  assert.equal(await m.maybeSelfTest(), undefined);
});

test("flag set, chunk missing: reports a failing verdict and rejects", { timeout: 10_000 }, async () => {
  const dir = distWithoutSelftest();
  const m = await import(pathToFileURL(join(dir, "index.js")).href);
  const b = recordingBridge();
  m.setBridge(b);
  setFlag(true);
  try {
    await assert.rejects(m.maybeSelfTest());
  } finally {
    setFlag(false);
  }
  const reports = b.calls.filter((c) => c.cmd === REPORT);
  assert.equal(reports.length, 1, "exactly one verdict");
  const { result } = reports[0].args;
  assert.equal(result.ok, false);
  assert.match(result.checks["selftest-import"], /^FAIL: /);
  assert.deepEqual(Object.keys(result), ["ok", "checks"], "same payload shape as the owner role");
});

/**
 * A bridge that plays the runtime for the owner role: log_stderr is
 * missing (the probe skips), count streams 0..n-1 and ends in
 * TASK_CANCELLED once cancelled, add answers a+b, and the intruder's
 * report arrives clean on pylair://selftest-b.
 */
const DENIED = { denied: true, code: -32050, fn: "add", window: "win-b" };

function runtimeBridge(denyProbe = { viaCall: DENIED, viaRaw: DENIED }, fixture = true, opts = {}) {
  const { fairness = false, fairnessProbe = { ok: true, value: "hello" } } = opts;
  // "ok" (default): the crate carries python_health. "missing": an old
  // crate — Tauri rejects the invoke before the plugin runs (a plain
  // string, no numeric code). "error": the plugin answers a coded
  // ErrorObj — always a real failure, never a skip.
  const { health = "ok" } = opts;
  const calls = [];
  const listens = [];
  const cancelHooks = new Map();
  let nextTask = 1;
  let heldN = 0;
  let holdCalls = 0;
  let bSeen = 0;
  const later = (f) => setTimeout(f, 0);
  const CELL = { in_use: 0, ceiling: 3, cap: 4 };
  return {
    calls,
    listens,
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === "plugin:pylair|python_cancel") {
        cancelHooks.get(args.task)?.();
        return "cancelled";
      }
      if (cmd === "plugin:pylair|python_health") {
        if (health === "missing")
          throw new Error("pylair.python_health not allowed. Command not found");
        if (health === "error") throw { code: -32000, message: "health blew up" };
        return {
          version: 1,
          kind: "bridge",
          state: "ready",
          in_state_ms: 1000,
          generation: 1,
          uptime_ms: 1000,
          restarts: {
            crashes_in_window: 0,
            breaker: { crashes: 0, window_ms: 60_000 },
            backoff_ms: 0,
            last_exit: null,
          },
          liveness: {
            last_pong_age_ms: 12,
            rtt_ms: 11,
            missed_pongs: 0,
            missed_pongs_to_kill: 3,
            loop_stale_ms: 0,
            transport: "ok",
            loop: "ok",
          },
          load: { tasks: 0, streams: 0, isolated: 0, queued: 0, stream_bytes_total: 0 },
          self: {
            window: "main",
            tasks: CELL,
            streams: CELL,
            queued: CELL,
            isolated: CELL,
          },
        };
      }
      return null;
    },
    async invokeChannel(cmd, args, onEvent) {
      calls.push({ cmd, args });
      const task = nextTask++;
      const { fnName, input } = args;
      if (fairness && fnName === "hold" && holdCalls >= 3) {
        // Over the owner's share ceiling: refused before a task exists —
        // the invoke itself rejects, the call never reaches a task id.
        throw { code: -32040, message: "over your share of task slots", data: { limit: "window_share", max: 3, window: "main" } };
      }
      later(() => {
        if (fnName === "log_stderr") {
          onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
        } else if (fnName === "fn_permissions_fixture") {
          // Present only when the resolved plugin supports per-function
          // permissions (CI appends it); absent means FUNCTION_NOT_FOUND.
          if (fixture) onEvent({ task, done: true, result: null });
          else onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
        } else if (fnName === "holds_active") {
          // The fairness fixture; absent means FUNCTION_NOT_FOUND.
          if (fairness) onEvent({ task, done: true, result: heldN });
          else onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
        } else if (fnName === "b_seen") {
          if (fairness) {
            // win-b's ping lands between main's polls: first read sees
            // 0, every later read sees >= 1.
            if (bSeen === 0) later(() => { bSeen = 1; });
            onEvent({ task, done: true, result: bSeen });
          } else {
            onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
          }
        } else if (fnName === "b_ping") {
          if (fairness) {
            bSeen += 1;
            onEvent({ task, done: true, result: bSeen });
          } else {
            onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
          }
        } else if (fnName === "hold") {
          holdCalls += 1;
          heldN += 1;
          // Held until cancelled: no terminal until the abort lands.
          cancelHooks.set(task, () =>
            later(() => {
              heldN -= 1;
              onEvent({ task, done: true, error: { code: -32010, message: "cancelled" } });
            }),
          );
        } else if (fnName === "add") {
          onEvent({ task, done: true, result: input.a + input.b });
        } else if (fnName === "count" && input.n === 25) {
          for (let i = 0; i < 25; i++) onEvent({ task, seq: i + 1, value: i, done: false });
          onEvent({ task, done: true, result: null });
        } else if (fnName === "count") {
          onEvent({ task, seq: 1, value: 0, done: false });
          cancelHooks.set(task, () =>
            later(() =>
              onEvent({ task, done: true, error: { code: -32010, message: "cancelled" } }),
            ),
          );
        }
      });
      return { task, protocol: 2 };
    },
    async listen(event, cb) {
      listens.push(event);
      if (event === "pylair://selftest-b")
        later(() =>
          cb({
            sawEvents: 0,
            everCancelled: false,
            anyGrantSucceeded: false,
            seenSample: [],
            sawStderr: 0,
            sawStderrAny: 0,
            ...(denyProbe ? { denyProbe } : {}),
            ...(fairness ? { fairnessProbe } : {}),
          }),
        );
      return () => {};
    },
  };
}

test("flag set, real dist: dispatches to the owner role and reports its verdict", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge();
  m.setBridge(b);
  setFlag(true);
  try {
    await m.maybeSelfTest();
  } finally {
    setFlag(false);
    m.setBridge(null);
  }
  const reports = b.calls.filter((c) => c.cmd === REPORT);
  assert.equal(reports.length, 1, "exactly one verdict");
  const { ok, checks } = reports[0].args.result;
  assert.deepEqual(Object.keys(checks), [
    "stderr-delivery",
    "stream-order",
    "call",
    "cancel",
    "window-fairness",
    "health",
    "event-scoping",
    "fn-permission",
  ]);
  assert.equal(ok, true, JSON.stringify(checks));
  assert.match(checks["stderr-delivery"], /^skipped: /);
  assert.match(checks["window-fairness"], /^skipped: /);
  assert.equal(checks["health"], "ok");
  assert.ok(b.listens.includes("pylair://health"), "onHealth subscribed on the health event");
  assert.equal(checks["fn-permission"], "ok");
});

/** Run the owner against a runtime whose win-b report carries `probe`. */
async function ownerVerdict(probe, fixture = true) {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge(probe, fixture);
  m.setBridge(b);
  setFlag(true);
  try {
    await m.maybeSelfTest();
  } finally {
    setFlag(false);
    m.setBridge(null);
  }
  return b.calls.find((c) => c.cmd === REPORT).args.result;
}

test("flag set: a win-b report with no deny probe fails the verdict", { timeout: 20_000 }, async () => {
  const { ok, checks } = await ownerVerdict(null);
  assert.equal(ok, false);
  assert.match(checks["fn-permission"], /^FAIL: .*no deny probe/);
});

test("flag set: no fixture means an older plugin: fn-permission is skipped, not failed", { timeout: 20_000 }, async () => {
  const through = { viaCall: { denied: false }, viaRaw: { denied: false } };
  const { ok, checks } = await ownerVerdict(through, false);
  assert.equal(ok, true, JSON.stringify(checks));
  assert.equal(
    checks["fn-permission"],
    "skipped: resolved plugin predates per-function permissions",
  );
});

test("flag set: win-b getting through to the denied function fails the verdict", { timeout: 20_000 }, async () => {
  const through = { denied: false };
  const { ok, checks } = await ownerVerdict({ viaCall: through, viaRaw: DENIED });
  assert.equal(ok, false);
  assert.match(checks["fn-permission"], /^FAIL: .*not denied/);
});

test("flag set: the window-fairness leg passes when the share refusal and b-probe land", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, { fairness: true });
  m.setBridge(b);
  setFlag(true);
  try {
    await m.maybeSelfTest();
  } finally {
    setFlag(false);
    m.setBridge(null);
  }
  const { ok, checks } = b.calls.find((c) => c.cmd === REPORT).args.result;
  assert.equal(checks["window-fairness"], "ok", JSON.stringify(checks));
  assert.equal(ok, true, JSON.stringify(checks));
});

test("flag set: a refused flood that names the global cap fails window-fairness", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  // win-b is live (b_seen answers) but the bridge still refuses with the
  // GLOBAL cap rather than window_share — the check must fail.
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, {
    fairness: true,
    fairnessProbe: { ok: true },
  });
  const orig = b.invokeChannel;
  b.invokeChannel = async (cmd, args, onEvent) => {
    if (args.fnName === "hold") {
      throw { code: -32040, message: "too many in-flight tasks", data: { limit: "max_concurrent_tasks", max: 64 } };
    }
    return orig(cmd, args, onEvent);
  };
  m.setBridge(b);
  setFlag(true);
  try {
    await m.maybeSelfTest();
  } finally {
    setFlag(false);
    m.setBridge(null);
  }
  const { ok, checks } = b.calls.find((c) => c.cmd === REPORT).args.result;
  assert.equal(ok, false);
  assert.match(checks["window-fairness"], /^FAIL: .*max_concurrent_tasks/);
});

test("flag set: a crate without python_health skips health, never fails", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, { health: "missing" });
  m.setBridge(b);
  setFlag(true);
  try {
    await m.maybeSelfTest();
  } finally {
    setFlag(false);
    m.setBridge(null);
  }
  const { ok, checks } = b.calls.find((c) => c.cmd === REPORT).args.result;
  assert.equal(
    checks["health"],
    "skipped: resolved plugin predates python_health",
    JSON.stringify(checks),
  );
  assert.ok(!b.listens.includes("pylair://health"), "no subscription on a missing command");
  assert.equal(ok, true, JSON.stringify(checks));
});

test("flag set: a coded python_health rejection fails, never skips", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, { health: "error" });
  m.setBridge(b);
  setFlag(true);
  try {
    await m.maybeSelfTest();
  } finally {
    setFlag(false);
    m.setBridge(null);
  }
  const { ok, checks } = b.calls.find((c) => c.cmd === REPORT).args.result;
  assert.equal(ok, false);
  assert.match(checks["health"], /^FAIL: /);
});
