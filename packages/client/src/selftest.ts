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

import { call, cancel, health, LimitExceededError, onApp, onHealth, onStderr, PERMISSION_DENIED, PermissionDeniedError, PythonError, stream } from "./index.js";
import { CMD, ipc, liveTasks } from "./internal.js";

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
  // The latency bench replaces the checks entirely (docs/performance.md):
  // PYLAIR_SELF_TEST_BENCH injects both flags and spawns no second window.
  if (
    (globalThis as { __PYLAIR_SELF_TEST_BENCH__?: boolean }).__PYLAIR_SELF_TEST_BENCH__
  )
    return selfTestBench(spec);
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
    fairnessProbe?: { ok?: boolean; skipped?: string; error?: string; value?: unknown };
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
  // Per-window fairness (docs/decisions/0005): with main and win-b both
  // live, main floods `hold` calls until the host refuses — and the
  // refusal must be main's SHARE (window_share naming "main"), never the
  // global cap. win-b's own call, made while main is held, must still be
  // admitted: that is the whole point of the share. The held calls are
  // then cancelled and the owner's count drains. This is the only check
  // that proves a real webview subscribes through WindowRegistry and gets
  // its share — the Rust-side tests drive the counters through hooks.
  // The fixture lives in the backend (hold/holds_active): its absence
  // means the resolved app predates it — a skip, never a failure.
  let fairnessArmed = true;
  await run("window-fairness", async () => {
    try {
      await within(call<number>("holds_active", {}), 15_000, "holds_active");
    } catch (e) {
      if (e instanceof PythonError && e.code === -32601) {
        fairnessArmed = false;
        return;
      }
      throw e;
    }
    // win-b must be LIVE before the flood — a lone main would be refused
    // on the global cap, never window_share. `b_seen` is the proof: the
    // intruder's `b_ping` call is itself the bridge command that
    // subscribes it, so a nonzero count means win-b already holds a live
    // entry. Poll Python state, not an event — an emit can arrive before
    // this window's listener exists and is gone forever.
    for (let i = 0; ; i++) {
      const seen = await within(call<number>("b_seen", {}), 15_000, "b_seen");
      if (seen > 0) break;
      if (i === 119) throw new Error("win-b never pinged (b_seen stayed 0)");
      await sleep(500);
    }
    // Quiet-runtime gate: `b_seen` proves win-b joined the live set, but
    // its `b_ping` response — or a straggler from an earlier check — can
    // still hold a global slot while it drains. The host evaluates
    // `max_concurrent_tasks` BEFORE the share check, so one stray
    // in-flight call turns the expected window_share refusal into the
    // global cap's. health() is a host command — polling it adds no
    // task; a host predating python_health falls back to the hold
    // counter.
    for (let i = 0; ; i++) {
      let quiet: boolean;
      try {
        quiet = (await within(health(), 15_000, "health")).load.tasks === 0;
      } catch {
        quiet =
          (await within(call<number>("holds_active", {}), 15_000, "holds_active")) === 0;
      }
      if (quiet) break;
      if (i === 199)
        throw new Error(
          `runtime still busy before the flood (liveTasks=${JSON.stringify([...liveTasks])})`,
        );
      await sleep(150);
    }
    const ac = new AbortController();
    const held: Promise<unknown>[] = [];
    try {
      // Flood `hold` calls, pacing each on liveTasks growth: an admitted
      // call lands in liveTasks when its task id arrives, a refused one
      // rejects during the invoke and is never tracked. Waiting for one
      // or the other keeps every call inside in_flight before the next
      // goes, so the refusal comes from the TASK share — a burst would
      // hit the queue share instead.
      let refused: LimitExceededError | undefined;
      for (let i = 0; i < 80 && !refused; i++) {
        const before = liveTasks.size;
        const p = call("hold", { seconds: 30 }, { signal: ac.signal });
        held.push(p);
        p.catch(() => {});
        const outcome = await Promise.race([
          p.then(
            () => "answered" as const,
            (e: unknown) => e,
          ),
          (async () => {
            for (let j = 0; j < 600 && liveTasks.size <= before; j++) await sleep(50);
            return liveTasks.size > before ? ("registered" as const) : ("stall" as const);
          })(),
        ]);
        if (outcome instanceof LimitExceededError) refused = outcome;
        else if (outcome instanceof Error) throw outcome;
        else if (outcome === "stall")
          throw new Error("a hold call neither registered nor was refused");
        else if (outcome !== "answered" && outcome !== "registered")
          throw new Error(`hold call threw ${String(outcome)}`);
      }
      if (!refused) throw new Error("80 held calls and still no refusal");
      if (refused.limit !== "window_share") {
        // The global cap shadows the share check, so report which calls
        // were in flight — that's what made this refusal the wrong one.
        let inflight = `liveTasks=${JSON.stringify([...liveTasks])}`;
        try {
          const h = await health();
          inflight += ` load=${JSON.stringify(h.load)} self=${JSON.stringify(h.self)}`;
        } catch {
          // host predates python_health — the client-side set is all we have
        }
        throw new Error(
          `flood refused on limit "${refused.limit}", expected window_share ` +
            `(data=${JSON.stringify(refused.data ?? null)}; in flight: ${inflight})`,
        );
      }
      const d = refused.data as { window?: unknown; max?: unknown } | undefined;
      if (d?.window !== "main")
        throw new Error(`window_share named window ${String(d?.window)}, expected "main"`);
      if (typeof d?.max !== "number")
        throw new Error("window_share refusal carried no share ceiling");
      // win-b's probe ran while main held (the intruder gates its report
      // on it): its call must have been admitted.
      const b = await within(bReport, 60_000, "win-b report");
      if (!b.fairnessProbe) throw new Error("win-b reported no fairness probe");
      if (b.fairnessProbe.skipped)
        throw new Error(`win-b skipped its probe: ${b.fairnessProbe.skipped}`);
      if (!b.fairnessProbe.ok)
        throw new Error(
          `win-b's call failed while main was held: ${b.fairnessProbe.error ?? "no detail"}`,
        );
    } finally {
      ac.abort();
      await Promise.allSettled(held);
    }
    // Counts drain: the runtime-side hold counter returns to 0 and a
    // fresh call is admitted — released slots are claimable again.
    for (let i = 0; i < 100; i++) {
      if ((await within(call<number>("holds_active", {}), 15_000, "holds_active")) === 0) break;
      if (i === 99) throw new Error("held calls never drained (holds_active stayed >0)");
      await sleep(100);
    }
    const v = await within(call<number>(spec.add, { a: 2, b: 2 }), 15_000, "post-drain call");
    if (v !== 4) throw new Error(`post-drain add returned ${String(v)}`);
  });
  if (!fairnessArmed && checks["window-fairness"] === "ok")
    checks["window-fairness"] = "skipped: backend has no hold/holds_active fixture";
  // Runtime health (docs/decisions/0006): python_health is a host
  // command, not a Python fixture — the resolved crate carries it from
  // rc.19 on, so the invoke itself is the probe. An old crate rejects
  // it before the plugin ever runs (Tauri: "not allowed. Command not
  // found" — a plain string), which is a clean skip on pack/registry
  // legs and a failure on --local ones (CI greps this verdict). A
  // {code:number} rejection is the plugin's own ErrorObj answer —
  // always a real failure.
  let healthArmed = true;
  await run("health", async () => {
    const snap = await within(
      health().catch((e: unknown) => {
        const o = e as { code?: unknown } | null;
        if (o && typeof o === "object" && typeof o.code === "number") throw e;
        healthArmed = false;
        return null;
      }),
      15_000,
      "health",
    );
    if (snap === null) return;
    if (snap.version !== 1)
      throw new Error(`health version ${JSON.stringify(snap.version)}, expected 1`);
    if (snap.state !== "ready") throw new Error(`state "${snap.state}", expected "ready"`);
    if (snap.liveness.transport !== "ok")
      throw new Error(`liveness.transport "${snap.liveness.transport}", expected "ok"`);
    if (typeof snap.load?.tasks !== "number")
      throw new Error("health snapshot carries no load block");
    // `self` is the calling window's own row — "main" here; null only
    // when the deployment runs without per-window fairness (§4).
    if (snap.self !== null && snap.self.window !== "main")
      throw new Error(
        `health self named window ${JSON.stringify(snap.self.window)}, expected "main"`,
      );
    // The event half: subscribe + unlisten through the real bridge must
    // complete cleanly. A steady-state runtime emits nothing (events
    // fire on verdict transitions only), so no payload is observable
    // here — shape is covered by the Rust runtime_health suite.
    const unlisten = await within(onHealth(() => {}), 15_000, "onHealth subscribe");
    await unlisten();
  });
  if (!healthArmed && checks["health"] === "ok")
    checks["health"] = "skipped: resolved plugin predates python_health";
  // App readiness (docs/decisions/0007): needs BOTH sides — the CI
  // fixture (pylair >= 0.1.0rc12 carries on_startup/loading/ready and
  // @expose(requires=)) and the host (crate >= 0.1.0-rc.20 carries
  // health().app and the pylair://app event). A missing fixture is a
  // -32601 from selftest_ping; a fixture that answers on a host that
  // never reports an app block means the crate predates
  // app.component — both are clean skips on pack/registry legs and
  // failures wherever CI armed PYLAIR_APP_READINESS_EXPECTED
  // (ci.yml --local legs arm it implicitly by grep).
  let appReadinessArmed = true;
  await run("app-readiness", async () => {
    // Subscribe BEFORE the calls: startup's transitions fired before
    // this listener exists, so the fixture splits the re-report into
    // two calls — selftest_ping reports `loading`, selftest_done
    // reports `ready`. Each is a real phase transition (a single
    // loading+ready inside one call would coalesce to a no-op on the
    // wire), so a post-startup subscriber observes pylair://app events
    // deterministically.
    const phases: string[] = [];
    const unlisten = await within(onApp((c) => phases.push(c.phase)), 15_000, "onApp subscribe");
    try {
      const pong = await within(
        call<string>("selftest_ping").catch((e: unknown) => {
          if (e instanceof PythonError && e.code === -32601) {
            appReadinessArmed = false;
            return null;
          }
          throw e;
        }),
        30_000,
        "selftest_ping",
      );
      if (pong === null) return;
      // requires=("selftest",) let the call through only after the
      // component went ready; the answer itself is the gate proof.
      if (pong !== "pong")
        throw new Error(`selftest_ping answered ${JSON.stringify(pong)}, expected "pong"`);
      // Restore the component to ready — its transition is the second
      // observed event, and health().app must read it below.
      const done = await within(call<string>("selftest_done"), 30_000, "selftest_done");
      if (done !== "done")
        throw new Error(`selftest_done answered ${JSON.stringify(done)}, expected "done"`);
      // health().app must name the component ready — bounded poll, no
      // fixed sleep: a host that predates app readiness keeps app
      // null/undefined on every read, so the first read exits.
      let sawApp = false;
      let phase: string | null = null;
      const deadline = Date.now() + 15_000;
      do {
        const snap = await within(health().catch(() => null), 15_000, "health").catch(
          () => null,
        );
        if (!snap?.app) break;
        sawApp = true;
        phase = snap.app.components["selftest"]?.phase ?? null;
        if (phase === "ready") break;
        await sleep(100);
      } while (Date.now() < deadline);
      if (!sawApp) {
        appReadinessArmed = false;
        return;
      }
      if (phase !== "ready")
        throw new Error(
          `health().app selftest phase ${JSON.stringify(phase)}, expected "ready"`,
        );
      // The ping/done transitions must have emitted at least one
      // pylair://app phase event through this subscription — bounded
      // wait (the event bus can lag the call responses).
      const evDeadline = Date.now() + 10_000;
      while (phases.length === 0 && Date.now() < evDeadline) await sleep(50);
      if (phases.length === 0)
        throw new Error("onApp delivered no phase event for the ping/done transitions");
    } finally {
      // Unlisten through the real bridge — part of the check.
      await unlisten();
    }
  });
  if (!appReadinessArmed && checks["app-readiness"] === "ok")
    checks["app-readiness"] =
      "skipped: resolved host or package predates app readiness";
  // Warm isolated workers (docs/decisions/0008): the fixture reports its
  // member's pid and platform. Two calls must share a member; a cancelled
  // sleep then diverges per platform — POSIX interrupts the call and the
  // SAME member serves the next call (plain "cancelled" ack), Windows is
  // kill-only ("terminated" ack, a respawned member = new pid). Needs
  // pylair >= 0.1.0rc13 — a missing fixture is -32601, a clean skip on
  // pack/registry legs and a failure wherever CI armed
  // PYLAIR_WARM_EXPECTED (--local legs arm it implicitly by grep).
  let warmArmed = true;
  await run("warm-workers", async () => {
    const witness = async () =>
      within(
        call<{ pid: number; platform: string }>("warm_selftest", { seconds: 0 }),
        30_000,
        "warm_selftest",
      );
    let w1: { pid: number; platform: string };
    try {
      w1 = await witness();
    } catch (e) {
      if (e instanceof PythonError && e.code === -32601) {
        warmArmed = false;
        return;
      }
      throw e;
    }
    const w2 = await witness();
    if (w2.pid !== w1.pid)
      throw new Error(`warm pool respawned between calls (pid ${w1.pid} -> ${w2.pid})`);
    // The sleeping call runs through the raw channel so the task id is
    // known for an explicit cancel() — the CancelOutcome it acknowledges
    // is what this check asserts per platform.
    const b = await ipc();
    let termCode: number | undefined;
    let termResolve!: () => void;
    const terminal = new Promise<void>((res) => (termResolve = res));
    const ack = (await b.invokeChannel(CMD.callStream, {
      fnName: "warm_selftest",
      input: { seconds: 30 },
      stream: false,
    }, (ev: { done?: boolean; error?: { code: number } }) => {
      if (!ev.done) return;
      termCode = ev.error ? ev.error.code : 0;
      termResolve();
    })) as { task?: number };
    if (typeof ack.task !== "number")
      throw new Error(`python_call_stream ack carried no task id: ${JSON.stringify(ack)}`);
    const outcome = await within(cancel(ack.task), 30_000, "cancel");
    await within(terminal, 30_000, "cancelled call's terminal");
    if (termCode !== -32010)
      throw new Error(`cancelled warm call ended with code ${String(termCode)}, expected -32010`);
    const w3 = await witness();
    if (w1.platform === "win32") {
      if (outcome !== "terminated")
        throw new Error(`Windows kill-only expected a "terminated" ack, got ${outcome}`);
      if (w3.pid === w1.pid)
        throw new Error("a member the cancel killed was reused for the next call");
    } else {
      if (outcome !== "cancelled")
        throw new Error(`POSIX interrupt-first expected a plain "cancelled" ack, got ${outcome}`);
      if (w3.pid !== w1.pid)
        throw new Error(
          `member did not survive the interrupt (pid ${w1.pid} -> ${w3.pid})`,
        );
    }
  });
  if (!warmArmed && checks["warm-workers"] === "ok")
    checks["warm-workers"] =
      "skipped: backend has no warm_selftest fixture (resolved pylair predates warm workers)";
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

// ---- latency bench ----------------------------------------------------------
// docs/performance.md. Opt-in only (PYLAIR_SELF_TEST_BENCH injects
// __PYLAIR_SELF_TEST_BENCH__); runs instead of the checks, on the main
// window alone. The same four payloads are timed host-side in
// crates/tauri-plugin-pylair/tests/bench_latency.rs and dispatch-side in
// python/pylair/tests/bench_dispatch.py so the layer shares can be
// subtracted. Reported as one `webview-bench:` JSON line by the host.

const BENCH_64K = "x".repeat(64 * 1024);
const BENCH_NESTED = {
  shape: {
    name: "tri",
    origin: { x: 0, y: 0, label: "o" },
    points: [
      { x: 1, y: 2, label: "a" },
      { x: 3, y: 4, label: "b" },
      { x: 5, y: 6, label: "c" },
    ],
    meta: { n: 3 },
  },
};

const benchQuantile = (sorted: number[], p: number): number =>
  sorted[Math.min(sorted.length - 1, Math.ceil(p * sorted.length) - 1)];

async function selfTestBench(spec: SelfTestSpec): Promise<void> {
  const flags = globalThis as {
    __PYLAIR_SELF_TEST_BENCH_N__?: number;
    __PYLAIR_SELF_TEST_BENCH_WARMUP__?: number;
  };
  // Test hooks — the bench harness in CI always sees the defaults.
  const n = flags.__PYLAIR_SELF_TEST_BENCH_N__ ?? 500;
  const warmup = flags.__PYLAIR_SELF_TEST_BENCH_WARMUP__ ?? 50;

  // Wait for the runtime to be ready — the same bounded poll as the
  // health check, except a host predating python_health proceeds on
  // the first failed read (calls queue behind Ready anyway).
  for (let i = 0; i < 300; i++) {
    const snap = await health().catch(() => null);
    if (snap === null || snap.state === "ready") break;
    await sleep(100);
  }

  const streamFirstChunk = async (): Promise<void> => {
    const it = stream<number>(spec.count, { n: 3 })[Symbol.asyncIterator]();
    try {
      const first = await it.next();
      if (first.done) throw new Error("stream closed without a chunk");
    } finally {
      await it.return?.();
    }
  };
  const cases: [string, () => Promise<unknown>][] = [
    ["call_noop", () => call("bench_noop", {})],
    ["call_64k", () => call("bench_echo", { s: BENCH_64K })],
    ["call_nested", () => call("bench_nested", BENCH_NESTED)],
    ["stream_first_chunk", streamFirstChunk],
  ];

  const result: Record<string, unknown> = { warmup, n };
  for (const [name, probe] of cases) {
    try {
      for (let i = 0; i < warmup; i++) await probe();
      const samples: number[] = [];
      for (let i = 0; i < n; i++) {
        const t0 = performance.now();
        await probe();
        samples.push(performance.now() - t0);
      }
      samples.sort((a, b) => a - b);
      result[name] = {
        p50: benchQuantile(samples, 0.5),
        p95: benchQuantile(samples, 0.95),
        p99: benchQuantile(samples, 0.99),
      };
    } catch (e) {
      // A case that cannot run (fixtures missing on an app that did not
      // ship them) is recorded, not fatal — the report still lands.
      result[name] = { error: e instanceof Error ? e.message : String(e) };
    }
  }
  await (await ipc()).invoke(CMD.benchReport, { result });
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
  // Every python_state invoke subscribes this window for events; the
  // `b_ping` call below is what joins it to the fairness live set — the
  // observable mark main polls before it floods (Python state, not an
  // emit: an emit can arrive before main's listener exists and is gone
  // forever).
  for (let i = 0; i < 120; i++) {
    const s = await invoke<string>("plugin:pylair|python_state").catch(() => "");
    if (s === "ready") break;
    await sleep(500);
  }
  await call<number>("b_ping", {}).catch(() => -1);

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

  // Per-window fairness probe (docs/decisions/0005): while main holds
  // `hold` calls at its share ceiling, this window's plain call must
  // still be admitted — that is what the share is for. The rendezvous
  // is host-side: health() costs no task, so waiting here cannot
  // occupy a global slot and race main's refused call the way the old
  // Python-side poll could. A host predating python_health falls back
  // to the hold counter. A backend without the fixture is a skip,
  // never a failure.
  let fairnessProbe: { ok?: boolean; skipped?: string; error?: string; value?: unknown };
  {
    const first = await call<number>("holds_active", {}).then(
      (n) => n,
      (e) => (e instanceof PythonError && e.code === -32601 ? -1 : -2),
    );
    if (first === -1) {
      fairnessProbe = { ok: true, skipped: "backend has no holds_active fixture" };
    } else {
      let heldN = first;
      let healthMissing = false;
      for (let i = 0; i < 240 && heldN <= 0; i++) {
        const load = await health().then(
          (h) => h.load.tasks,
          () => {
            healthMissing = true;
            return -1;
          },
        );
        if (healthMissing) break;
        if (load > 0) {
          heldN = load;
          break;
        }
        await sleep(250);
      }
      if (healthMissing) {
        // Old host: no task-free observation exists — poll the counter
        // exactly as before. The window-fairness check on such legs is
        // not the strict one anyway (python_health itself skips).
        for (let i = 0; i < 120 && heldN <= 0; i++) {
          await sleep(500);
          heldN = await call<number>("holds_active", {}).catch(() => -2);
        }
      }
      if (heldN <= 0) {
        fairnessProbe = {
          ok: false,
          error: `main never held (in-flight count = ${String(heldN)} after 60s)`,
        };
      } else {
        fairnessProbe = await call<string>("hello", { name: "win-b" }).then(
          (value) => ({ ok: true, value }),
          (e: unknown) => ({
            ok: false,
            error: e instanceof Error ? e.message : String(e),
          }),
        );
      }
    }
  }

  // The report cannot ride emit/emitTo: a `core:event:deny-emit(-to)` entry
  // (as the default template ships) denies those commands origin-wide —
  // Tauri ignores window scoping for denials — so win-b relays through an
  // ACL-checked command whose Rust side emits for it.
  await invoke("plugin:pylair|python_selftest_b_report", {
    report: { sawEvents, everCancelled, anyGrantSucceeded, seenSample, sawStderr, sawStderrAny, denyProbe, fairnessProbe },
  }).catch(() => {});
}
