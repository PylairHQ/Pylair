/**
 * @pylair/client — typed calls into the managed Python runtime.
 *
 * Transport: `plugin:pylair|python_*` commands. Call/stream delivery
 * rides a per-call `tauri::ipc::Channel` bound to the invoking webview —
 * not an event bus. `pylair://*` events remain for broadcast lifecycle
 * (state/stderr/exposes). See docs/protocol-v1.md. The bridge is
 * injectable for testing/headless use.
 */

/**
 * Minimal bridge over Tauri invoke/listen — injectable for tests.
 *
 * `invokeChannel` carries the per-call event channel: the plugin command
 * requires an `onEvent` `tauri::ipc::Channel`, so each call/stream gets
 * its own delivery — there is no shared stream event bus to listen to
 * (that bus was the event-scoping leak). Breaking vs the pre-channel
 * interface: a custom bridge must now implement `invokeChannel`.
 */
export interface IpcBridge {
  invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  listen<T = unknown>(event: string, cb: (payload: T) => void): Promise<() => void>;
  /**
   * Invoke a streaming command with a per-call event sink. `onEvent`
   * receives StreamEvents for THIS invocation only; the implementation
   * binds it to a `Channel` passed as the `onEvent` argument.
   */
  invokeChannel<T = unknown>(
    cmd: string,
    args: Record<string, unknown>,
    onEvent: (ev: StreamEvent) => void,
  ): Promise<T>;
}

/** Python-side error object (docs/protocol-v1.md §8). */
export class PythonError extends Error {
  readonly code: number;
  readonly data?: unknown;
  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = "PythonError";
    this.code = code;
    this.data = data;
  }
}

/** Protocol code for limit rejections (docs/protocol-v1.md §8). */
export const LIMIT_EXCEEDED = -32040;

/**
 * A resource limit was hit — the host or runtime refused work rather than
 * queueing it unboundedly. `.limit` names the limit
 * ("max_concurrent_tasks", "max_concurrent_streams", "max_queued_commands",
 * "stream_buffer_bytes", "stream_buffer_bytes_total", "max_stream_credit",
 * "max_frame", "max_isolated_workers", "frame_out_queue"), `.max` carries
 * the configured bound when the host reported one.
 */
export class LimitExceededError extends PythonError {
  readonly limit: string;
  readonly max?: number;
  constructor(message: string, data?: unknown) {
    super(LIMIT_EXCEEDED, message, data);
    this.name = "LimitExceededError";
    const d = (data ?? {}) as { limit?: unknown; max?: unknown };
    this.limit = typeof d.limit === "string" ? d.limit : "unknown";
    if (typeof d.max === "number") this.max = d.max;
  }
}

/** Build the typed error for a wire code. */
function mkError(code: number, message: string, data?: unknown): PythonError {
  return code === LIMIT_EXCEEDED
    ? new LimitExceededError(message, data)
    : new PythonError(code, message, data);
}

/**
 * Normalize a thrown value into PythonError IF it is a serialized error
 * object (`{code, message, data?}`); anything else passes through
 * unchanged — transport failures keep their original identity.
 */
function asPythonError(e: unknown): unknown {
  const o = e as { code?: unknown; message?: unknown; data?: unknown };
  if (o && typeof o.code === "number")
    return mkError(
      o.code,
      typeof o.message === "string" ? o.message : String(e),
      o.data,
    );
  return e;
}

export type RuntimeState =
  | "starting"
  | "ready"
  | "unresponsive"
  | "restarting"
  | "failed"
  | "stopped";

export interface StateChange {
  from: RuntimeState;
  to: RuntimeState;
  detail: string;
}

export interface StreamEvent {
  task: number;
  seq?: number;
  value?: unknown;
  done: boolean;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

// Broadcast lifecycle events — stream/result delivery is NOT an event;
// it rides the per-call channel bound to the invoking webview.
const EVT = {
  state: "pylair://state",
  stderr: "pylair://stderr",
  exposes: "pylair://exposes",
} as const;

const CMD = {
  selftestReport: "plugin:pylair|python_selftest_report",
  call: "plugin:pylair|python_call",
  callStream: "plugin:pylair|python_call_stream",
  grantCredit: "plugin:pylair|python_grant_credit",
  cancel: "plugin:pylair|python_cancel",
  retry: "plugin:pylair|python_retry",
  state: "plugin:pylair|python_state",
  shutdown: "plugin:pylair|python_shutdown",
} as const;

// ---- bridge management -----------------------------------------------------

let _bridge: IpcBridge | null = null;
let _defaultBridge: Promise<IpcBridge> | null = null;

function defaultBridge(): Promise<IpcBridge> {
  _defaultBridge ??= Promise.all([
    import("@tauri-apps/api/core"),
    import("@tauri-apps/api/event"),
    import("@tauri-apps/api/webview"),
  ]).then(([core, event, webviewApi]): IpcBridge => {
    // Label-scoped listeners: an `Any`-target handler receives every event
    // regardless of the emit destination (Tauri's match_any_or_filter), so
    // a stock window on plain `listen` would see stderr addressed to other
    // windows. Webview-scoped handlers are filtered by `emit_to` — only a
    // deliberate Any-listen still sees other windows' events.
    let webviewListen: IpcBridge["listen"] | null = null;
    try {
      const wv = webviewApi.getCurrentWebview();
      webviewListen = <T,>(ev: string, cb: (p: T) => void) =>
        wv.listen<T>(ev, (e: { payload: T }) => cb(e.payload));
    } catch {
      // No webview context (tests/headless) — fall back to plain listen.
    }
    const listen =
      webviewListen ??
      (<T,>(ev: string, cb: (p: T) => void) =>
        event.listen<T>(ev, (e: { payload: T }) => cb(e.payload)));
    return {
      invoke: (cmd, args) => core.invoke(cmd, args),
      listen,
      invokeChannel: (cmd, args, onEvent) => {
        // onmessage is wired BEFORE invoke — the first chunk can race
        // the invoke response and must not be lost.
        const channel = new core.Channel<StreamEvent>();
        channel.onmessage = onEvent;
        return core.invoke(cmd, { ...args, onEvent: channel });
      },
    };
  });
  return _defaultBridge;
}

/** Inject a custom bridge (tests, non-webview hosts). */
export function setBridge(bridge: IpcBridge | null): void {
  _bridge = bridge;
}

async function ipc(): Promise<IpcBridge> {
  return _bridge ?? defaultBridge();
}

// ---- wire JSON (protocol §2, docs/codegen.md) -------------------------------
//
// The runtime encodes bytes as {"$b64": "<base64>"} — anywhere in the
// payload. `call`/`stream` apply the transform here so generated stubs
// and hand-written callers stay wire-agnostic: pass Uint8Array, get
// Uint8Array back.

const B64_TAG = "$b64";

function b64encode(u8: Uint8Array): string {
  let s = "";
  for (let i = 0; i < u8.length; i += 0x8000)
    s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
  return btoa(s);
}

function b64decode(b64: string): Uint8Array {
  const s = atob(b64);
  const u8 = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) u8[i] = s.charCodeAt(i);
  return u8;
}

/**
 * Types whose own enumerable properties do NOT capture their data —
 * serializing them produces `{}` (or garbage) and silently loses the
 * value. Named so the TypeError can tell the user exactly what they
 * passed, and where.
 */
const NOT_WIRE_DATA: ReadonlyArray<[abstract new (...a: never[]) => unknown, string]> = [
  [Date, "a Date"],
  [RegExp, "a RegExp"],
  [Map, "a Map"],
  [Set, "a Set"],
  [WeakMap, "a WeakMap"],
  [WeakSet, "a WeakSet"],
  [Promise, "a Promise"],
  [Error, "an Error"],
  [ArrayBuffer, "an ArrayBuffer"],
  [DataView, "a DataView"],
  [Int8Array, "an Int8Array"],
  [Uint8ClampedArray, "a Uint8ClampedArray"],
  [Int16Array, "an Int16Array"],
  [Uint16Array, "a Uint16Array"],
  [Int32Array, "an Int32Array"],
  [Uint32Array, "a Uint32Array"],
  [Float32Array, "a Float32Array"],
  [Float64Array, "a Float64Array"],
  [BigInt64Array, "a BigInt64Array"],
  [BigUint64Array, "a BigUint64Array"],
];

/**
 * Value → wire JSON. Anything that would silently lose data is a
 * TypeError naming the path (e.g. `input.when is a Date`), thrown
 * synchronously so nothing reaches the bridge. Supported: null,
 * undefined, booleans, numbers, strings, arrays, plain objects
 * (including null-prototype), `Uint8Array` (→ `$b64`), and class
 * instances carrying plain data — own enumerable props ARE the intent,
 * same as `{...x}` spread. Rejected: Date/Map/Set/RegExp/Promise/Error/
 * ArrayBuffer and views other than Uint8Array (own props don't hold the
 * data), functions/symbols/bigint (JSON can't represent them — JSON
 * would drop or throw later, we throw now with a path), non-plain
 * objects with zero own enumerable props (would serialize as {}), and
 * circular structures. `undefined`/`NaN`/`Infinity` keep JSON
 * semantics (omitted / null).
 */
function toWire(v: unknown, path = "input", seen = new Set<object>()): unknown {
  if (v === null || v === undefined) return v;
  const t = typeof v;
  if (t === "function" || t === "symbol" || t === "bigint")
    throw new TypeError(`${path} is a ${t} — cannot cross the bridge`);
  if (v instanceof Uint8Array) return { [B64_TAG]: b64encode(v) };
  if (t !== "object") return v;
  for (const [ctor, what] of NOT_WIRE_DATA)
    if (v instanceof ctor)
      throw new TypeError(`${path} is ${what} — pass a plain value or spread it yourself`);
  const obj = v as Record<string, unknown>;
  if (seen.has(obj))
    throw new TypeError(`circular reference at ${path}`);
  seen.add(obj);
  try {
    if (Array.isArray(obj))
      return obj.map((x, i) => toWire(x, `${path}[${i}]`, seen));
    const entries = Object.entries(obj);
    const proto = Object.getPrototypeOf(obj);
    if (proto !== Object.prototype && proto !== null && entries.length === 0) {
      const name = obj.constructor?.name ?? "anonymous-class";
      throw new TypeError(
        `${path} is a ${name} instance with no own enumerable properties — it would serialize as {}`,
      );
    }
    return Object.fromEntries(
      entries.map(([k, x]) => [k, toWire(x, `${path}.${k}`, seen)]),
    );
  } finally {
    seen.delete(obj);
  }
}

function fromWire(v: unknown): unknown {
  if (v !== null && typeof v === "object") {
    const ks = Object.keys(v);
    if (ks.length === 1 && ks[0] === B64_TAG)
      return b64decode((v as Record<string, string>)[B64_TAG]);
    if (Array.isArray(v)) return v.map(fromWire);
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, x]) => [
        k,
        fromWire(x),
      ]),
    );
  }
  return v;
}

// ---- internal stream machinery ---------------------------------------------
//
// Every call — plain or streaming — goes through `python_call_stream` so it
// always has a task id (needed for cancellation). Delivery arrives on the
// per-call `Channel` passed at invoke time: each invocation only ever sees
// its own events, so no task-map routing or early-event buffering exists.

/**
 * Bridge protocol version — `python_call_stream` answers
 * `{task, protocol: 2}`. `2` marks per-call channel delivery; `1` was the
 * bare task id + shared event bus. (Unrelated to the host↔Python wire
 * protocol in docs/protocol-v1.md.)
 */
const BRIDGE_PROTOCOL = 2;

interface StreamAck {
  task: number;
  protocol: number;
}

/**
 * Read the task id out of a `python_call_stream` ack. An OLD plugin
 * ignores the `onEvent` argument entirely and answers a bare task number
 * — the task IS started but no channel delivery can ever arrive, so a
 * bare-number answer would otherwise hang the call forever. Fail loudly
 * and cancel the orphaned task (best effort — the old plugin may already
 * have finished it). Anything else unexpected fails loudly too.
 */
function streamAckTask(ack: unknown): number {
  if (
    typeof ack === "object" &&
    ack !== null &&
    typeof (ack as StreamAck).task === "number" &&
    (ack as StreamAck).protocol === BRIDGE_PROTOCOL
  ) {
    return (ack as StreamAck).task;
  }
  if (typeof ack === "number") {
    void ipc().then((b) => b.invoke(CMD.cancel, { task: ack }).catch(() => {}));
    throw new Error(
      "this app's Pylair plugin is older than the client; upgrade tauri-plugin-pylair",
    );
  }
  throw new Error(`unexpected python_call_stream response: ${JSON.stringify(ack)}`);
}

/**
 * Live task ids in THIS window — not for routing (channels do that), only
 * so a page unload can cancel them. A reload kills every channel but
 * `Channel::send` does NOT reliably error on a reload (the webview object
 * survives; the eval lands in a dead JS context and the callback is a
 * no-op). The plugin's on_page_load hook already reaps the document's
 * tasks server-side when a navigation commits; this hook stays as a best
 * effort belt for engines that never surface a load event.
 */
const liveTasks = new Set<number>();
let unloadHooked = false;

function trackLive(task: number): void {
  // No DOM lib in this package — globalThis is the window in a webview.
  const g = globalThis as {
    addEventListener?: (event: string, cb: () => void) => void;
  };
  if (typeof g.addEventListener === "function" && !unloadHooked) {
    unloadHooked = true;
    g.addEventListener("pagehide", () => {
      // Best effort: teardown may cut the invoke off, but when it lands
      // the tasks die instead of running orphaned under a dead channel.
      void ipc().then((b) => {
        for (const task of liveTasks) b.invoke(CMD.cancel, { task }).catch(() => {});
      });
    });
  }
  liveTasks.add(task);
}

/**
 * Wire an AbortSignal to a task's cancel; returns a disarm that removes
 * the listener. The listener MUST be removed when the call settles —
 * `{once:true}` only cleans up after firing, so a long-lived signal
 * reused across calls would otherwise accumulate a listener (and the
 * closure) per call forever.
 */
function wireAbort(signal: AbortSignal | undefined, task: number): () => void {
  if (!signal) return () => {};
  const cancel = () => {
    void ipc().then((b) => b.invoke(CMD.cancel, { task }));
  };
  if (signal.aborted) {
    cancel();
    return () => {};
  }
  signal.addEventListener("abort", cancel, { once: true });
  return () => signal.removeEventListener("abort", cancel);
}

// ---- public API -------------------------------------------------------------

export interface CallOptions {
  signal?: AbortSignal;
  /**
   * Application-level deadline. When it expires the task is cancelled and the
   * call rejects/throws a PythonError with code DEADLINE_EXCEEDED, without
   * waiting for the runtime to acknowledge — a blocked runtime cannot be
   * relied on to answer.
   */
  timeoutMs?: number;
}

/** Client-side error code for an expired `timeoutMs`. */
export const DEADLINE_EXCEEDED = -32013;

function deadlineError(ms: number): PythonError {
  return new PythonError(DEADLINE_EXCEEDED, `deadline of ${ms}ms exceeded`);
}

/** Arm `timeoutMs`: on expiry cancel the task and run `onExpire`. */
function armDeadline(
  opts: CallOptions,
  task: number,
  onExpire: (e: PythonError) => void,
): () => void {
  const ms = opts.timeoutMs;
  if (ms === undefined || ms <= 0) return () => {};
  const t = setTimeout(() => {
    void ipc().then((b) => b.invoke(CMD.cancel, { task })).catch(() => {});
    onExpire(deadlineError(ms));
  }, ms);
  (t as { unref?: () => void }).unref?.();
  return () => clearTimeout(t);
}

/** Call a Python function; resolves with its result. */
export async function call<T = unknown>(
  fn: string,
  input: Record<string, unknown> = {},
  opts: CallOptions = {},
): Promise<T> {
  const b = await ipc();
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const result = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  // The terminal can arrive before the invoke resolves — the callback is
  // bound at invoke time, so nothing is lost. `task` must be a let bound
  // before invokeChannel runs: the callback can fire during the invoke.
  let settled = false;
  let disarm = () => {};
  let unwire = () => {};
  let task = -1;
  // A chunk arriving before the ack means the task id isn't known yet —
  // the reject still runs now; the cancel is deferred to the ack.
  let cancelOnAck = false;
  const failStreamMismatch = () => {
    if (settled) return;
    settled = true;
    liveTasks.delete(task);
    disarm();
    unwire();
    cancelOnAck = task < 0;
    if (!cancelOnAck) void b.invoke(CMD.cancel, { task }).catch(() => {});
    reject(
      new TypeError(`${fn} is a stream function — call it with stream(), not call()`),
    );
  };
  const ack = await b
    .invokeChannel<unknown>(
      CMD.callStream,
      // stream:false — the host counts a task slot only (no stream
      // slot, no credit) and the runtime validates intent. An OLD
      // plugin ignores the extra arg and treats it as a stream,
      // matching its pre-flag accounting.
      { fnName: fn, input: toWire(input) as Record<string, unknown>, stream: false },
      (ev) => {
        // A chunk means fn is a generator — call() cannot deliver its
        // values; name stream() and reap the task instead of silently
        // dropping every item.
        if (!ev.done) return failStreamMismatch();
        if (settled) return;
        settled = true;
        liveTasks.delete(task);
        disarm();
        unwire();
        if (ev.error) reject(mkError(ev.error.code, ev.error.message, ev.error.data));
        else resolve(fromWire(ev.result) as T);
      },
    )
    .catch((e) => {
      throw asPythonError(e);
    });
  task = streamAckTask(ack);
  if (cancelOnAck) void b.invoke(CMD.cancel, { task }).catch(() => {});
  if (!settled) {
    unwire = wireAbort(opts.signal, task);
    trackLive(task);
    disarm = armDeadline(opts, task, (e) => {
      unwire();
      reject(e);
    });
  }
  return result;
}

const CREDIT_BATCH = 32;

/** Call an async-generator function; yields chunks as they arrive. */
export async function* stream<T = unknown>(
  fn: string,
  input: Record<string, unknown> = {},
  opts: CallOptions = {},
): AsyncGenerator<T, void, void> {
  const b = await ipc();
  const queue: StreamEvent[] = [];
  let wake: (() => void) | null = null;
  let pendingCredit = 0;
  let finished = false; // terminal event seen: producer already done
  let expired: PythonError | null = null;
  // Bound at invoke time — events queue locally even if they beat the
  // invoke response back.
  const ack = await b
    .invokeChannel<unknown>(
      CMD.callStream,
      { fnName: fn, input: toWire(input) as Record<string, unknown>, stream: true },
      (ev) => {
        queue.push(ev);
        wake?.();
      },
    )
    .catch((e) => {
      throw asPythonError(e);
    });
  const task = streamAckTask(ack);
  const unwire = wireAbort(opts.signal, task);
  trackLive(task);
  const disarm = armDeadline(opts, task, (e) => {
    unwire();
    expired = e;
    finished = true; // the deadline already sent the cancel
    wake?.();
  });

  try {
    for (;;) {
      while (queue.length === 0) {
        if (expired) throw expired;
        await new Promise<void>((r) => (wake = r));
        wake = null;
      }
      if (expired) throw expired;
      const ev = queue.shift()!;
      if (ev.done) {
        finished = true;
        if (ev.error) throw mkError(ev.error.code, ev.error.message, ev.error.data);
        return;
      }
      // Pull-flow: top up credit in batches as the consumer pulls.
      if (++pendingCredit >= CREDIT_BATCH) {
        // Grant can now reject loudly (task finished between grants —
        // TASK_NOT_FOUND). A rejected grant is unremarkable: the stream
        // is already over, so swallow it.
        void b.invoke(CMD.grantCredit, { task, n: pendingCredit }).catch(() => {});
        pendingCredit = 0;
      }
      yield fromWire(ev.value) as T;
    }
  } finally {
    disarm();
    unwire();
    liveTasks.delete(task);
    // Consumer walked away (break / return() / throw) before the producer
    // finished: stop it instead of leaving it running and granting credit.
    if (!finished) void b.invoke(CMD.cancel, { task }).catch(() => {});
    else if (pendingCredit > 0)
      void b.invoke(CMD.grantCredit, { task, n: pendingCredit }).catch(() => {});
  }
}

/**
 * What a cancel actually did — distinct outcomes, never an ambiguous bool.
 * `cancelled`: runtime confirmed the work stopped.
 * `requested_may_continue`: inline sync work can't be interrupted — the
 *   caller sees TASK_CANCELLED but the Python may run to completion.
 * `terminated`: an isolated worker was terminated and reaped.
 * `termination_uncertain`: termination was requested but unconfirmed —
 *   the work may still be running.
 * `completed`: the task had already finished before the cancel landed.
 * `not_found`: no such task in this runtime generation (stale id).
 * `rejected`: this webview does not own the task.
 * `delivery_failed`: the control frame could not be queued at all.
 */
export type CancelOutcome =
  | "cancelled"
  | "requested_may_continue"
  | "terminated"
  | "termination_uncertain"
  | "completed"
  | "not_found"
  | "rejected"
  | "delivery_failed";

/** What shutdown actually achieved — reported, never assumed. */
export type ShutdownOutcome =
  | "terminated" // graceful exit inside the grace window
  | "forced_kill" // had to SIGKILL/taskkill — the tree is confirmed dead
  | "incomplete" // kill sent but the exit wasn't confirmed
  | "unknown"; // runtime already gone or state unknown

/** Explicitly cancel a running task by id. Only the task's own webview
 * may cancel it — the supervisor enforces ownership server-side. */
export async function cancel(task: number): Promise<CancelOutcome> {
  return (await ipc()).invoke<CancelOutcome>(CMD.cancel, { task });
}

/** Retry after the supervisor reaches `failed` — respawns the runtime. */
export async function retry(): Promise<void> {
  await (await ipc()).invoke(CMD.retry);
}

/** Current supervisor state. */
export async function state(): Promise<RuntimeState> {
  return (await ipc()).invoke<RuntimeState>(CMD.state);
}

/** Ask the supervisor to shut the runtime down; resolves to what
 * cleanup actually achieved, not merely that the request was queued. */
export async function shutdown(): Promise<ShutdownOutcome> {
  return (await ipc()).invoke<ShutdownOutcome>(CMD.shutdown);
}

/** Subscribe to lifecycle transitions; returns an unlisten function. */
export async function onState(cb: (s: StateChange) => void): Promise<() => void> {
  return (await ipc()).listen<StateChange>(EVT.state, cb);
}

/** Subscribe to runtime stderr lines. */
export async function onStderr(cb: (line: string) => void): Promise<() => void> {
  return (await ipc()).listen<string>(EVT.stderr, cb);
}

/** Subscribe to the exposes manifest emitted after handshake. */
export async function onExposes<T = unknown>(cb: (manifest: T) => void): Promise<() => void> {
  return (await ipc()).listen<T>(EVT.exposes, cb);
}

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

/**
 * Namespace-style convenience surface; `python.<fn>` typed wrappers are
 * generated by `pylair codegen` in M2 on top of call/stream.
 */
export const python = {
  call,
  stream,
  cancel,
  retry,
  state,
  shutdown,
  onState,
  onStderr,
  onExposes,
};

/** For tests: reset the injected bridge between cases. */
export function _resetForTests(): void {
  liveTasks.clear();
  _bridge = null;
  _defaultBridge = null;
}
