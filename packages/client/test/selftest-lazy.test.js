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

function runtimeBridge(denyProbe = { viaCall: DENIED, viaRaw: DENIED }) {
  const calls = [];
  const cancelHooks = new Map();
  let nextTask = 1;
  const later = (f) => setTimeout(f, 0);
  return {
    calls,
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === "plugin:pylair|python_cancel") {
        cancelHooks.get(args.task)?.();
        return "cancelled";
      }
      return null;
    },
    async invokeChannel(cmd, args, onEvent) {
      calls.push({ cmd, args });
      const task = nextTask++;
      const { fnName, input } = args;
      later(() => {
        if (fnName === "log_stderr") {
          onEvent({ task, done: true, error: { code: -32601, message: "no such function" } });
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
    "event-scoping",
    "fn-permission",
  ]);
  assert.equal(ok, true, JSON.stringify(checks));
  assert.match(checks["stderr-delivery"], /^skipped: /);
  assert.equal(checks["fn-permission"], "ok");
});

/** Run the owner against a runtime whose win-b report carries `probe`. */
async function ownerVerdict(probe) {
  const m = await import(pathToFileURL(join(dist, "index.js")).href);
  const b = runtimeBridge(probe);
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

test("flag set: win-b getting through to the denied function fails the verdict", { timeout: 20_000 }, async () => {
  const through = { denied: false };
  const { ok, checks } = await ownerVerdict({ viaCall: through, viaRaw: DENIED });
  assert.equal(ok, false);
  assert.match(checks["fn-permission"], /^FAIL: .*not denied/);
});
