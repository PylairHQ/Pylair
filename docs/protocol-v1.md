# Pylair wire protocol v1

The contract between the host (Rust supervisor, inside the Tauri app) and
the managed Python runtime process. Implemented in M1; streaming and
cancellation messages are specified here but land in M3. Everything here
is deliberately boring — JSON-RPC 2.0 shaped, length-prefixed, stdio.

Goals: unambiguous framing, a versioned handshake, an error model that
maps cleanly to TypeScript, and no way for a malformed or hostile peer to
hang or OOM the host.

## 1. Transport

- Bytes flow over the child process's stdio: host writes to child stdin,
  child writes to child stdout. Stderr is a free-form log stream — the
  host forwards it to diagnostics; it is *never* part of the protocol.
- Behind a `Transport` trait in Rust and a matching abstraction in
  Python, so unix sockets / Windows named pipes can replace stdio later
  without touching message semantics.
- **stdout carries frames and nothing else.** Any byte on the frame
  channel that is not a well-formed frame is a protocol violation → the
  host treats the runtime as crashed (kill + restart). But libraries
  `print()`, and native code can write straight to fd 1, so the runtime
  does not rely on discipline — it performs **stdout hygiene at startup,
  before any third-party import can run**:
  - `dup()` the real stdout to a private fd — that fd is the frame
    channel;
  - `dup2(stderr, 1)` — fd 1 now points at stderr, so stray writes
    (Python `print`, C-extension `fwrite`, anything) land in the log
    stream instead of corrupting frames;
  - on Windows, both stdin and the frame fd are put in binary mode
    (`_setmode(..., O_BINARY)`) — CRLF translation would otherwise
    corrupt length-prefixed frames.
- **The host drains stderr continuously** on a dedicated task/thread.
  A full stderr pipe deadlocks the child, so draining is unconditional,
  not tied to diagnostics being enabled; if consumers are slow the host
  drops oldest lines with a `[dropped N lines]` marker rather than let
  the pipe fill.

## 2. Framing

```
+--------+--------------+
| u32 BE |  JSON body   |   u32 = byte length of the JSON body
+--------+--------------+
```

- Big-endian length prefix, then exactly that many bytes of UTF-8 JSON.
- `MAX_FRAME` defaults to **16 MiB**, configurable via
  `PYLAIR_MAX_FRAME` (bytes). A length prefix above the cap →
  protocol violation → runtime killed and restarted (it is either buggy
  or hostile). The cap is not meant for data throughput — `bytes` still
  travels base64-in-JSON (+33% size); large binary payloads (images,
  audio) ride `pylair.Blob` handles over host-served `pylair-blob` URLs
  instead (`docs/blobs.md`, decision 0010).
- `MAX_DEPTH` defaults to **100** JSON nesting levels, configurable via
  `PYLAIR_MAX_DEPTH`. The host parses with serde_json, which refuses
  input nested deeper than ~128 — a response or `stream.chunk` value
  over the runtime's own depth cap fails ONLY that call with
  `LIMIT_EXCEEDED` (`"max_depth"`), never the runtime. A notification
  that exceeds it (no caller to report to) is dropped and logged.
- A peer that goes silent mid-frame is just slow; no read timeout on
  frames. Startup has its own deadline (§4) and steady-state liveness is
  covered by ping/pong (§5).
- **Wire JSON encoding** (`_json.py`, docs/codegen.md): beyond plain JSON
  types the runtime encodes `bytes`/`bytearray` as `{"$b64": "<base64>"}`
  (decoded symmetrically on input), `pylair.BigInt` as
  `{"$bigint": "<decimal>"}` (decoded symmetrically on input), `@dataclass`
  as its field dict,
  `datetime`/`date`/`time` as ISO 8601 strings, `Enum` as its value, and
  `set` as a list. Incoming params are validated then coerced by
  annotation — a dataclass-annotated parameter receives an instance,
  not a dict, and a value the annotation can't hold (wrong primitive,
  missing required key, unexpected key) is `INVALID_PARAMS` before the
  function body runs.
- **`{"$bigint": "<decimal>"}` is the exact-integer tag.** A value typed
  `pylair.BigInt` encodes this way regardless of magnitude — opt-in by
  type, never inferred: plain `int` keeps JSON-number semantics (safe
  range is numbers; outside fails the call with a clear error naming
  `pylair.BigInt`). In the host→runtime direction the client encodes a
  JS `bigint` argument as the same tag and the runtime decodes it back
  to `pylair.BigInt`, so exact integers round-trip in both directions.
  The declared type decides for results too: under a `-> pylair.BigInt`
  signature (`Optional`/`list[BigInt]` recurse; a generator's yield
  schema does the same for stream chunks), plain `int` results are
  widened to `BigInt` before encoding — int arithmetic drops the
  subclass, so `return a + b` still tags. A non-int result keeps the
  value's own encoding: the signature only widens ints, never str,
  float or bool.
  Decoding on both sides is defensive and matches the other tags: only
  an object with exactly the one key `"$bigint"` whose value matches
  `^-?(0|[1-9][0-9]*)$` becomes a big integer — anything else (extra
  keys, non-canonical or non-string values) passes through as plain
  data. The decimal string is capped at **4096 digits** (~12k bits):
  a longer payload is rejected with a clear error on both sides, so a
  hostile peer cannot make either end parse an arbitrary-length number.
  A peer that predates the tag sees a plain `{"$bigint": ...}` object —
  `pylair.BigInt` requires matching `pylair` and `@pylair/client`
  versions (released together). The host treats the tag as opaque data
  and never inspects it.
- **Serialization happens on the event loop**, before a message reaches
  the writer thread. A result that can't serialize (numpy/tensor
  objects, `NaN`/`Infinity` — `allow_nan=False` since bare `NaN` is not
  valid JSON and serde_json would reject the frame) fails *that one
  call* with `INTERNAL`; the writer thread sees only valid bytes and
  can never die silently.
- Param coercion failures are caller errors: `INVALID_PARAMS`, not a
  function exception (docs/codegen.md).

## 3. Message shapes

All messages carry `"jsonrpc": "2.0"`.

### Requests (host → runtime)

| method | params | response result |
|---|---|---|
| `call` | `{fn, input, task, stream?}` | fn's return value (JSON); for generator fns, see §3.1 |
| `cancel` | `{task}` | `{cancelled: true/false, status?, reason?}` — see §3.2 |
| `shutdown` | `{grace_ms}` | `{}` then process exits 0 |
| `ping` | `{seq}` | must produce a `pong {seq}` notification — see §5 |

- `fn`: name registered via `@expose`. Anything else →
  `FUNCTION_NOT_FOUND`. There is no way to reach unexposed callables —
  the allowlist lives in Python (`_registry`), enforced again by the
  host's permission layer. Names must be unique across modules —
  registering the same name for a different definition raises at import
  (re-importing the same definition replaces silently, so hot reload
  keeps working). An isolated function may live in any module the app
  imports; the worker resolves it through the registry, not by
  attribute lookup on the app module.
- `task`: opaque id assigned by the host; scopes streaming + cancel.
- `stream` (optional bool): the host's declaration of client intent —
  `false` from a plain `call()`, `true` from `stream()`. When present,
  the runtime validates it against the function's kind: `stream: false`
  on a generator or `stream: true` on a non-generator fails
  `INVALID_PARAMS` with a message naming the right API, instead of
  streaming into a `call()` that drops every chunk or a `stream()` that
  silently ends. Older hosts omit it → no validation.
- `input`: the single JSON argument object, matching the generated TS
  client call shape `python.<fn>({...})`. **What the client accepts**
  (`toWire`): `null`, `undefined`, booleans, numbers, strings, arrays,
  plain objects (null-prototype included), `Uint8Array` (→ `$b64`),
  `bigint` (→ `$bigint`), and
  class instances carrying plain data — own enumerable props are the
  intent, same as `{...x}` spread. Anything whose data doesn't live in
  own enumerable props is a TypeError naming the path: `Date`, `Map`,
  `Set`, `RegExp`, `Promise`, `Error`, `ArrayBuffer`, typed arrays other
  than `Uint8Array`, functions, symbols, non-plain objects
  with zero own enumerable props (would serialize as `{}`), and
  circular references. `undefined`/`NaN`/`Infinity` keep JSON
  semantics — omitted in objects, `null` in arrays.

### Responses (runtime → host)

```json
{"jsonrpc":"2.0","id":3,"result":{...}}
{"jsonrpc":"2.0","id":3,"error":{"code":-32010,"message":"task cancelled","data":{...}}}
```

`id` echoes the request. `error.data` may carry a Python traceback and
a `type` string (the Python exception class) for diagnostics.

### Notifications

| method | dir | params | meaning |
|---|---|---|---|
| `hello` | runtime→host | see §4 | handshake — first frame, once |
| `stream.chunk` | runtime→host | `{task, seq, value}` | one item of an async-generator result |
| `stream.credit` | host→runtime | `{task, n}` | consumer grants the producer n chunks of credit |
| `pong` | runtime→host | `{seq}` | liveness reply to `ping` |
| `log` | runtime→host | `{level, msg}` | structured log line (also goes to stderr) |
| `ready` | runtime→host | `{url}` | **optional, web runtimes only**: the wrapped server answers HTTP at `url`; see §4.1. Bridge runtimes never send it, and a bridge-mode host ignores it like any unknown notification |
| `app.component` | runtime→host | `{name, phase, progress?, detail?}` | **optional, bridge runtimes only**: a coalesced report of one app-declared component's readiness (decisions 0007). `phase` ∈ `"loading" \| "ready" \| "failed"`; `progress` is a float in `[0, 1]`, absent when indeterminate; `detail` is free text. Fire-and-forget — a host that predates it ignores it like any unknown notification |

### 3.1 Calls and streaming

A `call` request always gets **exactly one response**.

- Non-streaming functions: the response is the return value
  (`result` or `error`), as usual.
- Streaming functions (`kind: "generator"` in `hello.exposes`): the
  `call`'s response **is the stream's terminal message** — it arrives
  after the last `stream.chunk` and carries either
  `result: {"done": true}` or an `error` (e.g. `TASK_CANCELLED`,
  `INTERNAL` raised mid-generation). The front end knows a call streams
  from the handshake allowlist metadata, not from message-shape
  inference — `python.<fn>` for a generator always yields an
  `AsyncIterable` whose items are the `stream.chunk` values and whose
  end/error is the call response.

**Flow control is pull-based and mandatory.** Async generators are
pull-based, so the wire is too: the runtime may only call `__anext__`
on a generator while it holds credit for that task.

- The consumer grants credit with `stream.credit {task, n}`. Credit is
  in *chunks*; each `stream.chunk` sent decrements outstanding credit
  by one.
- On stream open (a `call` request carrying `stream: true`, or a
  pre-flag request with no `stream` field) the host grants an initial
  credit of `INITIAL_STREAM_CREDIT` (default 64). A `stream: false`
  call gets no initial grant — it produces no chunks. Like every
  `stream.credit` it
  travels the control lane — before the call frame — so a saturated
  data lane cannot starve or reorder it, and the runtime applies it
  through pending credit if it arrives before registration. The TS
  client replenishes as items are consumed — a throttled or
  backgrounded webview simply stops granting, and the generator pauses
  at the next `__anext__`. The stream stalls, it does not die.
- Safety cap for misbehaving producers: a `stream.chunk` that arrives
  with no outstanding credit is a protocol violation — the host fails
  every in-flight call with `PROTOCOL_VIOLATION` and kills the runtime,
  the same treatment a malformed frame gets (a runtime that cannot
  count credit cannot be trusted to honour a per-task abort either).
  Chunks naming a task the generation no longer knows are ignored — a
  plausible race, not a violation. This is the only remaining abort
  path — a well-behaved producer can never hit a limit by streaming
  "too fast"; it can only run out of credit, which pauses it.
- `cancel` is independent of credit: a task is cancellable whether it
  is producing, waiting on credit, or fully drained.

### 3.2 Cancel acknowledgement

`cancel` returns `{cancelled, status?, reason?}`:

- `cancelled: true` — the runtime accepted the cancel. `status` refines
  where it landed:
  - absent — the task was live and has been cancelled.
  - `"pending"` — the task id is not registered **yet**: the host's
    control lane can overtake the data lane, so a `cancel` may arrive
    before its `call`. The runtime records the id in a bounded pending
    set and cancels the task at registration — the call still gets a
    terminal `TASK_CANCELLED` response. Because the host only forwards
    cancels for tasks of the current generation, a not-yet-registered id
    is always a call still in flight, so `pending` is an acceptance, not
    a miss. The runtime tracks the highest task id whose call frame it
    has handled (host ids only increase and frames arrive in order):
    only unknown ids **above** that mark are stored as pending — an
    unknown id at or below it has already finished or been rejected and
    is answered `task_not_found`, so the pending set stays proportional
    to the in-flight window rather than every id ever seen.
  - `"terminated"` — an isolated worker was killed and reaped.
  - `"may_continue"` — inline sync work that cannot be interrupted; it
    may run to completion.
  - `"termination_in_progress"` — termination was requested but is not
    confirmed.
- `cancelled: false, reason: "task_not_found"` — the task already
  finished (its terminal was emitted and the entry reaped), the id sits
  at or below the high-water mark of handled calls, or the bounded
  pending set refused a new id. The host maps this to `completed`.

A `pending` ack therefore means "will be cancelled", never "already
finished" — the host reports `cancelled`, not `completed`.

## 4. Handshake and lifecycle

Host-side states (emitted to the UI as events):
`starting → ready → crashed → restarting → failed`, plus `unresponsive`
(liveness warning, §5) and `stopped` (clean terminal after `shutdown` —
host-side only, never emitted by the runtime).

`StateChanged.detail` is informational only — free text for humans and
logs. Clients must not match on its wording; the state machine is
`from`/`to`, nothing else.

1. Host spawns the process, state `starting`.
2. Runtime's **first stdout frame must be `hello`**:
   `{protocol: 1, runtime_version, python_version, exposes: [{name, kind, signature, doc}]}`
   - `kind`: `"sync" | "async" | "generator" | "isolated"`
   - `signature`: `{params: [{name, kind, required, schema}], returns: schema}`
     where `schema` is the annotation map defined in docs/codegen.md (the
     same schema `pylair codegen` consumes). `doc` is the function's
     docstring or null — used for generated JSDoc.
3. `HELLO_TIMEOUT = 30s` default. Settable via the
   `PYLAIR_HELLO_TIMEOUT_MS` env var (milliseconds) or
   `Builder::hello_timeout(Duration)`; every source is clamped to
   1s..=600s and a clamp is logged on `pylair://stderr`. No hello →
   kill → `crashed` → backoff → `restarting`. The failure detail names
   both configuration knobs.
   - **Slow-start hint.** At half the timeout the host emits one
     repeated `starting → starting` state event whose `detail` reports
     elapsed time — a spinner-message hook for UIs. It is not a new
     state, fires at most once per spawn attempt, never after `hello`,
     and (like all `detail`) is informational: clients must not depend
     on it arriving.
4. `protocol` mismatch → `failed` immediately (no restart — retrying
   can't fix a version skew). Error message names both versions.

After hello, bridge runtimes may keep emitting `app.component`
notifications while app-declared components warm up (decisions 0007):
a function declared `@expose(requires=…)` waits for its components to
report `ready` before the call body runs. None of that changes the
host's `Ready` verdict — the runtime is `ready` at hello; app
readiness is the app's own finer-grained signal, not a host state.

### 4.1 Web runtimes (wrap mode)

Wrap mode (`docs/wrap-mode-brief.md`, `docs/decisions/0003-wrap-mode.md`)
shows a Python web framework's own UI in the window. Its runtime is the
launcher `python -m pylair.wrap`, which speaks this same protocol and
supervises one web server. The host is configured for it with the `Web`
runtime kind (`SupervisorConfig::kind`, `Builder::runtime_kind`). The
default is the bridge kind, and nothing below applies to it.

- **`hello` still comes first** and is checked exactly as in §4 (same
  `protocol` rule, same `hello_timeout`). The launcher's `exposes` is `[]`
  and `kind` is `"web"`. `kind` is informational: the host's own
  configuration decides how it treats the runtime. A `call` to a web
  runtime answers `FUNCTION_NOT_FOUND` (nothing is exposed) and `cancel`
  answers `{cancelled: false, reason: "task_not_found"}`.
- **`ready {url}` is what makes a web runtime `ready`, not `hello`.** After
  `hello` the host stays in `starting` and waits for `ready` for at most
  `READY_TIMEOUT` (default **120s**, `SupervisorConfig::ready_timeout` or
  `Builder::ready_timeout`, clamped to 1s..=3600s with the clamp logged on
  `pylair://stderr`). A server that loads a model at import time
  legitimately takes minutes to answer, so this is separate from
  `hello_timeout`, which only covers the `hello`. No `ready` in time means
  kill, `crashed`, backoff, `restarting`, with a detail naming the knob. A
  shutdown requested while waiting is honoured at once, and calls issued
  while waiting fail fast with `RUNTIME_UNAVAILABLE` (a web runtime has
  nothing to queue them for).
- **The url is validated before it is trusted.** It must parse as `http`
  with the host exactly `127.0.0.1` and an explicit port, carry no
  credentials, contain no control character, whitespace or backslash, and
  share its origin with `http://127.0.0.1:<port>` (`same_origin`,
  parse-based; the control-character rule exists because the url parser
  silently drops a CR and reads a backslash as a slash, so a JSON-escaped CR
  that was never decoded would navigate to `/r`). `localhost`, `[::1]`, `https`, a missing
  port, `user@host` forms and `127.0.0.1.evil.com` are all rejected. A
  missing, non-string or failing url is a **protocol violation**, handled
  like a malformed frame: kill, `crashed`, restart policy. The url is the
  address the window should navigate to. The host never connects to it
  itself.
- **The current url is host state.** `Handle::ready_url()` (and a watch
  channel) holds it while the web runtime is `ready` or `unresponsive`,
  and it is cleared the moment the generation ends. `python_state`
  returns it for the web kind (the bridge kind still returns the bare
  state string), and every `pylair://state` payload carries `url` while
  one is set. A restart is a new process on a new port, so it is a new
  url: consumers navigate again and never cache one.
- **Health is the launcher's job, not the host's.** The host's liveness is
  the §5 ping, answered by the launcher's reader thread. It proves the
  process and the pipe, not the server. The launcher probes the server
  over HTTP (TCP connect, then `GET <ready-path>` expecting non-5xx) every
  10s starting one interval after `ready`, tolerates slow answers while
  the app hydrates, and exits **non-zero** only after 3 consecutive misses
  (exit code 5). Any server exit makes the launcher exit non-zero (code
  4), and a failed spawn or no free loopback port is code 3. The host sees
  an ordinary child exit: `crashed`, the §7 backoff and crash-loop
  breaker.
- **The server's stdout never reaches the frame channel.** The launcher
  performs the §1 hygiene on itself and starts the server with stdin
  closed and stdout and stderr both on the log stream, so the app's
  `print()`s and native writes to fd 1 become `pylair://stderr` lines.
- **Process tree.** On POSIX the server shares the launcher's process
  group (no `setsid`), so the host's group kill, and its group reap when
  the launcher exits on its own, reach it and anything it spawned. On
  Windows it stays inside the launcher's Job Object (§6).

## 5. Liveness

Supervision based on process exit alone misses the most common real
failure: a runtime **wedged** in native code (a C extension spinning, a
deadlock) never crashes, so it never restarts.

- The host sends `ping {seq}` every `PING_INTERVAL` (default 5s) through a
  dedicated single-slot path the writer drains ahead of data, so a
  saturated data lane cannot starve liveness and queued pings cannot
  crowd out real control frames. Only a ping actually written to the
  wire counts toward the miss accounting: one still waiting in the slot
  behind a stalled writer is backpressure, not a miss. The runtime
  answers with a `pong {seq}` notification within `PING_TIMEOUT`
  (default 10s).
- **Pongs are emitted by the reader thread, not the event loop.** A
  pong therefore proves the frame pipeline (and process) is alive —
  *not* that asyncio is making progress. Blocking code inside an
  `async def` (a very common AI-app pattern: model calls inside an
  async handler) starves the loop but must not fail liveness, or a
  healthy runtime would be killed on its first real workload.
- Event-loop lag is reported **separately as a diagnostic**: the
  runtime logs `event loop lag Ns` lines on stderr (forwarded to the
  UI via `pylair://stderr`) when the loop falls >1s behind. Calls
  queued behind a busy loop simply wait — same as any overloaded
  async service.
- A missed deadline → state `unresponsive` (event emitted). The host
  keeps pinging. `unresponsive` is entered **only after a real miss** —
  an in-flight ping below its deadline does not flap the UI state.
- `MAX_MISSES` consecutive misses (default 3) → treated as `crashed`:
  killed and the restart policy applies. Since pongs come from the
  reader thread, missing them means the *process* is truly hung
  (deadlocked native code, interpreter fault) — the correct thing to
  kill.
- **Transport health is not serving health.** The `pong` also carries
  `loop_stale_ms`: how long the runtime's asyncio loop has made no
  progress (measured by a heartbeat task on the loop, read by the reader
  thread). A stalled loop cannot serve calls or cancels, but a stall is
  not proof of a wedge — legitimate blocking work (loading a multi-GB
  model inside an `async def`) can stall it for minutes. The host
  therefore reports stalls past ~1s as a diagnostic (`Unresponsive`
  state), warns once per stall episode past `loop_stall_warn` (default
  5s — naming the likely cause and fix, plus a matching "recovered"
  line when the loop frees; suppressed while `health().app` reports a
  component `loading`) and keeps serving once the loop frees. Only a
  configured `loop_stall_kill` (**off by default**) treats a stall at or
  beyond that duration as wedged: in-flight calls fail with
  `RUNTIME_WEDGED` and the runtime is killed and restarted. The field is
  additive — hosts that ignore it keep the older behaviour.
- The `pong` also carries `pools` when the runtime has warm pools in
  use (decision 0008 §health): `{<fn>: {size, live, busy, idle, queued,
  deaths, crash_loop, idle_ttl_s, reaps}}` — one flat entry per
  warm-pooled function, replaced wholesale each pong. Additive like
  `loop_stale_ms`: hosts that ignore it keep the older behaviour. The
  host mirrors the latest report into `health().pools` (`null` until
  the first pong of a live generation; absent on the `web` kind) —
  reported, never host-measured, so it costs no round trip.
- `seq` matching guards staleness.
- **Host-side observability is additive.** The host exposes these
  liveness signals to webviews through the `python_health` command (a
  `health()` snapshot: verdicts, restart accounting, load totals) and
  the `pylair://health` event, emitted only when a transport or loop
  verdict *changes* — see `docs/decisions/0006-runtime-health.md`. The
  wire protocol is unchanged: `ping`/`pong` carry the same fields.
  `pylair://health` stays liveness-only: app-declared component phases
  (decisions 0007) arrive separately on `pylair://app`, emitted on
  phase transitions only — progress reads stay poll-only through
  `health().app`.

### 5.1 Event delivery (host → webviews)

Call data and lifecycle events use different mechanisms:

- **Call data is channel-bound — not an event.** `python_call_stream`
  requires an `onEvent` `tauri::ipc::Channel` argument and sends every
  stream item `{task, seq?, value?, done, result?|error?}` through it.
  Both client entry points ride this command: `call()` passes
  `stream: false`, `stream()` passes `stream: true` — the declared
  intent flows to the `call` request's `stream` field above, and the
  host accounts from it: `stream: false` takes a `max_concurrent_tasks`
  slot only (no `max_concurrent_streams` slot, no initial
  `stream.credit` grant, no stream-buffer accounting); `stream: true`
  is a stream as before. A plain call's terminal still arrives on the
  channel with the early task-id ack, so cancel, abort and
  `timeoutMs` deadlines behave identically. When `stream` is **absent**
  (a pre-flag client) the host accounts conservatively — stream slot +
  initial credit — but omits the field from the request, so the runtime
  applies no mismatch validation. `python_call` is the separate
  direct-`Value` command (no early task id, no channel): it always
  declares `stream: false`.
  The invoke itself answers `{task, protocol: 2}` — `protocol` is the
  client↔plugin bridge version (unrelated to the wire version above):
  an older plugin returns a bare task id and ignored `onEvent`, so a
  bare-number answer means "no channel delivery will arrive" and the
  client must fail loudly (and cancel the orphaned task) rather than
  hang. Delivery is bound to the invoking webview: a channel cannot be
  listened to by another window, replayed elsewhere, or forged from JS —
  there is no shared bus (the `pylair://stream` event was removed; see
  `docs/design/event-scoping.md` for the leak/forgery record). When the
  channel's consumer is gone (`send` fails), the forwarder reaps that
  task and stops — no stream slot or credit is left tied up, and
  same-owner siblings keep running.
  A webview **reload** kills the channel without reliably failing `send`
  (the eval lands in a dead JS context), so the plugin reaps the old
  document's tasks server-side at `on_page_load`/`PageLoadEvent::Started`
  (bounded by a task-id watermark — the new document's calls survive);
  the client's `pagehide` hook remains as best effort.
- **Broadcast lifecycle stays broadcast — except stderr.**
  `pylair://state` and `pylair://exposes` are `emit_to`-addressed at
  webviews that have invoked the bridge — routing, not a security
  boundary: any `Any`-target listener receives them regardless of the
  emit target. `pylair://stderr` is opt-in per window instead: the
  builder's `stderr_delivery` defaults to `Only(["main"])`, an explicit
  label list intersects with the subscriber set, and
  `StderrDelivery::All` restores broadcast. Deliberate `Any`-target
  listeners still see emitted stderr lines (same residual as the other
  broadcasts — delivery selection, not sniffing protection). A
  destroyed window is removed from the subscriber set.
- **State events are lossless for the latest state.** `pylair://state`,
  `pylair://exposes`, `pylair://health` and `pylair://app` are state,
  not traffic: they travel per-kind latest-state slots beside the
  bounded events channel, so bulk pressure (a stderr flood) can never
  drop one. Re-entries into the SAME state may coalesce under load —
  repeated reports for one component on `pylair://app`, or a crash
  loop's hundredth `Starting` — but distinct lifecycle transitions are
  distinct facts and always arrive in order, and the newest state
  always reaches every subscriber: a webview can never be stranded on a
  superseded readiness or liveness verdict, and a newer generation's
  report retires an older one's undelivered entries. Ordering across
  kinds follows store order. `pylair://stderr` stays lossy by design —
  a flooded log drops lines with a `[dropped N lines]` marker, never
  states — and the supervisor never blocks on either path.

- `cancel()` waits at most `cancel_timeout` (default 5s) for the runtime's
  acknowledgement and then fails with `RUNTIME_WEDGED`; a blocked runtime
  cannot be relied on to answer. It resolves to a distinct outcome —
  `cancelled`, `requested_may_continue` (inline sync work that cannot be
  interrupted and may run to completion), `terminated` (isolated worker
  killed and reaped), `termination_uncertain`, `completed`,
  `not_found` (no such task in this runtime generation), `rejected`
  (caller doesn't own the task), or `delivery_failed` — never an
  ambiguous boolean. The client's `timeoutMs` call option is
  an application-level deadline: it cancels the task and rejects with the
  client-side code `-32013` (`DEADLINE_EXCEEDED`) without waiting for that
  acknowledgement.
- **Task ownership is supervisor-enforced.** The supervisor records
  `{task → call id, owner webview}` per runtime generation; `cancel` and
  `stream.credit` from a webview that doesn't own the task are rejected
  (outcome `rejected` / `INVALID_REQUEST`), and a task id from a dead
  generation resolves `not_found` — it can never control a new task. The
  owner is always the Tauri-reported window label, never a JS-supplied
  argument. Destroying a window cancels all of its tasks.
- **Control frames have their own lane.** `cancel`, `stream.credit` and
  `shutdown` are queued on a small dedicated channel the writer drains
  before data frames — a stream flood can fill the data lane but cannot
  starve or silently drop control; a failed queue send surfaces as an
  explicit failure. `ping` is different again: a single latest-wins slot
  drained ahead of everything, so repeated pings can never occupy the
  finite control capacity that cancel/credit/shutdown need. Control can also *outrun* its call, so the runtime
  records cancels/credit for not-yet-registered task ids and applies
  them at registration.
- **Runtime-side inbound bound.** The runtime counts data frames read
  but not yet dispatched (`PYLAIR_IN_MAX`, default 4096); a data frame
  over that bound is rejected with
  `LIMIT_EXCEEDED{limit:"inbound_queue"}` if it carries a request id,
  or dropped with a log line if not - the reader never waits, so a
  control frame later in the pipe is never stuck behind queued data.
  Inbound control (`cancel`, `stream.credit`, `shutdown`) always
  enqueues and sorts ahead of queued data, so a flood cannot starve
  the cancel that is meant to stop it.
- `shutdown()` resolves to what cleanup achieved — `terminated` (grace
  window exit), `forced_kill` (tree confirmed dead after a hard kill),
  `incomplete` (kill sent, exit unconfirmed), or `unknown`.

## 6. Orphan prevention

"No child process outlives the host" needs an actual mechanism, per
platform:

- **stdin EOF is the portable guarantee.** The runtime's frame loop
  exits when stdin reaches EOF (host dead → pipe closed), and it kills
  its isolated workers first. Workers likewise exit on their own
  control-channel EOF. The host therefore only has to *stop* cleanly;
  if it dies, everything downstream notices within one pipe read.
- **Windows:** "process group" doesn't exist — the host puts the
  runtime in a **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`;
  when the host's last job handle closes (clean or crash), the kernel
  kills everything in the job. The runtime is created suspended
  (`CREATE_SUSPENDED`), assigned to the job, and only then resumed, so
  it executes no instruction (and spawns no descendant) outside the job;
  see `docs/design/windows-job-object-race.md`. Isolated workers are
  not given a job of their own: they inherit the runtime's, and a
  worker cancel uses `taskkill /T`.
- **POSIX:** the runtime gets its own process group and the supervisor
  kills `-pgid` — both on a forced kill and when the runtime exits on its
  own (so group members cannot outlive a dead leader). Isolated workers
  run in their *own session* (so one task can be cancelled by killing
  exactly its group), which a kill of the runtime's group cannot reach.
  Each worker therefore watches its parent and, when re-parented,
  SIGKILLs its whole group (worker plus any grandchildren, SIGTERM
  ignorers included). On Linux both levels also get a kernel backstop:
  the runtime is spawned with `PR_SET_PDEATHSIG = SIGKILL` (the kernel
  kills it even when the host dies before any cleanup can run), and
  each worker sets `PR_SET_PDEATHSIG` on itself at startup — covering a
  worker wedged in native code holding the GIL, which could stall the
  Python watcher thread. There is no parent-death signal on macOS:
  there, a host kill is covered only by stdin EOF + the worker's ppid
  watcher — a wedged *runtime* on macOS is the accepted gap.
- **Shutdown completes before it is acknowledged.** `shutdown()` resolves
  only after the runtime's process tree has been reaped, and the plugin
  blocks application exit on it (`RunEvent::Exit`). A shutdown requested
  during a restart backoff is honoured immediately.
- **Windows without a Job Object:** if assignment fails the host says so
  on the stderr event stream and falls back to `taskkill /T` on kill.

## 7. Supervisor policy (host-side, not on the wire)

- Restart backoff: exponential, `250ms → 8s` cap, `×2` factor, `±20%`
  jitter.
- Crash-loop breaker: **5 restarts in 60s → `failed`**. `failed` requires
  an explicit UI/app action (a "retry" path is allowed, it resets the
  counter).
- In-flight calls when the process dies → immediately fail with
  `RUNTIME_CRASHED`; they never hang waiting on a dead pipe. Whether a
  call is retried is the app's choice — the supervisor never replays
  calls (no idempotency guarantees in Python user code).
- Graceful `shutdown` → `grace_ms` deadline → SIGTERM → 5s → SIGKILL.
  The host sends `grace_ms` = its own kill grace minus ~1s (floored at
  500 ms), so the runtime's drain always finishes before escalation.
  The runtime treats `grace_ms` as ONE deadline shared across every
  shutdown phase - stop accepting, task cancel, isolated-worker kill
  waits, writer flush, close - never a fresh timeout per phase. A host
  that sends no params gets the runtime's 5000 ms default.
- **A cleanup step that can itself hang is skipped, and the runtime
  exits the process instead of performing it.** If the writer is still
  inside `write()` when its budget expires, the channel is dead —
  closing it can block on the pending write (Windows, #142) — so the
  runtime leaves the handle open and exits; the OS reaps it. The same
  exit covers executor threads still running cancelled work, which the
  loop teardown would otherwise join unbounded (3.12) or for 300 s
  (3.15). Nothing is lost: the host is already gone, so no frame could
  be delivered anyway.
- Calls issued while the runtime is *restarting* (crash backoff) or
  *failed* fail fast with `RUNTIME_UNAVAILABLE`; calls issued while it is
  merely *starting* queue until the handshake completes.
- A stream consumer that stops draining (buffer full) or disappears ends
  with an explicit `INTERNAL` terminal error and the Python producer is
  cancelled. The terminal result is delivered on its own slot, so it is
  never lost to a full chunk buffer.

## 8. Error codes

JSON-RPC reserved plus app-specific, all in `-32000..-32099`:

| code | name | when |
|---|---|---|
| -32700 | PARSE_ERROR | malformed JSON / bad frame |
| -32600 | INVALID_REQUEST | wrong shape, missing fields |
| -32601 | FUNCTION_NOT_FOUND | `fn` not in `@expose` registry |
| -32602 | INVALID_PARAMS | input fails signature validation |
| -32000 | INTERNAL | uncaught Python exception (`data.type` = class) |
| -32001 | TASK_NOT_FOUND | cancel/shutdown for unknown `task` |
| -32010 | TASK_CANCELLED | task ended via cancel (also stream terminus) |
| -32011 | WORKER_DIED | isolated worker subprocess exited non-zero |
| -32012 | RUNTIME_WEDGED | liveness deadline exceeded `MAX_MISSES` times |
| -32030 | RUNTIME_UNAVAILABLE | host-side: not `ready`, restarting, or dead |
| -32031 | RUNTIME_CRASHED | process died mid-call |
| -32032 | PROTOCOL_VIOLATION | garbage on frame fd / bad hello / credit overrun |
| -32033 | RUNTIME_RESTARTED | in-flight call killed by a dev-mode hot reload |
| -32040 | LIMIT_EXCEEDED | a configured resource limit was hit |
| -32050 | PERMISSION_DENIED | host-side: the window's capability does not permit `fn` (never reaches Python) |

`LIMIT_EXCEEDED` errors carry `data: {"limit": "<name>", "max": <n>}`
naming which cap fired — e.g. `"max_concurrent_tasks"`,
`"stream_buffer_bytes"`, `"max_stream_credit"`, `"max_frame"`,
`"max_depth"`, `"inbound_queue"`. Clients
surface it as `LimitExceededError` with `.limit`/`.max`. Limits are
enforced per runtime on the supervisor's `Limits`
config; defaults are env-overridable via `PYLAIR_MAX_*` and the
effective set is logged at startup.

Four caps are additionally split per caller (per-window fairness,
decisions 0005): `max_queued_commands`, `max_concurrent_tasks`,
`max_concurrent_streams`, and `max_isolated_workers`. While other live
windows hold fewer than their guaranteed minimum, a caller that reaches
its share ceiling has new `call`/`callStream` requests refused *before
anything is committed*: before the command enters the queue for the
queue cap, and before a task id or stream slot is allocated for the
task, stream, and isolated-worker caps. The error carries
`data: {"limit": "window_share", "max": <ceiling>, "window": "<label>"}`.
`window` is the webview label, or `null` for host-side callers, which
follow the same rule only while they hold queued or in-flight work. A
caller alone keeps each entire resource and still sees
`"max_queued_commands"` / `"max_concurrent_tasks"` /
`"max_concurrent_streams"` at the global cap. For isolated workers the
host adds no global cap of its own: which functions count comes from
the runtime's `exposes` manifest each generation (an undeclared
function counts as non-isolated), and Python's `max_isolated_workers`
remains the authoritative global backstop, so a lone caller's outcome
is unchanged. The check is admission-only, so work already queued or
running is unaffected, and `PYLAIR_PER_WINDOW_FAIRNESS=0` disables it.
This is host-side behavior and does not change the wire protocol;
`stream_buffer_bytes_total` stays a global backstop by design
(decisions 0005 §5.1).

`INTERNAL` also covers a `requires=` gate failure (decisions 0007): a
call to a function declared `@expose(requires=…)` whose named
component reached `failed` answers `INTERNAL` with
`data: {"component": "<name>", "phase": "failed"}` — the call body
never ran. Cancelling such a call while it waits on the gate is an
ordinary `TASK_CANCELLED`, not an INTERNAL.

`PERMISSION_DENIED` is raised by the host's `python_call` and
`python_call_stream` commands when the calling window's capability scope
(`docs/security.md`, "Restricting functions per window") does not permit
`fn`. The message names the function and window and the error carries
`data: {"fn": "<name>", "window": "<label>"}`. The check runs before the
window is subscribed, a task id is allocated, credit is taken or anything
is sent on the call's channel, so a denied call consumes no task id and
produces no frames to Python; the host-to-Python wire is unchanged.
Clients surface it as `PermissionDeniedError` with `.fn`/`.window`.

## 9. Concurrency and ordering

- Multiple `call`s may be in flight; `task` ids are unique per process
  lifetime and never reused.
- `stream.chunk` ordering is guaranteed **per task only** (seq-ordered);
  chunks from different tasks may interleave.
- Flow control is credit-based (§3.1), not buffer-cap-based — producers
  are paused, never aborted for streaming "too much". The runtime's
  outbound queue (`OUT_MAX_BYTES`, default 64 MiB) remains only as a
  host-not-draining tripwire.

## 10. Versioning

- `protocol` is a single integer. Host accepts exact match only in v1
  (a minor-version field can be added when there's a second version to
  talk to).
- Unknown top-level fields are ignored both directions (forward
  compatibility within a major version).
- The Python runtime refuses to start for an incompatible host the same
  way (host sends its version in an env var `PYLAIR_PROTOCOL=1`).

## 11. What this does not cover (yet)

- Type/schema details of `exposes[].signature` — docs/codegen.md.
- Cancellation *guarantees* per execution mode — `docs/cancellation.md`.
- Bulk binary transport — shipped as `pylair.Blob` handles and
  `pylair-blob` URLs (`docs/blobs.md`, decision 0010); `bytes` still
  base64 in JSON.
- Warm/model-persistent isolated workers — the `worker` option on
  `@expose` is reserved now (see `docs/cancellation.md`); v1 isolated
  workers are per-call.
- Auth/attestation of the child — the child is spawned by the host with
  stdio wired directly; nothing else can write its frame fd by design.
  Threat model in `docs/security.md`.
