/**
 * @pylair/client — typed calls into the managed Python runtime.
 *
 * Transport: `plugin:pylair|python_*` commands. Call/stream delivery
 * rides a per-call `tauri::ipc::Channel` bound to the invoking webview —
 * not an event bus. `pylair://*` events remain for broadcast lifecycle
 * (state/stderr/exposes). See docs/protocol-v1.md. The bridge is
 * injectable for testing/headless use.
 */

import { CMD, ipc, liveTasks } from "./internal.js";
// Type only, erased at runtime: neither line loads selftest.js. The
// self-test is imported on demand inside maybeSelfTest.
import type { SelfTestSpec } from "./selftest.js";

export { _resetForTests, setBridge } from "./internal.js";
export type { SelfTestSpec } from "./selftest.js";

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
 *
 * `.limit === "window_share"` is the per-window fairness refusal
 * (docs/decisions/0005): the calling window exceeded its share of a
 * resource another live window needs. Then `.max` is this window's
 * current ceiling (not the global cap) and `data.window` carries the
 * offending owner's label (`null` for host-side calls). Retrying once
 * the flood drains is safe — nothing was committed.
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

/** Protocol code for per-function permission denials (docs/protocol-v1.md §8). */
export const PERMISSION_DENIED = -32050;

/**
 * The host refused the call before it reached Python: the window's
 * capability does not permit this function (docs/security.md, "Restricting
 * functions per window"). `.fn` is the function name and `.window` the
 * window label, both from the error's data; empty if the host sent none.
 * The call never ran and consumed no task.
 */
export class PermissionDeniedError extends PythonError {
  readonly fn: string;
  readonly window: string;
  constructor(message: string, data?: unknown) {
    super(PERMISSION_DENIED, message, data);
    this.name = "PermissionDeniedError";
    const d = (data ?? {}) as { fn?: unknown; window?: unknown };
    this.fn = typeof d.fn === "string" ? d.fn : "";
    this.window = typeof d.window === "string" ? d.window : "";
  }
}

/** Protocol code for update machinery failures (decision 0011). */
export const UPDATE_FAILED = -32060;

/**
 * An update command failed. `.updateCode` is the stable machine code
 * from the error's data — `"NotUpdatable"` (App Translocation, a
 * non-updater build, an unresolvable install location),
 * `"NeedsApproval"` (per-machine install requiring elevation),
 * `"SignatureInvalid"`, `"NoOffer"` (download/install before check),
 * `"EndpointError"`, `"CheckFailed"`, `"DownloadFailed"`,
 * `"CacheFailed"` (rollback cache), `"InstallFailed"`.
 */
export class UpdateError extends PythonError {
  readonly updateCode: string;
  constructor(message: string, data?: unknown) {
    super(UPDATE_FAILED, message, data);
    this.name = "UpdateError";
    const d = (data ?? {}) as { code?: unknown };
    this.updateCode = typeof d.code === "string" ? d.code : "UpdateFailed";
  }
}

/** Build the typed error for a wire code. */
function mkError(code: number, message: string, data?: unknown): PythonError {
  if (code === LIMIT_EXCEEDED) return new LimitExceededError(message, data);
  if (code === PERMISSION_DENIED) return new PermissionDeniedError(message, data);
  if (code === UPDATE_FAILED) return new UpdateError(message, data);
  return new PythonError(code, message, data);
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
  health: "pylair://health",
  app: "pylair://app",
} as const;

// ---- wire JSON (protocol §2, docs/codegen.md) -------------------------------
//
// The runtime encodes bytes as {"$b64": "<base64>"} and pylair.BigInt
// values as {"$bigint": "<decimal>"} — anywhere in the payload.
// `call`/`stream` apply the transform here so generated stubs and
// hand-written callers stay wire-agnostic: pass Uint8Array, get
// Uint8Array back; pass a JS bigint, get a JS bigint back.

const B64_TAG = "$b64";
const BLOB_TAG = "$blob";
const BIGINT_TAG = "$bigint";
// Both ends reject a digit string longer than this — a hostile peer
// must not be able to make either side parse a megabyte-long number.
const BIGINT_MAX_DIGITS = 4096;
const BIGINT_RE = /^-?(0|[1-9][0-9]*)$/;

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

// ---- blob handles (docs/decisions/0010-blob-urls.md) -----------------------
//
// The runtime encodes a pylair.Blob result or stream chunk as
// {"$blob": {"id","mime","size"}}; the bytes live host-side and ride back
// through the `pylair-blob:` URI scheme, owner-bound to the webview whose
// task produced them. This handle is the JS face of that file.

const BLOB_SCHEME = "pylair-blob";
/** mediaUrl() materializes a copy inside the webview — bounded (§6). */
const MEDIA_URL_CAP = 256 * 1024 * 1024;

/**
 * The scheme's URL base for this platform — mirrors `convertFileSrc`'s
 * rule (tauri `protocol::origin`): Windows/Android get
 * `http://<scheme>.localhost`, everything else `<scheme>://localhost`.
 */
function blobUrlBase(): string {
  const ua =
    (globalThis as { navigator?: { userAgent?: string } }).navigator?.userAgent ?? "";
  return ua.includes("Windows") || ua.includes("Android")
    ? `http://${BLOB_SCHEME}.localhost`
    : `${BLOB_SCHEME}://localhost`;
}

/**
 * A file held in the host's blob registry, produced by a `pylair.Blob`
 * result or stream chunk. `.url` hands it to `<img>`, media and fetch;
 * `.stream()` reads it as ranged chunks; `.release()` frees it early.
 *
 * Blob URLs die with their generation: a dev reload, runtime restart or
 * window close invalidates every outstanding handle (reads then 404).
 */
export class BlobHandle {
  /** Registry id — 128-bit unguessable, owner-bound. */
  readonly id: string;
  /** Content-Type the scheme serves. */
  readonly mime: string;
  /** Byte length registered at encode time. */
  readonly size: number;

  private _released = false;
  private _objectUrl: string | null = null;
  private _mediaPromise: Promise<string> | null = null;

  /** @internal Built by the `$blob` wire decoder — not for app code. */
  constructor(id: string, mime: string, size: number) {
    this.id = id;
    this.mime = mime;
    this.size = size;
  }

  private _assertLive(what: string): void {
    if (this._released)
      throw new Error(`blob ${this.id} was released — ${what} can no longer be used`);
  }

  /** The `pylair-blob:` URL — element loads, media (Win/macOS), fetch. */
  get url(): string {
    this._assertLive(".url");
    return `${blobUrlBase()}/b/${this.id}`;
  }

  /**
   * Read the blob as a ReadableStream of ranged chunks. Advances by the
   * bytes actually received (`Range: bytes=pos-`), so a server that
   * truncates 206s just means another request — scripted readers never
   * depend on the full-body cap. Stops at `size`; a non-2xx answer
   * (403/404/410/416) errors the stream naming the status.
   */
  stream(): ReadableStream<Uint8Array> {
    this._assertLive("stream()");
    const { id, size } = this;
    const url = `${blobUrlBase()}/b/${id}`;
    let pos = 0;
    return new ReadableStream<Uint8Array>({
      async pull(controller) {
        if (pos >= size) {
          controller.close();
          return;
        }
        const res = await fetch(url, {
          headers: { Range: `bytes=${pos}-${size - 1}` },
        });
        if (!res.ok) {
          controller.error(
            new Error(`blob read failed at byte ${pos}: HTTP ${res.status}`),
          );
          return;
        }
        const chunk = new Uint8Array(await res.arrayBuffer());
        if (chunk.length === 0) {
          // A 0-byte 206 would spin pull() forever — treat as a dead read.
          controller.error(
            new Error(
              `blob read stalled at byte ${pos} of ${size}: empty range response`,
            ),
          );
          return;
        }
        pos += chunk.length;
        controller.enqueue(chunk);
        if (pos >= size) controller.close();
      },
    });
  }

  /**
   * `Promise<string>` for a `blob:` object URL — the blob copied once
   * into the webview, then the NATIVE media pipeline owns it (instant
   * seek, works on WebKitGTK where the custom-scheme pipeline is broken;
   * §6). Capped at 256 MiB — a larger blob's mediaUrl() throws naming
   * `.url`/`stream()` instead of silently buffering a huge copy.
   * `release()` revokes the URL.
   */
  async mediaUrl(): Promise<string> {
    this._assertLive("mediaUrl()");
    if (this.size > MEDIA_URL_CAP)
      throw new Error(
        `blob ${this.id} is too large for mediaUrl() (${this.size} bytes > 256 MiB) — use .url or stream()`,
      );
    // Cache the PROMISE, not the URL: concurrent callers share one object
    // URL, and a release() landing mid-fetch revokes the URL it would
    // otherwise leak.
    if (this._mediaPromise === null) {
      this._mediaPromise = this.arrayBuffer()
        .then((buf) => {
          const u = URL.createObjectURL(new Blob([buf], { type: this.mime }));
          if (this._released) {
            URL.revokeObjectURL(u);
            throw new Error(
              `blob ${this.id} was released — mediaUrl() can no longer be used`,
            );
          }
          this._objectUrl = u;
          return u;
        })
        .catch((e) => {
          // A failed read doesn't poison the cache — the next call retries.
          this._mediaPromise = null;
          throw e;
        });
    }
    return this._mediaPromise;
  }

  /** The whole blob as an ArrayBuffer (via stream()). */
  async arrayBuffer(): Promise<ArrayBuffer> {
    return new Response(this.stream()).arrayBuffer();
  }

  /** The whole blob as a typed Blob (via stream()). */
  async blob(): Promise<Blob> {
    return new Blob([await this.arrayBuffer()], { type: this.mime });
  }

  /** The whole blob decoded as UTF-8 text (via stream()). */
  async text(): Promise<string> {
    return new Response(this.stream()).text();
  }

  /**
   * Free the blob early — `python_release_blob`, owner-checked host-side.
   * Idempotent (a second call is a no-op); afterwards `.url`, `.stream()`
   * and friends throw a clear error, and a mediaUrl() object URL is
   * revoked. Window close / generation end already reap every blob —
   * this is for "done with it NOW".
   */
  async release(): Promise<void> {
    if (this._released) return;
    this._released = true;
    if (this._objectUrl !== null) {
      URL.revokeObjectURL(this._objectUrl);
      this._objectUrl = null;
    }
    const b = await ipc();
    await b.invoke(CMD.releaseBlob, { id: this.id });
  }
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
 * same as `{...x}` spread. A JS `bigint` becomes `{"$bigint": "<decimal>"}`
 * (the wire's exact-integer tag — `pylair.BigInt` on the Python side).
 * Rejected: Date/Map/Set/RegExp/Promise/Error/
 * ArrayBuffer and views other than Uint8Array (own props don't hold the
 * data), functions/symbols (JSON can't represent them — JSON would drop
 * or throw later, we throw now with a path), non-plain objects with
 * zero own enumerable props (would serialize as {}), and circular
 * structures. `undefined`/`NaN`/`Infinity` keep JSON
 * semantics (omitted / null).
 */
function toWire(v: unknown, path = "input", seen = new Set<object>()): unknown {
  if (v === null || v === undefined) return v;
  const t = typeof v;
  if (t === "bigint") return { [BIGINT_TAG]: (v as bigint).toString() };
  if (t === "function" || t === "symbol")
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
    if (ks.length === 1 && ks[0] === BLOB_TAG) {
      const t = (v as Record<string, unknown>)[BLOB_TAG];
      // A handle only for a well-formed tag: an object with a string id
      // and numeric size. Anything else using the key ("$blob": null, a
      // number, a partial object) is the caller's own data — decode it
      // through the plain-object branch like any other value.
      if (
        t !== null &&
        typeof t === "object" &&
        typeof (t as Record<string, unknown>).id === "string" &&
        typeof (t as Record<string, unknown>).size === "number"
      ) {
        const tag = t as { id: string; mime?: unknown; size: number };
        return new BlobHandle(
          tag.id,
          typeof tag.mime === "string" ? tag.mime : "application/octet-stream",
          tag.size,
        );
      }
    }
    if (ks.length === 1 && ks[0] === BIGINT_TAG) {
      const s = (v as Record<string, unknown>)[BIGINT_TAG];
      if (typeof s === "string" && BIGINT_RE.test(s)) {
        const digits = s.startsWith("-") ? s.length - 1 : s.length;
        if (digits > BIGINT_MAX_DIGITS)
          throw new TypeError(
            `$bigint payload exceeds the ${BIGINT_MAX_DIGITS}-digit cap`,
          );
        return BigInt(s);
      }
      // Malformed tag (non-string, non-canonical) is caller data — fall
      // through to the plain-object branch like any other value.
    }
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
        else {
          // A fromWire throw (e.g. an over-cap $bigint) must reject —
          // settled is already true, so an uncaught throw hangs the call.
          try {
            resolve(fromWire(ev.result) as T);
          } catch (e) {
            reject(e instanceof Error ? e : new Error(String(e)));
          }
        }
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

// ---- runtime health (docs/decisions/0006) ------------------------------------

/** Transport liveness: pongs are arriving ("ok"), going unanswered
 * ("missing"), or there is no live generation to judge ("unknown"). */
export type HealthTransport = "ok" | "missing" | "unknown";

/** Event-loop liveness: serving ("ok"), stalled past the warn
 * threshold ("busy"), or unmeasurable — a `web` runtime never
 * reports loop health ("unknown"). */
export type HealthLoop = "ok" | "busy" | "unknown";

/**
 * Per-generation liveness (docs/decisions/0006 §4). `loop_stale_ms`
 * is present only for `kind: "bridge"`; on `web` runtimes it is
 * absent and `loop` reads "unknown" rather than a fabricated "ok".
 * Outside a live generation the age/stale fields are null and both
 * verdicts read "unknown".
 */
export interface HealthLiveness {
  /** ms since the last pong; null before the first. */
  last_pong_age_ms: number | null;
  /** Round-trip time of the last seq-echoed ping; null if unmatched. */
  rtt_ms: number | null;
  /** Pings currently unanswered for a full interval. */
  missed_pongs: number;
  /** Misses that kill the runtime. */
  missed_pongs_to_kill: number;
  /** Reported event-loop staleness (bridge kind only). */
  loop_stale_ms?: number | null;
  transport: HealthTransport;
  loop: HealthLoop;
}

export interface HealthRestarts {
  crashes_in_window: number;
  breaker: { crashes: number; window_ms: number };
  /** The backoff that will apply to the next restart. */
  backoff_ms: number;
  /** null until the first generation ends. */
  last_exit: {
    kind: "crashed" | "fatal" | "dev_restart";
    /** Free text — informational only (protocol-v1 §8), never parse it. */
    detail: string;
    ms_ago: number;
  } | null;
}

/** Global in-flight totals — every window summed, always counted. */
export interface HealthLoad {
  tasks: number;
  streams: number;
  isolated: number;
  queued: number;
  stream_bytes_total: number;
}

export interface HealthSelfCell {
  in_use: number;
  /** This window's current ceiling (0005 min-reserve). */
  ceiling: number;
  cap: number;
}

/**
 * The calling window's own row — `null` from a host-side `health()`
 * or when `per_window_fairness` is off. Never another window's row.
 */
export interface HealthSelf {
  window: string;
  tasks: HealthSelfCell;
  streams: HealthSelfCell;
  queued: HealthSelfCell;
  isolated: HealthSelfCell;
}

/**
 * One app-reported component's latest state (docs/decisions/0007 §5).
 * `progress` is the app's own best-effort number in [0,1] or `null`;
 * `detail` is free text — informational only, never parse it.
 */
export interface HealthAppComponent {
  phase: "loading" | "ready" | "failed";
  progress: number | null;
  detail: string | null;
  /** ms since the component's latest report. */
  ms_ago: number;
}

/**
 * App-reported readiness, latest report per component (0007 §5).
 * Runtime-wide, not caller-scoped — `health()` returns it on every
 * call. `null` when the generation has reported nothing and always on
 * `web` runtimes (a wrapped app cannot report); absent entirely
 * against a host that predates app readiness.
 */
export interface HealthApp {
  components: Record<string, HealthAppComponent>;
}

/**
 * The host-observed runtime-health snapshot (docs/decisions/0006 §4).
 *
 * `version` is the only schema contract: **version 1 may gain fields**;
 * a shape change bumps it. A `pylair:deny-python-health` capability
 * deny is origin-wide — it refuses the command on every local webview,
 * not just the window that carries the capability.
 */
export interface HealthSnapshot {
  version: 1;
  kind: "bridge" | "web";
  state: RuntimeState;
  in_state_ms: number;
  generation: number;
  /** ms this generation has been alive; null until it reaches Ready. */
  uptime_ms: number | null;
  /** Validated server url — web kind only. */
  url?: string | null;
  restarts: HealthRestarts;
  liveness: HealthLiveness;
  load: HealthLoad;
  self: HealthSelf | null;
  /** App-reported readiness — null/absent unless the runtime reports. */
  app?: HealthApp | null;
  /** Update activity (decision 0011) — absent on non-updater apps. */
  update?: HealthUpdate;
}

/**
 * `health().update` — the watchdog-visible update lifecycle. `state`
 * shares the `updating` readiness component's vocabulary:
 * `pending` (installed, first boot unconfirmed), `installing`,
 * `ready` (watchdog confirmed it), `rolled-back`,
 * `rollback-pending-approval` (per-machine: restore needs elevation),
 * `none`. `lastRollback` carries the watchdog's `{from, to, reason,
 * at}` record when a rollback happened.
 */
export interface HealthUpdate {
  state:
    | "none"
    | "pending"
    | "installing"
    | "ready"
    | "rolled-back"
    | "rollback-pending-approval";
  lastRollback?: { from?: string; to?: string; reason?: string; at?: number };
}

/**
 * The `pylair://health` event payload — emitted to subscribed webviews
 * only when a transport or loop verdict *changes*, never on a poll
 * cadence. Carries no per-window data (`self` is command-only).
 */
export interface HealthChange {
  version: 1;
  generation: number;
  liveness: HealthLiveness;
}

/**
 * The `pylair://app` event payload (docs/decisions/0007 §5): one emit
 * per component phase transition — a first sighting counts as a
 * transition into its phase — never per progress tick. Progress and
 * detail are poll-only; read `health().app` for the latest values.
 */
export interface AppChange {
  version: 1;
  generation: number;
  name: string;
  phase: "loading" | "ready" | "failed";
}

/**
 * The current health snapshot, caller-scoped. Like `state()` and
 * `retry()` this is read-only observation: calling it does not join
 * the fairness live set, so a poller reserves no share.
 *
 * Against a plugin that predates `python_health` the invoke rejects
 * — the same contract as `retry()`: the error propagates and never
 * hangs. Same for a `pylair:deny-python-health` ACL deny.
 */
export async function health(): Promise<HealthSnapshot> {
  return (await ipc()).invoke<HealthSnapshot>(CMD.health);
}

/**
 * Subscribe to liveness verdict changes; resolves to an unlisten
 * function. For "the runtime is busy, calls are slow" banners —
 * subscribe once and flip the banner on transitions instead of
 * polling `health()`.
 */
export async function onHealth(cb: (c: HealthChange) => void): Promise<() => void> {
  return (await ipc()).listen<HealthChange>(EVT.health, cb);
}

/**
 * Subscribe to app component phase transitions; resolves to an
 * unlisten function. For loading screens: render `health().app` once,
 * then flip components on `pylair://app` transitions instead of
 * polling — progress bars still read `health().app` on their own
 * cadence, since progress never emits an event (0007 §5).
 */
export async function onApp(cb: (c: AppChange) => void): Promise<() => void> {
  return (await ipc()).listen<AppChange>(EVT.app, cb);
}

// ---- installed-webview self-test, loaded on demand ---------------------------

/**
 * When the host runs the app with PYLAIR_SELF_TEST_WEBVIEW, the plugin sets
 * `window.__PYLAIR_SELF_TEST__`. Call this once at startup: it drives the
 * REAL bridge (ACL permissions, event routing, wire transform): a typed
 * call, an ordered stream, cancellation, shutdown. It then reports the
 * verdict so the host process exits 0/5. A no-op in normal runs.
 * PYLAIR_SELF_TEST_BENCH instead runs the latency bench (docs/performance.md)
 * and reports percentiles — a measurement, never a verdict.
 *
 * The self-test code (selftest.js) is loaded only when the flag is set.
 */
export async function maybeSelfTest(
  spec: SelfTestSpec = { add: "add", count: "count" },
): Promise<void> {
  if (!(globalThis as { __PYLAIR_SELF_TEST__?: boolean }).__PYLAIR_SELF_TEST__) return;
  // The import resolves before the self-test creates any task, so the
  // task ids it relies on are unaffected.
  let m: typeof import("./selftest.js");
  try {
    m = await import("./selftest.js");
  } catch (e) {
    // The host has no timeout of its own: without a verdict it would run
    // until killed. Report a failing one through the same command and
    // payload shape the owner role uses, then surface the error.
    const checks = { "selftest-import": `FAIL: ${e instanceof Error ? e.message : String(e)}` };
    await (await ipc())
      .invoke(CMD.selftestReport, { result: { ok: false, checks } })
      .catch(() => {});
    throw e;
  }
  return m.maybeSelfTest(spec);
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
  health,
  shutdown,
  onState,
  onStderr,
  onExposes,
  onHealth,
  onApp,
};

// ---- in-app updates (decision 0011) -----------------------------------------

/** `update.check()`'s answer for an offerable update. */
export interface UpdateInfo {
  /** The offered version. */
  version: string;
  /** Manifest `notes` — null when the release carries none. */
  notes: string | null;
  /** Manifest `sha256` extension for the platform artifact, when published. */
  sha256: string | null;
  /** The version this app is running. */
  current: string;
}

/** Download progress — the upstream updater plugin's event shapes. */
export type UpdateDownloadEvent =
  | { event: "Started"; data: { contentLength: number | null } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished"; data: Record<string, never> };

/**
 * Check the update endpoint for a newer release. Resolves to the offer
 * or `null` — including when the artifact is on the skip list, so a
 * rolled-back build is never re-presented. Throws `UpdateError`
 * (`updateCode` `NotUpdatable`) on a non-updater or translocated app.
 */
async function updateCheck(): Promise<UpdateInfo | null> {
  return (await ipc())
    .invoke<UpdateInfo | null>(CMD.updateCheck)
    .catch((e) => {
      throw asPythonError(e);
    });
}

/**
 * Download the checked update (progress via `onEvent` in the upstream
 * Started/Progress/Finished shapes), arm the watchdog contract and
 * install it. On Windows the install never returns — the process
 * exits into the installer — so anything that must persist before
 * relaunch is already written by the time this call resolves on other
 * platforms. Follow with `update.relaunch()` where it does return.
 */
async function updateDownloadAndInstall(opts: {
  onEvent?: (ev: UpdateDownloadEvent) => void;
} = {}): Promise<void> {
  const b = await ipc();
  await b
    .invokeChannel(CMD.updateDownload, {}, (ev) =>
      opts.onEvent?.(ev as unknown as UpdateDownloadEvent),
    )
    .catch((e) => {
      throw asPythonError(e);
    });
  await b.invoke(CMD.updateInstall, {}).catch((e) => {
    throw asPythonError(e);
  });
}

/** Restart into the installed version. */
async function updateRelaunch(): Promise<void> {
  return (await ipc())
    .invoke<void>(CMD.updateRelaunch, {})
    .catch((e) => {
      throw asPythonError(e);
    });
}

/**
 * In-app update surface (decision 0011). All commands reject with
 * `UpdateError`; the app must be built with the `updater` cargo
 * feature and a `plugins.updater` config block (`--with-updater`
 * scaffolds carry both) or every call lands `NotUpdatable`.
 */
export const update = {
  check: updateCheck,
  downloadAndInstall: updateDownloadAndInstall,
  relaunch: updateRelaunch,
};
