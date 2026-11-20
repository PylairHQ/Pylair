# 0005: Per-window fairness for runtime limits

Status: proposed (spike, 2026-11-20). No implementation in this PR;
`tests/window_fairness.rs` is throwaway proof the starvation is real
and deterministic.

## Question

`docs/security.md` §6 and `docs/protocol-v1.md` §8 carry a known gap:
`Limits` (`crates/tauri-plugin-pylair/src/supervisor/config.rs:80`) are
per runtime, not per window, so one webview can exhaust a shared cap and
starve the others. `TaskMeta.owner` (`src/supervisor/run.rs:63`) already
records the Tauri-reported window label on every task, and cancel-on-close
already reaps by owner. Should we add per-window fairness, and how?

**Evaluated direction (kept): a work-conserving guaranteed minimum.**
Each *live* window reserves `floor(cap / live)` of a shared resource; a
window may use any capacity not reserved for other live windows.
Admission only - running tasks are never preempted. A single-window app
behaves exactly as today (its one live window reserves everything).
Over-share calls fail with `LIMIT_EXCEEDED` and a distinct
`data.limit = "window_share"`. Windowless host-side callers
(`owner: None` on `Handle::call`, e.g. `selftest.rs:68` and builder-API
users) get their own pseudo-window bucket with the same rules.

The wire protocol is unchanged: everything below is host-side accounting
in the Handle and the supervisor actor - the same boundary the
`PERMISSION_DENIED` check already uses (`commands.rs:20`, rejected
before a task id exists).

## 1. What one window can exhaust today, and where it is attributable

| Resource | Cap (default) | Enforcement point | Owner attributable? |
|---|---|---|---|
| in-flight tasks | `max_concurrent_tasks` 64 | `session_commands.rs:33`, `:99` - `self.tasks.len() >= lim` | Yes - `TaskMeta.owner` is set at registration (`:54`, `:164`); count by owner at admission |
| in-flight streams | `max_concurrent_streams` 32 | `session_commands.rs:113` - counts `is_stream` metas | Yes - same `TaskMeta` |
| supervisor command queue | `max_queued_commands` 64 | `mpsc::channel` bound (`run.rs:136`); `try_send` in `handle.rs:191`, `:241`, `:275`, `:364` | Yes - `owner` rides every `Command` variant. BUT the slot frees at actor dequeue, which the Handle cannot see (see §2) |
| buffered stream bytes | `stream_buffer_bytes_total` 64 MiB | `StreamLease` fetch_add on a shared `Arc<AtomicU64>` (`stream.rs:36`, created per call at `handle.rs:233`) | Yes - `call_stream` knows the owner at `stream_pair` time; a per-owner counter slots into the same lease. Release is free: `Drop` on the chunk (`stream.rs:37`) |
| per-stream bytes + chunks | `stream_buffer_bytes` 16 MiB, `stream_buffer` 256 | same `StreamSink` (`stream.rs:76`) | Per-stream already; needs nothing for fairness |
| stream credit | `max_stream_credit` 4096 | per-task `meta.credit` (`session_commands.rs:273`) | Already per-task => per-window transitively; nothing to do |
| isolated workers | `max_isolated_workers` 8 | **Python-side** (`_tasks.py:111`) - the host never counts them | Indirectly - see §4 |
| `frame_out` data lane | `FRAME_OUT_CAPACITY` 64 (`spawn.rs:38`) | `send_frame` → `LaneFull` (`session_commands.rs:71`, `:198`) | The owner is known at the send site, but a full lane means the *runtime* is not draining - a per-runtime symptom, not a per-window one. Covered indirectly: calls are what fill the lane, so the queue admission cap bounds each window's contribution per drain cycle. Keep the global cap as the wedge tripwire |
| `max_frame` | 16 MiB | `send_frame` → `Oversize` | Per-frame, not monopolizable; untouched |

Control lane (`CTRL_OUT_CAPACITY` 16, `spawn.rs:41`) - exempt by design:
`cancel`/`credit`/`shutdown` must outrun the flood they stop. The same
doctrine applies to fairness: see §5.

## 2. Can admission happen before the shared mpsc queue? What must be released?

Yes, with a two-layer split - the only place that sees both the owner
and the queue is the Handle<->actor boundary:

- **In-queue admission (Handle side).** `Handle::call_task`/`call_stream`
  (and any other window-sourced command we choose to gate) increment a
  per-owner `in_queue` counter *before* `try_send`, in a map shared with
  the actor (an `Arc<Fairness>` created in `spawn()` at `run.rs:144`,
  next to `next_task`/`stream_bytes_total` - the same Handle<->actor
  sharing pattern already used for both). Reject over-share with
  `window_share` and the command never occupies a slot.
- **In-queue release (actor side).** The actor decrements at every point
  `cmd_rx.recv()` returns `Some(_)`: the `select!` arm in
  `run_child` (`run.rs:485`), `sleep_or_shutdown` (`run.rs:310`),
  `park_failed` (`run.rs:424`). Three sites, all enumerated today.
  `fail_command_fast` commands release the same way.
- **In-flight admission (actor side).** The task cap lives in
  `Session::call`/`call_stream`, which already sees `owner`. Maintain a
  per-owner `in_flight` count alongside `self.tasks` - increment on
  `tasks.insert`, decrement on the three `tasks.remove` sites
  (`session.rs:185`, `:313`; `run.rs:584` in `drain_exited_inbox`).
- **In-flight release on generation end.** `self.tasks` is per-session;
  a crash, wedge, dev restart or shutdown drops the whole map. The
  per-owner counts must zero with it - one sweep in `run_child`
  teardown. (This is the subtle one: shared-map counters would leak
  across generations without it; counters maintained inside `Session`
  die with the map for free.)
- **Window close.** `WindowEvent::Destroyed` → `cancel_owner`
  (`builder.rs:288`) → tasks leave via the normal remove sites; nothing
  new needed. The window's queued commands drain and register, then the
  reaper kills them - bounded, no leak.
- **Page reload.** `PageLoadEvent::Started` → `cancel_owner_before`
  (`builder.rs:344`): the label persists, the share persists, old tasks
  release normally.
- **Rejection paths after queue admission** (`send_frame` Oversize /
  LaneFull, `reject_stream`): a task is counted only on successful
  `tasks.insert`, so a failed dispatch decrements `in_queue` and never
  touched `in_flight`. No leak.

## 3. Definition of "live window"

**Chosen: the `WindowRegistry.subscribed` set** (`registry.rs:12`) -
windows that have invoked at least one Pylair command and still exist.

- It already exists and is already maintained: `subscribed()` runs
  before every command (`commands.rs:21` - *before* the call, so a
  window's first call always joins before its admission check);
  `drop_label` fires on `WindowEvent::Destroyed` (`builder.rs:283`) and
  on `emit_to` failure against a dead window
  (`handle_emit_failure`, `registry.rs:65`).
- First-call semantics come out right: a window that has never called
  reserves nothing (it consumes nothing - fairness is unaffected). The
  moment it subscribes it becomes live, its share is reserved, and an
  over-share hog stops being admitted until it drains below its new
  ceiling. Joining never preempts; it only stops new admissions.
- "Every webview whose capability includes pylair" is not obtainable
  host-side: capability resolution happens inside Tauri's invoke
  machinery - the plugin only sees a `CommandScope` *inside* an
  invocation (`commands.rs:14`). Counting all `get_webview_windows()`
  labels would reserve shares for windows that can never call, shrinking
  real capacity for everyone (a reserved share cannot be borrowed - that
  is the point of a reservation).
- Wrap mode fits naturally: the wrap window is remote content - it
  never invokes `python_call`, never subscribes, reserves nothing
  (`builder.rs:226` gates events to it separately anyway). Windowless
  host callers (`owner: None`) join a named pseudo-bucket on first use,
  same rule.

## 4. Isolated workers - attributable, with one new piece of state

Yes, feasible: `hello["exposes"]` already carries
`{"name", "kind", ...}` and `kind` is `"isolated"` for
`@expose(isolated=True)` (`_expose.py:113`). The host discards it after
broadcast (`handshake.rs:90` emits `RuntimeEvent::Exposes`, nothing
retains it). So:

- Retain `fn_name -> kind` per generation (store on the `Supervisor` at
  hello; a restart's manifest may differ, so it belongs to the session,
  not the plugin).
- `Command::Call` already carries `fn_name`: `Session::call` looks up
  the kind and counts the task against a per-owner isolated share of
  `max_isolated_workers` alongside the task share. Unknown fn → treated
  as non-isolated; Python's `FUNCTION_NOT_FOUND` sorts it out anyway.
- The Python cap (`_tasks.py:111`) stays as the global backstop - host
  admission is best-effort because the worker is counted only when the
  runtime registers the task; a same-instant double-admit can still
  overbook by the race width. Acceptable for fairness (admission only),
  unacceptable for safety (cap stays).

Not a wire change. Cost: one small retained map + one `TaskMeta`-level
count. If we wanted zero new state, the honest fallback is: isolated
calls share the *task* share (a window can still monopolize all 8
workers within its task budget - weaker but strictly better than today).
Recommend including it; it is the heaviest resource on the list.

## 5. Reject vs per-window fair queueing - recommend reject

**Reject (admission only).** The whole limits doctrine is
"one failure, bounded and explicit, never a silent stall"
(`security.md` §6): a full command queue is `LIMIT_EXCEEDED`, not a
wait (`handle.rs:265`). Fair queueing would invent waits the system has
no model for: a call parked in a fairness queue is indistinguishable
from a wedge to the existing `cancel_timeout`
(`handle.rs:284`) and `RUNTIME_WEDGED` machinery; cancelling a
queued-but-not-registered call needs a new cancellation path the task-id
model cannot express (no task exists yet); and over-share calls in this
design are *not* delayed - they are refused, which is exactly what
`LimitExceededError` already tells the client to handle. Work
conservation means a solo window never feels rejection at all; queueing
only smooths multi-window bursts, and a typed rejection plus client
retry is simpler and honest.

The one exemption, same as the control lane: `cancel`, `grant_credit`,
`retry`, `shutdown`, `cancel_owned` bypass the share check - they cannot
create load (credit is per-task capped already; cancel relieves it), and
an over-share window must always be able to cancel its own flood.

If real apps later show bursts where rejection hurts, the alternative
is a per-window bounded wait-list with a deadline and cancel-while-queued -
new machinery that needs its own decision record. Stop there.

## 6. Config surface

Minimal, one switch, no tuning knobs:

- `Limits.per_window_fairness: bool` - default **true**. Lives on
  `Limits` because it is limit policy: validated and logged with the
  rest in `spawn()` (`run.rs:123`). Builder:
  `Limits::with_per_window_fairness(bool)` matching the existing
  `with_*` style.
- Env escape hatch: `PYLAIR_PER_WINDOW_FAIRNESS` - `0`/`false`/`off`
  disables, same `env_usize`-style parse family. Disabling restores
  today's exact behavior.
- Startup log: extend the existing `pylair limits:` line with
  `window_share=on|off`.
- No share-size knob: `floor(cap / live)` *is* the fair share; a
  percentage knob was considered and rejected - it only creates ways to
  be unfair by accident. (Live count is read at admission time; shares
  re-balance as windows join and leave.)
- Defaults cannot reduce a single-window app: `live = 1` reserves the
  whole cap for that window, so every admission path is byte-identical
  to today. The env kill switch covers any surprise in the field.
- Error shape: `LIMIT_EXCEEDED` with `data: {"limit": "window_share",
  "max": <window's ceiling>, "window": "<label>"}` - the `window` field
  mirrors `PERMISSION_DENIED`'s `{fn, window}` precedent. Client-side it
  surfaces as the existing `LimitExceededError` with `.limit ==
  "window_share"`; no client change needed.

## 7. Test plan

Spike proof in this PR (`tests/window_fairness.rs`): a fake runtime that
answers `hello`/`pong`/`shutdown` but never answers calls, so every
admitted call holds its task slot forever. Owner `a` fills
`max_concurrent_tasks`; owner `b`'s call then fails
`max_concurrent_tasks` - the starvation, deterministically, today.
(The queue-flood variant is provable only by outracing the actor and is
kept out of the suite; §1/§2 show it analytically.)

Implementation PRs add (flipping the spike test is the acceptance):

- `tests/window_fairness.rs`:
  - starvation repro flipped: `b` is admitted while `a` is over-share;
    `a`'s next call fails `LIMIT_EXCEEDED{limit:"window_share"}`
  - single-window parity: one live owner reaches the full global cap
    before any rejection
  - work conservation: `a` alone uses 100%; `b` joins and is admitted
    even while `a` remains over-share
  - same shape for `max_concurrent_streams` and for queue admission
    (`max_queued_commands` small; `a` floods `call_task` try_sends,
    `b`'s call still lands)
  - `Destroyed`-equivalent release: `cancel_owner("a")` drains `a`'s
    slots and `b` fills the freed capacity
  - generation reset: crash the runtime mid-flood, restart, verify
    `in_flight` did not carry over and a solo window still reaches the
    cap
  - windowless bucket: `owner: None` calls obey their own share
- Selftest/acceptance: the `PYLAIR_SELF_TEST_WEBVIEW` two-window
  harness already exists (`win-b`, `builder.rs:258`). Recommended, not
  required: `main` floods streams while `win-b` proves a call lands -
  the Rust tests exercise owners directly on the `Handle`; only the
  installed-app leg proves the label plumbing end to end.

## Implementation plan (one concern per PR)

1. **`Fairness` scaffolding + live set.** `Arc<Fairness>` in `spawn()`;
   `WindowRegistry` delegates `join`/`leave` so `subscribed` and
   `drop_label` maintain the live set; `owner: None` pseudo-bucket; the
   `per_window_fairness` config + env + startup log. No enforcement yet -
   counters only, tests cover join/leave/reset.
2. **Queue admission.** `in_queue` on Call/CallStream (+release at the
   three `cmd_rx.recv()` sites); `window_share` error; the flipped
   queue-flood test.
3. **Task/stream admission.** Per-owner `in_flight` inside `Session`
   (dies with the generation), tasks + streams ceilings, the flipped
   starvation test, single-window parity test.
4. **Stream bytes.** Per-owner `Arc<AtomicU64>` on `StreamLease` and a
   per-owner ceiling computed from the live set at `chunk()` time.
5. **Isolated workers.** Retained per-generation `fn_name -> kind`;
   per-owner isolated share; Python cap untouched as backstop.
6. **Docs + selftest leg.** This doc → `accepted`; `security.md` §6 and
   `protocol-v1.md` §8 gap lines updated; README known-limitations line
   removed; optional `win-b` flood-survival selftest.

PRs 2-5 land behind the switch; PR 6 can merge at any point after PR 3.

## Consequences

- A hostile or runaway window gets a hard, typed failure instead of a
  runtime-wide denial of service - and the failure names its ceiling.
- Fairness is admission-only: no preemption, no priority, no
  reordering. A window never loses running work to another window
  joining.
- New shared state: one `Arc<Fairness>` (label →
  `{in_queue, in_flight, stream_bytes}` counters + live set). Locking is
  a `Mutex` around the map on the command-admission path only - no
  await under it.
- The `in_queue` release contract is the fragile part: every future
  `cmd_rx.recv()` site inherits the obligation to decrement. Mitigation:
  the recv sites are few and centralized (§2); the PR 2 test covers the
  restarting/failed-park paths.
- Per-window `pylair://stderr`-style accounting (stderr volume, event
  fan-out cost) stays out of scope: those lanes are already bounded and
  drop-oldest, and a window cannot grow them.
