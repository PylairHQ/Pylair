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

import { call, onStderr, PERMISSION_DENIED, PermissionDeniedError, PythonError, stream } from "./index.js";
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
  if ((await selfTestLabel()) === "win-b") return selfTestIntruder(spec);
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
 * The fn-permission check's overall verdict. `supported` says whether the
 * resolved plugin enforces per-function permissions (the CI fixture function
 * exists only then). Unsupported is a skip with its reason, never a failure;
 * supported stays mandatory, so a stale capability file or a plugin that
 * stopped enforcing still fails.
 */
export function judgeFnPermission(
  supported: boolean,
  probe: Partial<DenyProbe> | undefined,
  fn: string,
  mainCallOk: boolean,
): { ok: boolean; check: string } {
  if (!supported)
    return { ok: true, check: "skipped: resolved plugin predates per-function permissions" };
  const why =
    judgeDenyProbe(probe, fn) ?? (mainCallOk ? null : `main could not call ${fn} (the "call" check failed)`);
  return why ? { ok: false, check: `FAIL: ${why}` } : { ok: true, check: "ok" };
}

/** What win-b saw when it tried a function its capability denies. */
interface DenyOutcome {
  denied: boolean;
  code?: number;
  fn?: string;
  window?: string;
  error?: string;
}
interface DenyProbe {
  viaCall: DenyOutcome;
  viaRaw: DenyOutcome;
}

/**
 * The owner's verdict on win-b's deny probe: null when both call commands
 * answered PERMISSION_DENIED naming `fn` and win-b, otherwise why not.
 * A missing report is a failure, never a pass.
 */
export function judgeDenyProbe(probe: Partial<DenyProbe> | undefined, fn: string): string | null {
  if (!probe || !probe.viaCall || !probe.viaRaw)
    return "win-b reported no deny probe (is selftest-winb.json missing its deny entry?)";
  for (const [label, o] of [
    ["call() (python_call_stream)", probe.viaCall],
    ["python_call", probe.viaRaw],
  ] as const) {
    if (!o.denied)
      return `${label} was not denied for win-b (code ${String(o.code)}, ${o.error ?? "call succeeded"})`;
    if (o.code !== PERMISSION_DENIED)
      return `${label} was denied with code ${String(o.code)}, expected ${PERMISSION_DENIED}`;
    if (o.fn !== fn || o.window !== "win-b")
      return `${label} denial named fn=${String(o.fn)} window=${String(o.window)}, expected fn=${fn} window=win-b`;
  }
  return null;
}

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
    denyProbe?: DenyProbe;
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
  // Per-function permissions: win-b's capability denies `spec.add`. The
  // owner (main) just called it successfully (the "call" check above), so a
  // pass here means the denial is per window, not global. The fixture exists
  // only when CI found a plugin that enforces scopes (the same pattern as
  // log_stderr); without it the check is skipped with its reason. It runs
  // last so its task id cannot disturb the ids the intruder targets.
  {
    let supported = true;
    try {
      await within(call("fn_permissions_fixture", {}), 30_000, "fn_permissions_fixture");
    } catch (e) {
      if (e instanceof PythonError && e.code === -32601) supported = false;
      else {
        ok = false;
        checks["fn-permission"] = `FAIL: ${e instanceof Error ? e.message : String(e)}`;
        supported = true;
      }
    }
    if (checks["fn-permission"] === undefined) {
      const b = await within(bReport, 30_000, "win-b report").catch(() => undefined);
      const v = judgeFnPermission(supported, b?.denyProbe, spec.add, checks["call"] === "ok");
      checks["fn-permission"] = v.check;
      if (!v.ok) ok = false;
    }
  }
  await (await ipc()).invoke(CMD.selftestReport, { result: { ok, checks } });
}

/**
 * Intruder role ("win-b"): a hostile second window. Its capability grants
 * emit/emitTo on purpose, so a pass proves delivery — not the deny-emit
 * stopgap — protects the owner's stream. Reports findings back on
 * `pylair://selftest-b`; it cannot exit the app itself.
 */
async function selfTestIntruder(spec: SelfTestSpec): Promise<void> {
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

  // Per-function permissions: selftest-winb.json denies `spec.add` for this
  // window, on the set-level `pylair:default`. Try it through both call
  // commands. Denials consume no task id, so the owner's id assumptions hold.
  const outcome = (e: unknown): DenyOutcome => {
    if (e instanceof PermissionDeniedError)
      return { denied: true, code: e.code, fn: e.fn, window: e.window };
    const o = e as { code?: unknown; message?: unknown; data?: { fn?: unknown; window?: unknown } };
    return {
      denied: o?.code === PERMISSION_DENIED,
      code: typeof o?.code === "number" ? o.code : undefined,
      fn: typeof o?.data?.fn === "string" ? o.data.fn : undefined,
      window: typeof o?.data?.window === "string" ? o.data.window : undefined,
      error: typeof o?.message === "string" ? o.message : String(e),
    };
  };
  const viaCall = await call(spec.add, { a: 1, b: 2 }).then(
    () => ({ denied: false }) as DenyOutcome,
    outcome,
  );
  const viaRaw = await invoke("plugin:pylair|python_call", {
    fnName: spec.add,
    input: { a: 1, b: 2 },
  }).then(() => ({ denied: false }) as DenyOutcome, outcome);
  const denyProbe: DenyProbe = { viaCall, viaRaw };

  // The report cannot ride emit/emitTo: a `core:event:deny-emit(-to)` entry
  // (as the default template ships) denies those commands origin-wide —
  // Tauri ignores window scoping for denials — so win-b relays through an
  // ACL-checked command whose Rust side emits for it.
  await invoke("plugin:pylair|python_selftest_b_report", {
    report: { sawEvents, everCancelled, anyGrantSucceeded, seenSample, sawStderr, sawStderrAny, denyProbe },
  }).catch(() => {});
}
