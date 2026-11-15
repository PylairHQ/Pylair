/**
 * The installed-webview self-test. `maybeSelfTest` and `SelfTestSpec` are
 * public API, re-exported from the package root (index.ts); the rest of
 * this module is private to it.
 *
 * Circular import: index.ts re-exports from this module and this module
 * imports from index.ts. That is safe because nothing here touches an
 * import at module load: every use of `call`, `stream`, `onStderr`,
 * `PythonError`, `ipc` and `CMD` is inside a function body.
 */

import { call, onStderr, PythonError, stream } from "./index.js";
import { CMD, ipc } from "./internal.js";

// ---- installed-webview self-test --------------------------------------------

export interface SelfTestSpec {
  /** Function returning a number: add(a, b). */
  add: string;
  /** Async generator yielding 0..n-1 slowly. */
  count: string;
}

/**
 * When the host runs the app with PYLAIR_SELF_TEST_WEBVIEW, the plugin sets
 * `window.__PYLAIR_SELF_TEST__`. Call this once at startup: it drives the
 * REAL bridge (ACL permissions, event routing, wire transform) — a typed
 * call, an ordered stream, cancellation, shutdown — then reports the verdict
 * so the host process exits 0/5. A no-op in normal runs.
 */
export async function maybeSelfTest(
  spec: SelfTestSpec = { add: "add", count: "count" },
): Promise<void> {
  if (!(globalThis as { __PYLAIR_SELF_TEST__?: boolean }).__PYLAIR_SELF_TEST__) return;
  // Under the flag the plugin also opens a second webview, "win-b". It
  // runs the intruder role; every other label runs the owner role.
  if ((await selfTestLabel()) === "win-b") return selfTestIntruder();
  return selfTestOwner(spec);
}

async function selfTestLabel(): Promise<string> {
  try {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    return getCurrentWebview().label;
  } catch {
    return "main"; // headless/test hosts have no webview API
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Owner role ("main"): run a real stream and a plain call to completion,
 * then gate on the intruder's report. Any leaked chunk, forged item or
 * successful cross-window cancel/grant fails the verdict.
 */
async function selfTestOwner(spec: SelfTestSpec): Promise<void> {
  const checks: Record<string, string> = {};
  let ok = true;
  const run = async (name: string, f: () => Promise<void>) => {
    try {
      await f();
      checks[name] = "ok";
    } catch (e) {
      ok = false;
      checks[name] = `FAIL: ${e instanceof Error ? e.message : String(e)}`;
    }
  };
  const within = <T,>(p: Promise<T>, ms: number, what: string) =>
    Promise.race([
      p,
      new Promise<T>((_, rej) => setTimeout(() => rej(new Error(`${what} timed out`)), ms)),
    ]);

  // The intruder's verdict must be captured before it can possibly send —
  // the listener installs before any task runs.
  const bReport = new Promise<{
    sawEvents?: number;
    everCancelled?: boolean;
    anyGrantSucceeded?: boolean;
    seenSample?: unknown[];
    sawStderr?: number;
    sawStderrAny?: number;
  }>((res) => {
    void ipc().then((b) => b.listen("pylair://selftest-b", res));
  });

  // The owner proves it still receives runtime stderr on its own window,
  // while the intruder proves it receives none. log_stderr fires first so
  // the line lands inside win-b's whole-lifetime listening window.
  const ownStderr: string[] = [];
  void onStderr((l) => ownStderr.push(l));
  const stderrMarker = `SELFTEST-STDERR-${Date.now()}`;
  // The fixture exists only when CI appended it, which it does only when
  // the resolved crate carries stderr scoping. An older resolved plugin
  // would broadcast the marker to win-b — that run must skip the probe,
  // not fail for the absence of the feature under test.
  let stderrProbeRan = false;
  await run("stderr-delivery", async () => {
    try {
      await within(call("log_stderr", { msg: stderrMarker }), 30_000, "log_stderr");
    } catch (e) {
      if (e instanceof PythonError && e.code === -32601) return;
      throw e;
    }
    stderrProbeRan = true;
    for (let i = 0; i < 50 && !ownStderr.some((l) => l.includes(stderrMarker)); i++) {
      await sleep(100);
    }
    if (!ownStderr.some((l) => l.includes(stderrMarker)))
      throw new Error("main window did not receive its own stderr line");
  });
  if (!stderrProbeRan && checks["stderr-delivery"] === "ok")
    checks["stderr-delivery"] =
      "skipped: backend has no log_stderr (resolved plugin predates stderr scoping)";

  // Task ids are sequential from 1 on a fresh app: the stderr probe = 1,
  // stream = 2, call = 3, cancel-check = 4. The intruder targets 2 and 3.
  await run("stream-order", async () => {
    const got: number[] = [];
    await within(
      (async () => {
        for await (const n of stream<number>(spec.count, { n: 25 })) got.push(n);
      })(),
      30_000,
      "stream",
    );
    if (got.join(",") !== "0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24")
      throw new Error(`stream gave [${got.join(",")}]`);
  });
  await run("call", async () => {
    const v = await within(call<number>(spec.add, { a: 2, b: 3 }), 30_000, "call");
    if (v !== 5) throw new Error(`add(2,3) returned ${String(v)}`);
  });
  await run("cancel", async () => {
    const ac = new AbortController();
    let seen = 0;
    let code: number | undefined;
    await within(
      (async () => {
        try {
          for await (const _ of stream<number>(spec.count, { n: 1000 }, { signal: ac.signal })) {
            if (++seen === 1) ac.abort();
          }
        } catch (e) {
          if (e instanceof PythonError) code = e.code;
          else throw e;
        }
      })(),
      30_000,
      "cancel",
    );
    if (code !== -32010) throw new Error(`expected TASK_CANCELLED, got ${String(code)} after ${seen} chunks`);
    if (seen >= 100) throw new Error(`producer kept running (${seen} chunks)`);
  });
  await run("event-scoping", async () => {
    const b = await within(bReport, 30_000, "win-b report");
    if ((b.sawEvents ?? 0) !== 0)
      throw new Error(
        `win-b read ${String(b.sawEvents)} of win-a's stream events: ${JSON.stringify(b.seenSample ?? [])}`,
      );
    if (b.everCancelled) throw new Error("win-b cancelled win-a's task");
    if (b.anyGrantSucceeded) throw new Error("win-b granted credit on win-a's task");
    if (typeof b.sawStderr !== "number") throw new Error("win-b reported no stderr count");
    // Meaningful only when the probe ran: a resolved plugin that predates
    // stderr scoping broadcasts by design, so win-b's count is reported
    // for visibility, not gated.
    if (stderrProbeRan && b.sawStderr !== 0)
      throw new Error(`win-b received ${b.sawStderr} stderr line(s) not addressed to it`);
  });
  await (await ipc()).invoke(CMD.selftestReport, { result: { ok, checks } });
}

/**
 * Intruder role ("win-b"): a hostile second window. Its capability grants
 * emit/emitTo on purpose, so a pass proves delivery — not the deny-emit
 * stopgap — protects the owner's stream. Reports findings back on
 * `pylair://selftest-b`; it cannot exit the app itself.
 */
async function selfTestIntruder(): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  const { emit, emitTo, listen } = await import("@tauri-apps/api/event");

  // Passive receipt: a default-target listener — what a stock webview's
  // client does — must see nothing of the owner's call data. Tauri's
  // Any-target listener ALSO echoes this window's own forgeries back —
  // count only events lacking the FORGED markers emitted below.
  const forged = new Set([
    "FORGED-BY-B",
    "FORGED-EMITTO-BY-B",
    "FORGED-EARLY-BY-B",
    "FORGED-RESULT-BY-B",
  ]);
  let sawEvents = 0;
  const seenSample: unknown[] = [];
  await listen<{ value?: unknown; result?: unknown }>("pylair://stream", (e) => {
    if (seenSample.length < 6) seenSample.push(e.payload);
    if (forged.has(String(e.payload.value)) || forged.has(String(e.payload.result))) return;
    sawEvents += 1;
  });

  // Stderr scoping, two probes. The webview-scoped listener mirrors what a
  // stock @pylair/client window gets — must stay zero under per-window
  // opt-in. The Any-target listener documents the event-bus residual:
  // Any handlers bypass emit_to filtering by design (match_any_or_filter),
  // so a deliberate sniffer still sees the lines — reported, not asserted.
  let sawStderr = 0;
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  await getCurrentWebview().listen<string>("pylair://stderr", () => {
    sawStderr += 1;
  });
  let sawStderrAny = 0;
  await listen<string>("pylair://stderr", () => {
    sawStderrAny += 1;
  });

  // Early-buffer attack: pre-fire a terminal for the owner's NEXT task id
  // (task 3 — the plain call) before it exists.
  void emit("pylair://stream", { task: 3, done: true, result: "FORGED-EARLY-BY-B" }).catch(() => {});

  // Wait until the runtime is up so task 2 (the owner's stream) is live.
  for (let i = 0; i < 120; i++) {
    const s = await invoke<string>("plugin:pylair|python_state").catch(() => "");
    if (s === "ready") break;
    await sleep(500);
  }

  // Mid-stream forgeries at task 2: plain emit, emitTo("main"), terminal.
  void emit("pylair://stream", { task: 2, seq: 999, value: "FORGED-BY-B", done: false }).catch(
    () => {},
  );
  void emitTo("main", "pylair://stream", {
    task: 2,
    seq: 998,
    value: "FORGED-EMITTO-BY-B",
    done: false,
  }).catch(() => {});
  setTimeout(() => {
    void emit("pylair://stream", { task: 2, done: true, result: "FORGED-RESULT-BY-B" }).catch(
      () => {},
    );
  }, 400);

  // Task control: cancel/grant on the owner's task must never succeed —
  // probe repeatedly across the stream's live window.
  let everCancelled = false;
  let anyGrantSucceeded = false;
  const acted = new Set(["cancelled", "terminated", "requested_may_continue", "termination_uncertain"]);
  for (let i = 0; i < 8; i++) {
    const oc = await invoke<string>("plugin:pylair|python_cancel", { task: 2 }).catch(() => "");
    if (acted.has(oc)) everCancelled = true;
    const granted = await invoke("plugin:pylair|python_grant_credit", { task: 2, n: 8 })
      .then(() => true)
      .catch(() => false);
    if (granted) anyGrantSucceeded = true;
    await sleep(400);
  }

  // The report cannot ride emit/emitTo: a `core:event:deny-emit(-to)` entry
  // (as the default template ships) denies those commands origin-wide —
  // Tauri ignores window scoping for denials — so win-b relays through an
  // ACL-checked command whose Rust side emits for it.
  await invoke("plugin:pylair|python_selftest_b_report", {
    report: { sawEvents, everCancelled, anyGrantSucceeded, seenSample, sawStderr, sawStderrAny },
  }).catch(() => {});
}
