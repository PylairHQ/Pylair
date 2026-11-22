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
  // App readiness (decisions 0007) needs BOTH sides. "ok" (default):
  // the fixture answers and health carries the app block. "missing":
  // an older pylair package — selftest_ping is FUNCTION_NOT_FOUND.
  // "hostless": the package fixture answers but a pre-rc.20 crate
  // reports no app block at all.
  const { appReadiness = "ok" } = opts;
  // "busyHealth": the first N python_health reads report one straggler
  // task still draining — the pre-flood quiet gate must keep polling
  // until load.tasks reaches 0 before any `hold` goes out.
  const { busyHealth = 0 } = opts;
  const calls = [];
  const listens = [];
  const cancelHooks = new Map();
  let nextTask = 1;
  let heldN = 0;
  let holdCalls = 0;
  let bSeen = 0;
  let healthCalls = 0;
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
        healthCalls += 1;
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
          load: {
            tasks: healthCalls <= busyHealth ? 1 : heldN,
            streams: 0,
            isolated: 0,
            queued: 0,
            stream_bytes_total: 0,
          },
          self: {
            window: "main",
            tasks: CELL,
            streams: CELL,
            queued: CELL,
            isolated: CELL,
          },
          ...(appReadiness === "ok"
            ? { app: { components: { selftest: { phase: "ready", progress: null, detail: null, ms_ago: 5 } } } }
            : {}),
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
        } else if (fnName === "bench_noop") {
          onEvent({ task, done: true, result: 1 });
        } else if (fnName === "bench_echo") {
          onEvent({ task, done: true, result: input.s });
        } else if (fnName === "bench_nested") {
          onEvent({ task, done: true, result: 6 });
        } else if (fnName === "selftest_ping") {
          // The app-readiness fixture; absent means FUNCTION_NOT_FOUND
          // (a resolved pylair that predates the readiness API).
          if (appReadiness === "missing")
            onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
          else onEvent({ task, done: true, result: "pong" });
        } else if (fnName === "selftest_done") {
          onEvent({ task, done: true, result: "done" });
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
      if (event === "pylair://app" && appReadiness === "ok")
        // selftest_ping re-reports a loading→ready cycle; a capable
        // host turns it into a pylair://app phase event.
        later(() => cb({ version: 1, generation: 1, name: "selftest", phase: "ready" }));
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
    "app-readiness",
    "event-scoping",
    "fn-permission",
  ]);
  assert.equal(ok, true, JSON.stringify(checks));
  assert.match(checks["stderr-delivery"], /^skipped: /);
  assert.match(checks["window-fairness"], /^skipped: /);
  assert.equal(checks["health"], "ok");
  assert.equal(checks["app-readiness"], "ok");
  assert.ok(b.listens.includes("pylair://health"), "onHealth subscribed on the health event");
  assert.ok(b.listens.includes("pylair://app"), "onApp subscribed on the app event");
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
  // The refusal names the wrong limit because SOMETHING held a global
  // slot — the report must say what was in flight.
  assert.match(checks["window-fairness"], /in flight: liveTasks=/);
  assert.match(checks["window-fairness"], /load=/);
});

test("flag set: the flood waits for a quiet runtime before refusing on share", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  // Two early health reads still show a straggler draining; the gate
  // must keep polling until load.tasks reaches 0 before any `hold`.
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, {
    fairness: true,
    busyHealth: 2,
  });
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
  const firstHold = b.calls.findIndex(
    (c) => c.cmd === "plugin:pylair|python_call_stream" && c.args?.fnName === "hold",
  );
  assert.ok(firstHold > 0, "a hold call was attempted");
  const healthBefore = b.calls
    .slice(0, firstHold)
    .filter((c) => c.cmd === "plugin:pylair|python_health").length;
  assert.ok(
    healthBefore >= 3,
    `quiet gate polled health ${healthBefore}x before flooding, expected >=3`,
  );
});

test("flag set, bench mode: times the four payloads and reports percentiles", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge();
  m.setBridge(b);
  globalThis.__PYLAIR_SELF_TEST__ = true;
  globalThis.__PYLAIR_SELF_TEST_BENCH__ = true;
  globalThis.__PYLAIR_SELF_TEST_BENCH_N__ = 8;
  globalThis.__PYLAIR_SELF_TEST_BENCH_WARMUP__ = 2;
  try {
    await m.maybeSelfTest();
  } finally {
    delete globalThis.__PYLAIR_SELF_TEST__;
    delete globalThis.__PYLAIR_SELF_TEST_BENCH__;
    delete globalThis.__PYLAIR_SELF_TEST_BENCH_N__;
    delete globalThis.__PYLAIR_SELF_TEST_BENCH_WARMUP__;
    m.setBridge(null);
  }
  const report = b.calls.find((c) => c.cmd === "plugin:pylair|python_bench_report");
  assert.ok(report, "python_bench_report invoked");
  const { result } = report.args;
  assert.equal(result.n, 8);
  assert.equal(result.warmup, 2);
  for (const k of ["call_noop", "call_64k", "call_nested", "stream_first_chunk"]) {
    assert.deepEqual(Object.keys(result[k]), ["p50", "p95", "p99"], `${k} percentiles`);
    assert.ok(result[k].p50 >= 0 && result[k].p99 >= result[k].p50, `${k} sane percentiles`);
  }
  // The bench replaces the checks — no selftest verdict is emitted.
  assert.equal(b.calls.filter((c) => c.cmd === REPORT).length, 0);
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

test("flag set: a package without the readiness fixture skips app-readiness", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, { appReadiness: "missing" });
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
    checks["app-readiness"],
    "skipped: resolved host or package predates app readiness",
    JSON.stringify(checks),
  );
  assert.equal(ok, true, JSON.stringify(checks));
});

test("flag set: a host without the app block skips app-readiness", { timeout: 20_000 }, async () => {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  // The fixture answers (new pylair) but a pre-rc.20 crate reports no
  // app block — the selftest skips on the missing snapshot field.
  const b = runtimeBridge({ viaCall: DENIED, viaRaw: DENIED }, true, { appReadiness: "hostless" });
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
    checks["app-readiness"],
    "skipped: resolved host or package predates app readiness",
    JSON.stringify(checks),
  );
  assert.equal(ok, true, JSON.stringify(checks));
});
