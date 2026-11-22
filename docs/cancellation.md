# Cancellation semantics

Cancellation is the hardest correctness problem in Pylair (docs/maintainers/PROJECT_BRIEF.md
section 4), so it is specified before it is built. This document is the
contract: `docs/protocol-v1.md` says *how* cancel travels on the wire;
this says *what it guarantees*. Implemented in M3; tests in M3 must prove
each guarantee below, per mode.

## The question

When the front end drops a call (AbortSignal, navigation, user cancel),
what exactly happens to the Python work? The answer differs by execution
mode — Python has no safe way to kill a thread — so the mode is chosen
at `@expose` time, not at cancel time.

## Execution modes

| mode | declared as | runs in | cancel mechanism |
|---|---|---|---|
| async task | `@expose` + `async def` | runtime event loop | `asyncio.Task.cancel()` |
| async generator | `@expose` + `async def` + `yield` | runtime event loop | cancel + generator `aclose()` |
| isolated sync | `@expose(isolated=True)` `def` | worker subprocess | `terminate()` → `kill()` |
| inline sync | `@expose` `def` | runtime worker thread | **cannot interrupt** — see below |

### Async tasks and generators

- Cancel sends `Task.cancel()` at the next `await` point. The coroutine
  unwinds normally: `try/finally` and `async with` blocks run, resources
  are released.
- **Guarantee:** if the Python code is well-behaved async code, cancel is
  clean and complete. `CancelledError` propagating out of the function
  maps to wire error `TASK_CANCELLED`.
- **Limit:** a coroutine blocked in *native* code (a sync C call inside a
  library — e.g., a torch op not awaited) cannot be preempted; cancel
  takes effect when control returns to the event loop. Docs must say:
  for CPU-heavy or native-blocking work, use `isolated=True`.
- Generators: cancel closes the stream; the TS `AsyncIterable` ends. If
  the generator is mid-`yield` the runtime discards the pending chunk.
  Partial results are *delivered* (already-sent chunks stand), never
  rolled back.

### Isolated sync functions (`isolated=True`)

- Each call runs in a dedicated worker subprocess (spawned per call).
- Cancel → `terminate()` (SIGTERM/SIGKILL-equivalent), 3s grace →
  `kill()`. Wire result: `TASK_CANCELLED` (or `WORKER_DIED` if the worker
  already died on its own).
- **Guarantee:** the worker *will* die — the supervisor owns its process
  handle and kills the whole process group (POSIX) / job object
  (Windows), so children the function spawned die too (no leaked
  grandchildren — M3 test asserts this).
- **Partial results:** discarded. An isolated call returns a value only
  on normal completion; cancelled calls deliver nothing.
- **Cleanup:** `finally` blocks in the worker run during the SIGTERM
  grace only. After `kill()` nothing in the worker runs again — files
  it wrote stay written (documented; cleanup is the app's business).

> **Cost warning — read before using `isolated=True`.**
> A per-call worker re-imports its modules and rebuilds its world *on
> every call*. With torch-class imports that is seconds per call, and
> for anything holding a loaded model it makes `isolated=True`
> effectively unusable. The intended fix is a **warm worker**: a
> persistent, lazily-spawned subprocess that keeps its imports/model
> resident and is only *killed* (not per-call spawned) on cancel, then
> lazily re-initialized on the next call. To keep that addition
> non-breaking, `@expose` reserves a `worker` option now:
> `@expose(worker={...})` — v1 accepts only `{mode: "process"}` (the
> per-call default implied by `isolated=True`); `mode: "warm"` lands
> when warm workers do. On the wire, `exposes[].kind` reports
> `"isolated"` either way; worker config rides inside `signature.meta`.

### Inline sync functions (non-isolated)

- **Cannot be safely cancelled.** Python cannot preempt a running
  sync function in a thread, full stop.
- The cancel request is *recorded*: the function keeps running, its
  result is computed and **discarded**, and the caller still observes
  `TASK_CANCELLED` immediately — the UI is never held hostage.
- Docs/API guidance: mark anything that can run longer than ~1s or touch
  native code `isolated=True`. `pylair doctor` warns on exposed sync
  functions without the flag (M4).

## Cross-cutting cases

- **Cancel during restart:** task dies with the process →
  `RUNTIME_CRASHED`, not `TASK_CANCELLED` (distinct errors matter —
  one is a user action, one is a failure).
- **Cancel outcomes are distinct, never ambiguous.** `cancel()` resolves
  to `cancelled` (the runtime confirmed it stopped), `terminated`
  (isolated worker killed and reaped), `requested_may_continue` (inline
  sync — the work cannot be interrupted; see the warning above),
  `termination_uncertain` (termination requested but unconfirmed — it
  may still be running), `completed` (it had already finished),
  `not_found` (no such task in this runtime generation — a stale id can
  never control a new task), `rejected` (this webview does not own the
  task), or `delivery_failed` (the control frame could not be queued).
- **Cancel that outruns its call:** the control lane can overtake the
  data lane, so the runtime may see `cancel` before `call`. It records
  the id and acknowledges `{cancelled: true, status: "pending"}` —
  applied at registration, terminal `TASK_CANCELLED` still delivered —
  so the host reports `cancelled`, never `completed`. Only a task that
  truly finished (or an id the bounded pending set refused) answers
  `cancelled: false` → `completed`.
- **Cancel an unknown/finished task:** `not_found` / `completed` —
  distinct outcomes, never an error for the caller. A stream the host
  abandoned (consumer stalled, vanished, or over a byte cap) counts as
  finished: a later cancel reports `completed`.
- **Ownership is enforced, not advisory.** Only the webview that started
  a task can cancel it or grant it stream credit — the supervisor
  rejects other webviews, and the owner is always Tauri's window label,
  never a JS-passed argument. A destroyed window's tasks are cancelled.
- **Cancel ack timeout:** if the runtime doesn't confirm within 5s
  (`cancel_timeout`), `cancel()` rejects with `RUNTIME_WEDGED`; a
  blocked runtime cannot be relied on to answer.
- **Cancel is one-way and idempotent:** re-cancel on a live task is a
  no-op (`cancelled` again); on a finished task it reports `completed`.
  There is no un-cancel.
- **In-flight during shutdown:** graceful shutdown drains up to
  `grace_ms`; tasks past the deadline get `TASK_CANCELLED` (async) or
  process-group kill (isolated workers are children of the runtime —
  killing the runtime must not orphan them; supervisor tracks and
  reaps).

## Leak guarantees (what the M3 tests assert)

- No child processes outlive the host or a cancelled isolated call.
  Mechanisms (protocol §6): children exit on stdin/control-channel EOF;
  Windows uses Job Objects (`KILL_ON_JOB_CLOSE`), POSIX uses process
  groups (+`PR_SET_PDEATHSIG` on Linux).
- No `asyncio.Task` objects leak after cancel (runtime-side assertion
  via task accounting).
- The host's stderr drain is unconditional — a full stderr pipe must
  never be what makes a task hang.
- Every cancelled stream produces exactly one terminal message — the
  `call` response carrying `TASK_CANCELLED` (or a normal
  `{done: true}` if the generator finished first; the race is resolved
  task-side and both orders are legal).
- Fault-injection tests must include: **a stray fd-1 write** (library
  `print` / native `fwrite` must not corrupt the frame channel nor kill
  the runtime) and **a wedged runtime** (event loop starved in native
  code — ping/pong misses → `unresponsive` → restart).

## Wire summary

- `cancel {task}` → `{cancelled: bool, reason?}` or `TASK_NOT_FOUND`.
- A `call` always produces exactly one response; for streaming calls
  that response is the terminal message (protocol §3.1).
- Task end states observable by caller: `result`/`{done: true}`,
  `INTERNAL`, `TASK_CANCELLED`, `RUNTIME_CRASHED`, `RUNTIME_WEDGED`,
  `WORKER_DIED`, `PROTOCOL_VIOLATION`. Exactly one, always.
