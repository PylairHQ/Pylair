# 0008: Warm isolated workers

Status: proposed (design spike; no implementation yet)

## Question

`@expose(isolated=True)` spawns `python -m pylair._worker <module> <fn>`
per call (`_tasks.py:345-352`), which re-imports the app module, runs
once and exits (`_worker.py:108-184`). For an app holding a loaded
model that is seconds of import+init per call — `docs/cancellation.md`
already calls `isolated=True` "effectively unusable" for it and reserves
`worker={"mode": "warm"}` (`_expose.py:112-115`, `ideas.md:14-18`).

Should warm workers exist as a per-function pool of long-lived workers
serving one call at a time over a private framed loop, and exactly what
lifecycle, interrupt, accounting and readiness semantics should they
have? This document is the design; the implementation PRs follow it.

## 1. Current lifecycle vs. a long-lived worker

**Today.** `TaskRegistry._run_isolated` (`_tasks.py:330-396`):
`PYLAIR_WORKER_PARENT=<runtime pid>` is injected (339); Windows gets
`creationflags=0` (342), POSIX `start_new_session=True` so the worker
is its own process-group leader and `killpg` reaches its whole tree
(344). `communicate(input_json)` does the one-shot exchange (353);
nonzero exit -> `WORKER_DIED` with an output tail (354-365).

`_worker.main` (`_worker.py:108-184`) is strictly single-shot:
`_exit_with_parent()` first, then read-once/run/write-once/exit.
The orphan reaper it installs (`_worker.py:28-105`) is the interesting
machinery:

- Windows returns immediately (38-39): the host's Job Object
  (`platform_windows.rs:120-156`, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
  at 131) already contains every descendant of the runtime, workers
  included — they are born inside it.
- POSIX: an immediate ppid check (44-46) for the died-during-exec case;
  a forked **reaper process** (56-77) that watches the worker's ppid
  and the runtime via `kill(parent, 0)` and `killpg(0, SIGKILL)`s on
  either death — forked pre-import while single-threaded; a Linux
  `PR_SET_PDEATHSIG` backstop (84-94); and a ppid-watch daemon thread
  (96-105) as the last fallback.
- The runtime side reaps on natural exit too: the worker's own session
  means `monitor_child`'s leader-exit `reap_group` (`platform_unix.rs:
  146-148`) cannot reach it — that asymmetry is exactly why
  `_exit_with_parent` exists. Runtime shutdown reaps via `_drain` ->
  `cancel_all` + per-task `_kill_worker` (`_server.py:506-513`).

**What changes for warm.** The spawn flags and the orphan machinery are
all still needed and reused verbatim — the reaper is forked once per
*worker spawn*, not per call, so its per-call cost disappears with the
respawn cost. Two real changes:

- `_worker.main`'s single-shot exchange becomes a framed **loop** on the
  same `_transport` framing (`_transport.py:27-41` `read_frame`/
  `write_frame` over `(rfd, wfd)` — the private dup'd-stdout hygiene at
  `_worker.py:120-123` stays: fd 1 still goes to stderr so a `print()`
  can never corrupt the channel).
- **Idle processes exist.** A warm worker between calls is alive,
  holds its group/session/Job membership, and must be reaped at runtime
  shutdown, on dev-restart, and on its `idle_ttl_s` (if set) — new
  runtime-side bookkeeping, same kill paths.

Dev reload needs nothing new: the restart trigger (`session.rs:172-186`)
kills the whole runtime generation; pools live inside the runtime and
die with it.

## 2. Cooperative interrupt: what actually reaches Python

Proposal: interrupt first (SIGINT / CTRL_BREAK), grace, then the
existing kill+reap.

- **POSIX** — `os.killpg(worker.pid, SIGINT)` (or the group): reliable.
  CPython delivers `KeyboardInterrupt` to the main thread at the next
  bytecode boundary; `time.sleep` and most blocking I/O are
  interruptible (PEP 475 retries after EINTR *only if the handler does
  not raise* — SIGINT's raises, so sleeps wake promptly). The honest
  limit: a native call holding the GIL (a torch op, a numpy matmul, a
  blocking read in a driver that swallows EINTR) is *not* preempted —
  KeyboardInterrupt lands when control returns to the interpreter.
  Grandchildren get the SIGINT too via `killpg`, matching today's
  kill semantics.
- **Windows** — today's `creationflags=0` (`_tasks.py:342`) cannot carry
  this: `CTRL_C_EVENT` cannot be targeted (it broadcasts to every
  process on the console). The proven pattern is: spawn warm workers
  with `CREATE_NEW_PROCESS_GROUP` (process group id = pid), then
  `os.kill(pid, signal.CTRL_BREAK_EVENT)` -> `GenerateConsoleCtrlEvent`
  reaches exactly that process. CPython maps CTRL_BREAK to `SIGBREAK`
  — default disposition is **terminate**, so the worker must install
  `signal.signal(signal.SIGBREAK, <raise KeyboardInterrupt>)` early in
  `main()`. One open point to prove in the test plan: the signal only
  travels to processes sharing the caller's console; the runtime is
  console-subsystem `python.exe` (it allocates a console under a GUI
  parent), and the worker shares it as long as no `CREATE_NEW_CONSOLE`/
  `DETACHED_PROCESS` flag appears. If that assumption fails on a
  supported configuration, Windows falls back to kill-only — the
  documented answer, not a hidden gap.

**Interrupted vs. stuck** needs no worker cooperation: the verdict is
parent-side. After the interrupt is sent, the runtime waits
`cancel_grace_ms` for that call's response frame on the private channel;
a response (result, error, or the worker's own KeyboardInterrupt report)
means *interrupted and survived*; silence means *stuck* -> the existing
`_kill_worker` path (SIGTERM->SIGKILL group / taskkill + Job close) runs
and the pool respawns lazily. Default `cancel_grace_ms = 3000`, bounded
by the same budget as today's `ISOLATED_GRACE_S` (`_tasks.py:17`) so the
cancel ack still lands inside the host's `cancel_timeout` (5 s,
`config.rs:397`); the `_report_isolated_cancel` bound
(`ISOLATED_GRACE_S + 1.0`, `_tasks.py:548-551`) covers it unchanged.

**Ack values must not change** (`wire.rs:99-101`, `cancellation.md:
88-96`): interrupt-survived reports plain `{"cancelled": true}` — the
work confirmed stopped, the worker merely lived; only the kill path
reports `status: "terminated"` ("worker killed and reaped",
`handle.rs:87`); timeout mid-path stays `termination_in_progress`. No
new outcome value.

## 3. Queueing and caps

- **pool=1, second call: queue in the runtime, not fail-fast.** The
  call registers as a normal isolated task first (slot, owner, cancel
  path all live), then waits on the pool — so it is cancellable and
  fairness-counted while queued, exactly like the `requires=` gate
  (`_tasks.py:135-147`) already is. A per-pool queue bound is
  unnecessary: `max_concurrent_tasks` already bounds total in-flight.
- **Process accounting must change what it counts.** Today
  `self._isolated` counts in-flight calls (`_tasks.py:112,127,612`) —
  1:1 with live processes because process mode is per-call. Warm breaks
  the 1:1: the honest rule is **live worker processes <=
  `max_isolated_workers` (8)**: `process-mode in-flight + warm-pool
  live members <= cap`. A warm worker holds its slot for its lifetime
  (spawn to reap), so two `pool: 1` functions permanently reserve 2 of
  the default 8 — documented, and the reason `pool` defaults to 1.
  A call that would exceed the cap fails `max_isolated_workers` at
  pool-acquire time with the same typed error (`_tasks.py:115-122`).
- **Host fairness stays correct unchanged.** It counts *calls*, not
  processes (`fairness.rs:52-54` `in_flight_isolated`; 0005 §4 made it
  a best-effort admission check with the Python cap as backstop).
  Queued warm calls are in-flight isolated calls — already counted.
  The Python cap remains the safety bound, now on live processes.

## 4. Memory and GPU guidance

Each pool member is a full interpreter plus whatever the function's
globals hold — with torch-class models, N members = N model copies
(CPU or VRAM). Defaults: `pool: 1`, `prewarm: False` (lazy spawn),
`idle_ttl_s: None` (a warm worker is a declared, opted-in cost; TTL
reap/respawn machinery can come later without an API change), and
`cancel_grace_ms: 3000` per §2. Docs guidance to write when it ships:
one model copy per worker, prefer `pool: 1` and let the queue absorb
bursts, raise `pool` only when throughput demands and memory allows.

## 5. Semantics change: state survives calls

`isolated` today guarantees a fresh process per call; warm keeps the
process, so **module globals persist between calls** — that is the
point (the model lives there) and the semantic difference to spell out:

- Still guaranteed: crash isolation (a worker segfault kills the
  worker, the runtime survives — `WORKER_DIED` for the in-flight call,
  lazy respawn), process-level cancellation (the kill path is
  identical), process/fd/environment separation from the runtime, and
  cap accounting per §3.
- No longer guaranteed: per-call hygiene. Mutable module state leaks
  between calls by design; a call interrupted mid-mutation can leave
  globals half-updated — the same exposure any `KeyboardInterrupt`
  path has, documented. An app needing "clean room per call" keeps
  `mode: "process"`.
- **No per-worker init hook needed.** The lazy-global pattern
  (`_MODEL = None; if _MODEL is None: _MODEL = load()` inside the
  function) already runs init exactly once per worker lifetime, at
  first serve — prewarm only moves where it happens. An explicit
  `init=` option would duplicate what the function body already
  expresses; revisit only if apps need init *separate* from the first
  call's timeout budget.

## 6. Readiness integration

- `prewarm: true` declares a component at decoration, same mechanism as
  `requires=` (`_expose.py:144-145` -> `_readiness.declare`): name it
  `worker:<fn>`, phase `loading`. The runtime spawns the pool at
  startup; when a member completes its import+expose handshake on the
  private channel, report `ready`. Spawn failure -> `failed` with the
  spawn error as detail — `requires=("worker:<fn>",)` and
  `health().app` then work for free, and a call gated on it never sees
  "unknown component".
- **health() stays minimal for v1**: pools add nothing new to
  `health().load` — in-flight calls already count as tasks; an idle
  warm worker is a process, not load. The `worker:<fn>` component is
  the observability surface.

## 7. Wire impact: none

`exposes[].kind` reports `"isolated"` either way (the contract is in
`docs/cancellation.md:69-70`); `worker` config rides inside
`signature.meta` (`_expose.py:41-47` — the host already reads `kind`
for isolated accounting per 0005 §4 and nothing else in `meta`). The
private framed loop between runtime and worker is an internal channel
reusing `_transport` framing — not the host<->runtime protocol — so
`docs/protocol-v1.md` needs no change. Cancel ack values unchanged per
§2. The only doc-contract updates: `docs/cancellation.md` (warm mode,
interrupt-first cancel, state persistence) and `docs/ideas.md` (the
entry graduates).

## 8. Test plan and PR split

**Tests**

- Python fakes, all OSes: a `sleep()`-bound fn interrupted mid-call ->
  `KeyboardInterrupt`, call ends, **same pid** serves the next call
  (proves warm survival, not just cancel); a fn that installs
  `SIGINT -> SIG_IGN` (or a C-level blocker on Windows) -> grace
  elapses -> kill path -> next call observes a **different pid**
  (proves respawn); crash mid-call -> `WORKER_DIED` + lazy respawn on
  next call; crash-loop limit (3 consecutive deaths -> pool stops
  respawning, typed error).
- Windows-specific: `CTRL_BREAK_EVENT` actually delivered (a spawn
  flags + SIGBREAK-handler test, not just "no crash") — the §2
  proof obligation; and the kill fallback still correct when the
  interrupt path can't run.
- Caps/fairness: warm member holds its `max_isolated_workers` slot for
  its lifetime; queued warm call counts for per-owner fairness and is
  cancellable while queued.
- Lifecycle: dev-restart kills pools (generation change); shutdown
  reaps idle members via `_drain`; idle worker produces no orphans on
  hard runtime death (the `_exit_with_parent` machinery already proven
  for process mode, re-asserted for the loop form).
- Selftest leg (installed app): a warm fixture returning `os.getpid()`;
  two calls -> same pid; a timed `sleep` cancel -> interrupt path ->
  third call still the same pid. On Windows this leg doubles as the
  CTRL_BREAK proof on a real packaged app.

**PR split (one concern each)**

1. Worker loop + private framed channel + pool spawn/serve, lazy only,
   kill-only cancel (process-mode semantics preserved end to end).
2. Cooperative interrupt: POSIX SIGINT, Windows CREATE_NEW_PROCESS_GROUP
   + SIGBREAK handler, `cancel_grace_ms`, ack mapping per §2.
3. `prewarm` + `worker:<fn>` readiness component; crash-loop limit.
4. Cap accounting on live processes, idle_ttl if accepted, selftest
   leg, `docs/cancellation.md`/`ideas.md` updates.

**Release impact:** additive and opt-in — `mode: "warm"` was rejected
at decoration until now (`_expose.py:112-115`); `mode: "process"`
behavior is untouched. No version bump in the implementation PRs; the
feature lands in the next RC train.
