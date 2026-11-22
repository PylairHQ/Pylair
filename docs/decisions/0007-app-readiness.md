# 0007: App-defined readiness

Status: accepted, implemented (PRs #128-#131, #132)

## Question

The host already reports *its own* view of the runtime — transport and
loop liveness, generation, restart accounting (`health()`/`onHealth`,
decisions 0006). What it cannot say is *"the app finished loading its
model and can serve"* — that signal lives only inside the app
(`docs/ideas.md` → "App-defined readiness"). Today a developer has two
ways to load something heavy, both wrong:

- **Load at import.** `python -m pylair` imports the user module
  *before* sending hello (`__main__.py:28-29` —
  `importlib.import_module(module)` runs before `run(channel)`, and
  hello is the first frame `run()` emits, `_server.py:546`). A 45s
  model load at import time eats the whole `hello_timeout`
  (`HELLO_TIMEOUT_DEFAULT_MS = 30_000`,
  `supervisor/config.rs:148`) and the runtime is declared
  `crashed` (`handshake.rs:71-76`) — the window saw only `starting`.
- **Load inside the first call.** The import is fast, hello makes it,
  but the first `call` blocks the loop for the whole load: pongs keep
  flowing (they are answered on the *reader* thread, `_server.py:256-266`)
  with `loop_stale_ms` climbing, so `health()` honestly reports
  `liveness.loop: "busy"` — and still cannot say *why*.

This decision designs the missing middle: a post-hello startup hook
plus an app-reported readiness surface the webview can render.

## Evidence base

- **Startup order today** (bridge kind): spawn → module import
  (`__main__.py:28`) → `run(channel)` → writer thread starts →
  `hello` (`_server.py:546`) → reader thread starts → dispatch loop
  (`_server.py:551-558`). Hello → `Ready` is immediate for bridge
  (`handshake.rs:124-126`, `set_state(State::Ready, "handshake complete")`);
  there is no "loading" state for bridge today.
- **The post-hello precedent exists already — on the `web` kind.**
  `handshake.rs:109-122`: a web runtime waits `hello` → `ready {url}`
  under `ready_timeout` (`handshake.rs:138-144`), serving the command
  channel meanwhile — `shutdown` honoured at once, calls
  `fail_command_fast(c, "web runtime is starting")` (`:199-215`).
  `pylair.wrap` sends `proto.notification("ready", {"url": url})`
  (`wrap/_launcher.py:546`). The wrap launcher *is* an app-defined
  readiness reporter for exactly one component (the HTTP server).
- **`_send` is callable from any thread** (`_server.py:163-216`): it
  serializes on the caller's thread and `put_nowait`s into
  `_BoundedOut` (`_server.py:91-103`), drained by the dedicated
  writer thread (`_writer`, `:220-233`). A report leaves even while
  the event loop is blocked — the exact property a "loading model"
  reporter needs when the load itself holds the loop.
- **Unknown notifications are ignored host-side**
  (`session.rs:415` — `_ => {} // unknown notifications are ignored
  (spec §8)`), and `protocol-v1` §10: unknown top-level fields are
  ignored both directions. An additive notification needs no version
  bump and is a no-op on an old host.
- **Unthrottled reporting can self-kill.** `_send` on a full `_out`
  queue logs "host not draining" and drives the drain-and-exit path
  (`_server.py:199-216`). `OUT_MAX_FRAMES` defaults to ~6208
  (`_server.py:41-48`). A progress loop calling a report API at
  1000/s would fill it and shut the runtime down — so coalescing
  *before* `_send` is a correctness requirement, not politeness.
- **Task machinery already gives cancellable waits.** `_call`
  registers the call id before dispatch (`_server.py:440`,
  `note_call_id`); a `cancel` that arrives before a task starts is
  recorded in `_pending_cancels` and honoured at `start()`
  (`_tasks.py:148-155`). A wait implemented *inside* the task
  coroutine gets `TASK_CANCELLED` for free (`_tasks.py:163-169`).
- **Fail-fast during not-ready windows is established.**
  `fail_command_fast` runs for `restarting` (`run.rs:353`),
  `stopped` (`run.rs:513`) and pre-`ready` web runtimes
  (`handshake.rs:215`).
- **The health surface to extend.** `Handle::health(owner)`
  (`handle.rs:197-311`) builds `HealthSnapshot` (v1, additive per its
  doc comment — `index.ts:692-693` "version 1 may gain fields");
  `python_health` serializes it (`commands.rs:282-288`);
  `RuntimeEvent::HealthChanged` → `pylair://health` →
  `broadcast_targets()` (`builder.rs:217,227`). Per-generation
  liveness resets via `HealthShared::reset_generation`
  (`health.rs`); the app map belongs to the same generation scope.

## 1. Startup order and the post-hello hook

**New API (Python):** `@pylair.on_startup`, a decorator registering a
startup hook. Any number may be registered; they run in registration
order, each exactly once per generation.

```python
import pylair

@pylair.on_startup
async def warm():
    pylair.loading("model", detail="loading checkpoint")
    model = load()
    pylair.ready("model")
```

**Where it runs.** Inside `run()`, after the reader thread starts and
*concurrent with* the dispatch loop — a task spawned beside `lag` and
`beat` (`_server.py:548-549` precedent), not awaited before
`while not self._stopping`. The dispatch loop therefore serves `call`,
`cancel`, `shutdown` normally while hooks run: queued calls reach
`_call`, `requires=` gates apply (§2), and a shutdown mid-load is not
held hostage by the loader. A hook that ran *before* the dispatch loop
would pile `call`s in `_in` exactly like today's load-inside-call,
just moved.

**Async vs sync.** Follow the `ExposedFn` kind precedent
(`_expose.py:27-34`): `inspect.iscoroutinefunction` picks the runner.
An `async def` hook becomes an `asyncio` task — while it holds the
loop, `loop_stale_ms` climbs honestly (`busy`), which is *correct* for
a hook that blocks. A `def` hook runs on the default executor
(`loop.run_in_executor`, the `_run_sync` precedent at
`_tasks.py:178-186`) — model loads are usually sync/blocking, so this
is the common path and the loop stays `ok` while components report
`loading`.

**Contract:**

- *Exceptions* become component failures, never a crash. Each hook
  runs under a `contextvars.ContextVar` carrying the set of
  component names it touched: every `loading()`/`ready()`/`failed()`
  call records its name in the current context's set (§4's registry).
  A hook that raises marks every touched component still in
  `loading` as `failed` with the exception as `detail`; a hook that
  touched none marks `failed("startup:<fn.__name__>")` instead.
  Sync hooks run through `contextvars.copy_context().run` inside
  `run_in_executor` — executor threads do not propagate context, so
  without the copy a sync hook's reports would land unattributed.
  The runtime keeps serving (the app decides whether the failure is
  fatal — e.g. every `requires=` call on the dead component then
  fails fast per §2). A hook does not block later hooks.
- *End-of-startup sweep*: when every `on_startup` hook has finished
  (success or failure), every declared component still not
  `ready`/`failed` is marked `failed` — `requires=` declarations and
  reported-`loading`-but-never-completed alike — so no gated wait
  can hang past the end of startup. Detail is
  `"never reported by a startup hook"` for a component with no
  reports at all; a component that reported `loading` but never
  completed gets `"startup hook finished without reporting ready"`.
  With *zero* hooks the sweep fires at startup end — i.e.
  immediately — so `requires=` on a component no hook will ever
  report is an app bug surfaced at once, not at first call. (The
  escape hatch is simply not declaring `requires=`: a lazy
  load-inside-call still works and can report progress as it goes.)
- *Shutdown*: hooks draw from the same shared `_deadline` the drain
  uses (`_server.py:385-390`). An async hook is cancelled at drain;
  a sync hook on an executor cannot be interrupted — same
  inline-sync rule as user functions (`docs/cancellation.md`): its
  result is discarded and shutdown proceeds.
- *Dev restart*: `pylair dev` touches the trigger file, the
  supervisor ends the generation and respawns (`run.rs:557-588`,
  `ExitCause::DevRestart`, `health.rs:31-32`). The new process
  reimports the module, hooks re-register and re-run — no special
  signal needed. The host-side component map resets with the
  generation (§4).

## 2. Calls that arrive during loading

**Chosen: opt-in `@expose(requires=(name, ...))` that waits, resolved
entirely Python-side.**

```python
@pylair.expose(requires=("model",))
def chat(prompt: str): ...
```

`_call` sees a non-empty `requires` on the resolved `ExposedFn` and
starts the task through a gated runner: the task coroutine first
`await`s the component-ready condition, then runs `fn`. The wait is
*inside* the task, so `note_call_id` has already run (`_server.py:440`)
and a `cancel` lands through the ordinary path — the caller's
`AbortSignal`/`timeoutMs` is the only bound needed (matches
`docs/cancellation.md`: cancellation is always caller-driven).

**Declaration at decoration closes the first-call race.** Hooks run
*concurrently* with dispatch, so components otherwise spring into
existence only when a hook's first `loading()` reports — a
`requires=("model",)` call arriving between hello and that first
report would see "unknown component" and error, which is precisely
the call the gate exists to serve. Instead every `requires=` name
**declares its component at decoration time**: the decorator enters
it in the readiness registry in phase `loading`, during the import
that already precedes hello. At dispatch a `requires=` name is
declared by construction — the call waits, which is the designed
semantics. The only remaining dispatch-time app error is a
`requires=` name with *no declaration path* — a name that cannot be
a component at all (not a non-empty string, or a registry mutated
past it): an app bug that errors loudly, never hangs.

- Component `ready` at dispatch → the wait returns immediately (no
  latency added to steady-state calls — one set lookup).
- Component `failed` (already failed at dispatch, or fails mid-wait)
  → immediate `INTERNAL` error with `data: {"component": "<name>",
  "phase": "failed"}` — the `data` channel is the established
  extension point (`data.type`, `data.limit`; protocol §8), so **no
  new error code**.
- Component `loading` (declared, reported, or swept) → the call
  waits; the end-of-startup sweep (§1) guarantees a terminal phase,
  so the wait can never be permanent; the caller's timeout/cancel
  may still end it earlier as `TASK_CANCELLED`.

**The wait still costs a slot.** A waiting `requires=` call is a
started task: it holds a `max_concurrent_tasks` slot and counts
toward the caller's window share for the whole wait — the host
charges the task when it forwards the call and cannot see inside the
gate (fairness `in_flight`/queue cells, decisions 0005). For
`isolated` functions the gate sits *before* the worker spawn inside
`_run_isolated` (`_tasks.py:270`): no worker is created until every
required component is ready, so a waiting isolated call holds its
isolated share without paying for a live worker. A component that
never arrives would burn that share for the wait's duration — one
more reason the sweep's guaranteed `failed` is a correctness rule,
not a convenience.

**Rejected:**

- *No gating:* works today and stays the default — but the common
  case (the chat function needs the model) still hangs silently, and
  the UI can only hint via `health().app`. Opt-in gating fixes the
  calls that need it without changing anything else.
- *Fail-fast for every not-ready call:* the host already does this
  for `web` pre-`ready` (`handshake.rs:215`) and restarting
  (`run.rs:353`), so it is defensible — but it pushes a poll/retry
  loop onto every caller for the *expected* slow-start case. Waiting
  is the friendlier default for a condition the app itself declared;
  failure is reserved for `failed`, where waiting is hopeless.
- *Host-side gating:* requires shipping the `fn → components` map in
  the manifest and host-side readiness state just to re-implement a
  one-line `await` — the Python side already owns the registry.
  (Manifest `signature.meta` can still carry `requires` later so
  codegen'd clients can render "waiting on model" — optional, not
  required for correctness.)

## 3. Wire shape and compatibility

**One additive notification, no protocol version bump** (§10: unknown
fields/frames ignored both directions; `session.rs:415` precedent):

```json
{"jsonrpc": "2.0", "method": "app.component",
 "params": {"name": "model", "phase": "loading",
            "progress": 0.4, "detail": "checkpoint 4/10"}}
```

- `phase` ∈ `"loading" | "ready" | "failed"`. `progress` optional,
  a float in `[0, 1]` — absent means indeterminate. `detail`
  optional free text. Both fields may be absent on every report.
- The name follows the dotted `stream.*` vocabulary
  (`stream.chunk`, `stream.credit`) rather than the flat `ready` —
  `app.*` leaves room for future app-originated notifications.

**Compatibility:**

- *Old host + new Python:* the notification is ignored
  (`session.rs:415`); `health().app` is absent; nothing else changes.
  The reporting API must therefore never *require* an ack — it is
  fire-and-forget, which the notification shape already is.
- *New host + old Python:* no reports ever arrive; `health().app` is
  `null`; the `pylair://app` event never fires. Same fallback as
  pre-rc.19 `health()` — the selftest gates on the resolved `pylair`
  version (§7).
- *Dev reload:* new process → new generation → the component map is
  cleared by `reset_generation`, hooks re-run and re-report, and
  `generation` on each event disambiguates stale deliveries.

**protocol-v1.md sections that change:** §3 (message shapes — add
`app.component` to the notification vocabulary and its param
schema/bounds), §4 (a short note: bridge runtimes may report
app-component phases *after* hello; the host's `Ready` verdict is
unchanged), §5 (a line that `pylair://health` remains liveness-only
and app phases arrive separately — the same kind of note 0006 PR 5
added), §8 (a `data.component` mention on `INTERNAL` for the
`requires` failure — no new code row). No §7/§9/§10 change.

## 4. Coalescing and bounds

**Python side — a `_Readiness` registry, not a per-call `_send`.**
`loading(name, progress=None, detail=None)`, `ready(name)`,
`failed(name, detail=None)` update in-memory component state
(latest-wins per component) and mark it dirty. A single flusher
daemon thread (the `_writer` precedent, `_server.py:541`) emits the
coalesced state — at most one `app.component` frame per component
per interval (default 250 ms, `PYLAIR_READINESS_FLUSH_MS`), *plus*
an immediate flush on any phase transition (`→ready`, `→failed` are
too important to wait for the tick). A hot progress loop therefore
costs one frame per interval — the `_out` tripwire
(`_server.py:199-216`) can never be reached by reporting alone.
Callable from the loop or any thread: state writes are a single dict
store under a `threading.Lock`; `_send` is already thread-safe.
The registry is module-level (the `_registry` precedent,
`_expose.py:17`), so reports made *before* the server exists —
`loading()`/`ready()`/`failed()` at import time — are **kept, not
dropped**: `run()` hands the accumulated state to the flusher and
the first post-hello flush carries it, coalesced like anything else
(an import-time `loading()` followed by `ready()` sends only
`ready`). Reports after shutdown still record silently — never
raise, since the caller cannot do anything about a report anyway.
The registry also carries `requires=` declarations (§2): a
declared-but-unreported component exists Python-side as `loading`.
A declaration is **not** a stream of frames — but it is sent
exactly once: the first post-hello flush emits one `loading`
`app.component` frame per declared component, so `health().app`
shows `"model": "loading"` from the start instead of nothing until
a hook reports. Latest-wins still rules that flush: a component a
hook already reported `ready`/`failed` skips the declared
`loading` shot — it fires only for declarations with no report.
After it the component follows normal reporting, and the sweep
still turns a never-reported declaration into `failed`.

**Host side — bounds in the notification arm** (`session.rs`): the
component map is per-generation, stored in `HealthShared` beside the
other liveness fields, cleared by `reset_generation`.

- ≤ **32 component names** per generation; new names beyond the cap
  are dropped (existing names still update) — a buggy app cannot
  grow host memory.
- `name` truncated at **64** chars, `detail` at **256** chars —
  app-supplied free text, same treatment as `last_exit.detail`
  ("informational only, never parse it", `index.ts:655-656`).
- `progress` outside `[0, 1]` or non-finite → treated as absent
  (`null`), never a violation: telemetry from the app must not be
  able to kill the runtime that carries it — the same reason a
  malformed *notification* drops the frame, not the loop
  (`_server.py:325-326` comment precedent).
- The stored record is `{phase, progress, detail, at: Instant}`;
  `health().app` renders `at` as `ms_ago`, matching
  `last_pong_age_ms` / `last_exit.ms_ago` conventions.

**What the webview may display safely:** `name`/`detail` are plain
informational strings — render as text only (React's default
escaping; the `last_exit.detail` rule applies verbatim). `progress`
drives a determinate bar only when non-null; `phase` drives
labels/icons. None of it is trusted input for logic — an app that
lies about its own readiness hurts only itself.

## 5. Host and client surface

**`health().app` — additive v1 field** ("version 1 may gain fields",
`index.ts:692`):

```ts
app: {
  components: Record<string, {
    phase: "loading" | "ready" | "failed";
    progress: number | null;
    detail: string | null;
    ms_ago: number;
  }>;
} | null
```

`null` when the generation has reported nothing and on the `web`
kind (§6) — the only two absence cases. `app` is runtime-wide app
state, not caller-scoped, so unlike `self` it does not depend on
`owner`: `Handle::health(None)` — a host-side read — includes it.

**Event: a new `pylair://app`, not folded into `pylair://health`.**
`RuntimeEvent::AppChanged` → `events::APP = "pylair://app"` →
`broadcast_targets()` (the `builder.rs:217,227` wiring precedent),
payload `{version: 1, generation, name, phase}` — emitted only on
**phase transitions**, never on progress ticks. Rationale: the
`pylair://health` contract is "emit on a liveness-verdict change"
(`health.rs:80-89` emitted-baseline machinery); app phases are a
different cadence and a different consumer (a loading UI cares about
component transitions, not transport verdicts). Progress stays
poll-only via `health().app` — an event per progress tick would be
the spam the whole design exists to avoid. No new ACL: the event
rides the subscribed-webview broadcast set like `pylair://health`;
there is no command to deny.

**Rust shape:** `HealthShared` gains
`app: BTreeMap<String, AppComponent>` (+ the 32-cap and truncation
rules of §4); the `app.component` match arm in `session.rs` writes
it and `try_send`s `AppChanged` on a phase transition;
`Handle::health` serializes `app` into the snapshot;
`reset_generation` clears it. `HealthSnapshot`/`HealthChange` TS
types gain `app?` / nothing respectively; `AppChange` +
`onApp(cb): Promise<() => void>` join `EVT`/`CMD` tables and the
default export (`index.ts:159`, `:743-745` precedent).

**Selftest: yes, prove a real round trip.** The installed-webview
selftest exists precisely to prove the shipped artifact end-to-end
(the `health` check precedent, `selftest.ts:353-354`). The selftest
app registers an `on_startup` hook that reports
`loading("selftest", 0.5)` then `ready("selftest")`; the check
asserts `health().app.components.selftest.phase === "ready"`, one
`pylair://app` phase event arrived through `onApp`, and the
unlisten fn works. Gate: this is the first feature that needs the
*Python* package too — presence-gate on the resolved `pylair`
version (the API ships in it) *and* the resolved crate (the host
arm), mirroring the `PYLAIR_HEALTH_EXPECTED` version-gate pattern
(`acceptance-registry.yml` capability step).

## 6. Wrap mode

**Out.** A wrap runtime exposes nothing
(`wrap/_launcher.py:514-523`) and its readiness is already the
post-hello `ready {url}` the launcher sends once the server answers
HTTP (`_launcher.py:546`, `handshake.rs:144`) — the app-defined
signal is the HTTP probe itself. The wrapped app is a *separate
process* the launcher spawns; it cannot `import pylair` to report
through the frame channel. `health().app` stays `null` on `web`
kind, same rule as `loop_stale_ms`/`loop` being absent/`"unknown"`
there (`index.ts:627-629`) — never fabricate a signal the runtime
cannot produce. If a future wrap variant ever embeds a bridge
runtime, the feature applies unchanged — nothing here is
bridge-only by design, only by where the reporter lives.

## 7. Test plan, PR split, release impact

**Python unit tests** (the bulk of the feature):

- `loading()`/`ready()`/`failed()` update the registry; flusher
  emits coalesced latest-wins (two loads in one interval → one
  frame); phase transitions flush immediately; rate cap holds under
  a 10k-call loop (no `_out` growth).
- Reporting from a plain thread and from the loop both land.
- `on_startup` ordering: hook runs *after* hello, concurrent with
  dispatch — a `call` dispatched mid-hook is answered, not queued
  behind it.
- Sync hook runs on an executor; async hook on the loop; exception
  attribution — touched-but-`loading` components → `failed` with the
  exception detail, untouched → `failed("startup:<fn.__name__>")`;
  the sync path attributes identically through `copy_context`
  (proves executor-context propagation); a second hook still runs
  after the first fails.
- The sweep: all hooks done with a declared component still
  `loading` → `failed`/`"never reported by a startup hook"`;
  reported-`loading`-never-completed → `failed` with the
  incomplete-hook detail; zero hooks → the sweep is immediate.
- `requires=` declares at decoration: a `requires=("model",)` call
  dispatched right after hello — before any hook ran — waits in
  `loading`, never sees "unknown component"; the wait survives a
  `cancel` as `TASK_CANCELLED`; a `ready` component short-circuits;
  `failed` (including the sweep's) → `INTERNAL` + `data.component`;
  a `requires=` name with no declaration path is a dispatch-time
  app error.
- A waiting `requires=` call holds its task slot and window share
  for the wait (assert against the registry's accounting); an
  `isolated` `requires=` wait spawns no worker until ready.
- Reports before the server exists are kept: `loading()` then
  `ready()` at import produce exactly one `ready` frame in the
  first post-hello flush.
- Declared components ride that first flush too: a `requires=` name
  with no report emits exactly one `loading` frame post-hello (one
  shot, once) so `health().app` shows it loading from the start; a
  component already reported `ready`/`failed` does not get a second
  `loading`.
- Shutdown mid-hook: async hook cancelled; sync hook doesn't block
  the shared deadline.

**Rust tests (fake runtime):** an `app.component` notification
lands in `health().app` with the right shape; phase transitions
emit exactly one `pylair://app` each; 33rd component dropped;
over-long name/detail truncated; out-of-range progress → `null`;
`reset_generation` clears the map and a new generation re-reports
cleanly; a runtime that never reports yields `app: null`.

**Client mocks:** typed `app` block reads; `onApp` subscribe →
deliver → unlisten; absent `app` (old runtime) tolerated.

**Selftest:** real hook + report + `health().app` + `onApp` +
unlisten, version-gated as in §5.

**PR split** (mirroring the health train):

1. **Python** — `_readiness.py` + public API + `requires=` gate +
   unit tests (touches `_server.py` run order, `_expose.py`
   options, `__init__.py` exports).
2. **Rust** — `app.component` arm, `HealthShared.app`, snapshot
   field, `AppChanged`/`pylair://app`, fake-runtime tests.
3. **Client** — `HealthApp`/`AppChange` types, `onApp`, default
   export, mocks, README section.
4. **Selftest + docs** — the selftest check + gating,
   protocol-v1 §3/§4/§5/§8 notes, README capability bullet,
   `ideas.md` entry retired, this record → `accepted, implemented`.

**Release impact:** all three registries this train — `pylair` on
PyPI carries the API and the wire notification, the crate carries
the host arm + snapshot/event, npm carries the client types and
selftest. (Runtime health skipped PyPI; this one cannot.) The
registry-acceptance gate must check the resolved `pylair` *and* the
resolved crate — first feature where both sides of the bridge gate
a self-test check.
