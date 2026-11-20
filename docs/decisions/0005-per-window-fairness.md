# 0005: Per-window fairness for runtime limits

Status: proposed (spike, 2026-11-20; reservation model revised after
review). No implementation in this PR; `tests/window_fairness.rs` is
throwaway proof the starvation is real and deterministic.

## Question

`docs/security.md` §6 and `docs/protocol-v1.md` §8 carry a known gap:
`Limits` (`crates/tauri-plugin-pylair/src/supervisor/config.rs:80`) are
per runtime, not per window, so one webview can exhaust a shared cap and
starve the others. `TaskMeta.owner` (`src/supervisor/run.rs:63`) already
records the Tauri-reported window label on every task, and cancel-on-close
already reaps by owner. Should we add per-window fairness, and how?

**Chosen model: a guaranteed minimum with work conservation.**
Each *live* window is guaranteed a small minimum of every shared
resource. An over-share window's **new** calls are refused with
`LIMIT_EXCEEDED` and a distinct `data.limit = "window_share"`; nothing
already admitted is ever touched. Admission only - no preemption, no
priority, no reordering.

The formula, evaluated at each admission for caller `A`:

```
ceiling(A) = cap - sum over other live windows B of
                     max(0, min_reserve - in_use(B))
```

where `in_use(B)` is what B already holds of that resource and

```
min_reserve = min( max(1, cap / 8), floor(cap / live) )
```

- **Why `cap / 8`.** The guarantee has to be *usable* (8 tasks or 8
  queue slots is a real burst; 1 is a token) and *cheap when idle*: a
  subscribed-but-idle window costs everyone else `min_reserve` of
  capacity, not an equal share. `cap / 8` bounds that cost at 12.5% per
  idle window instead of 50%. It is a derived constant, not a knob.
- **Why the `floor(cap / live)` cap.** When `live > cap / 8` the
  all-idle degenerate case must still admit every window: with all
  windows idle, `ceiling(A) = cap - (live - 1) * floor(cap / live) >=
  floor(cap / live) >= min_reserve`, so nobody is locked out by
  reservations alone. Past `live > cap`, `floor(cap / live)` is 0 -
  reservations collapse and the global cap rules alone, an honest
  degradation rather than a deadlock.
- **Why reduced by `in_use`.** A window's existing holdings *are* its
  reservation. Once B holds `min_reserve` or more it contributes 0, and
  the other windows' ceilings return to `cap` - at that point B is just
  another competitor for the global pool, which is what "guaranteed
  minimum" means rather than "guaranteed share".

Worked example (tasks, `cap = 64`, `min_reserve = 8`): a settings window
subscribes, makes one call, completes (holds 0). `main` then floods:
`ceiling(main) = 64 - max(0, 8 - 0) = 56`. `main`'s new calls are
refused past 56 while settings sits idle. Settings calls again:
`ceiling(settings) = 64 - max(0, 8 - 56) = 64` and it is admitted on
the first free slot. While that settings call is in flight,
`ceiling(main) = 64 - max(0, 8 - 1) = 57` - the reservation shrinks by
what settings holds. If settings holds 8 concurrent tasks its
contribution is 0 and `main`'s ceiling is 64 again, bounded by the
global cap as today.

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
| buffered stream bytes (total) | `stream_buffer_bytes_total` 64 MiB | `StreamLease` fetch_add on a shared `Arc<AtomicU64>` (`stream.rs:36`) | Attributable (`call_stream` knows the owner at `stream_pair` time, `handle.rs:233`) - but per-window enforcement is **dropped**: see §5.1. Stays a global backstop |
| buffered bytes (per stream) | `stream_buffer_bytes` 16 MiB + `stream_buffer` 256 chunks | same `StreamSink` (`stream.rs:76`) | Already per-stream; needs nothing |
| stream credit | `max_stream_credit` 4096 | per-task `meta.credit` (`session_commands.rs:273`) | Already per-task => per-window transitively; nothing to do |
| isolated workers | `max_isolated_workers` 8 | **Python-side** (`_tasks.py:111`) - the host never counts them | Indirectly - see §4 |
| `frame_out` data lane | `FRAME_OUT_CAPACITY` 64 (`spawn.rs:38`) | `send_frame` -> `LaneFull` (`session_commands.rs:71`, `:198`) | The owner is known at the send site, but a full lane means the *runtime* is not draining - a per-runtime symptom, not a per-window one. Covered indirectly: calls are what fill the lane, so the queue admission cap bounds each window's contribution per drain cycle. Keep the global cap as the wedge tripwire |
| `max_frame` | 16 MiB | `send_frame` -> `Oversize` | Per-frame, not monopolizable; untouched |

Control lane (`CTRL_OUT_CAPACITY` 16, `spawn.rs:41`) - exempt by design:
`cancel`/`credit`/`shutdown` must outrun the flood they stop. The same
doctrine applies to fairness: see §5.

## 2. Can admission happen before the shared mpsc queue? What must be released?

Yes, with a two-layer split - the only place that sees both the owner
and the queue is the Handle/actor boundary:

- **In-queue admission (Handle side).** `Handle::call_task`/`call_stream`
  increment a per-owner `in_queue` counter *before* `try_send`, in a map
  shared with the actor (an `Arc<Fairness>` created in `spawn()` at
  `run.rs:144`, next to `next_task`/`stream_bytes_total` - the same
  Handle/actor sharing pattern already used for both). A call whose
  `in_queue + in_flight` would exceed its ceiling is refused with
  `window_share` and never occupies a slot.
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
- **Window close.** `WindowEvent::Destroyed` -> `cancel_owner`
  (`builder.rs:288`) -> tasks leave via the normal remove sites; nothing
  new needed. The window's queued commands drain and register, then the
  reaper kills them - bounded, no leak.
- **Page reload.** `PageLoadEvent::Started` -> `cancel_owner_before`
  (`builder.rs:344`): the label persists, the membership persists, old
  tasks release normally.
- **Rejection paths after queue admission** (`send_frame` Oversize /
  LaneFull, `reject_stream`): a task is counted only on successful
  `tasks.insert`, so a failed dispatch decrements `in_queue` and never
  touched `in_flight`. No leak.

## 3. Definition of "live window"

**Chosen: the `WindowRegistry.subscribed` set** (`registry.rs:12`) -
windows that have invoked at least one Pylair command and still exist -
**plus the `owner: None` host bucket, counted as live only while it has
queued or in-flight work.**

- The window set already exists and is already maintained:
  `subscribed()` runs before every command (`commands.rs:21` - *before*
  the call, so a window's first call always joins before its admission
  check); `drop_label` fires on `WindowEvent::Destroyed`
  (`builder.rs:283`) and on `emit_to` failure against a dead window
  (`handle_emit_failure`, `registry.rs:65`).
- First-call semantics come out right: a window that has never called
  reserves nothing except its `min_reserve` once subscribed - and it
  subscribes before its first admission check, so its guarantee applies
  to the very first call. An over-share hog stops being admitted until
  it drains below its new ceiling; joining never preempts.
- "Every webview whose capability includes pylair" is not obtainable
  host-side: capability resolution happens inside Tauri's invoke
  machinery - the plugin only sees a `CommandScope` *inside* an
  invocation (`commands.rs:14`). Counting all `get_webview_windows()`
  labels would reserve minimums for windows that can never call -
  wasted capacity, since a reservation is exactly what cannot be
  borrowed.
- Wrap mode fits naturally: the wrap window is remote content - it
  never invokes `python_call`, never subscribes, reserves nothing
  (`builder.rs:226` gates events to it separately anyway).
- **The `None` bucket is different from a window.** A subscribed window
  is a live UI that might call at any instant - its minimum must be held
  open continuously or the guarantee is meaningless. A host-side caller
  (`owner: None`, e.g. `selftest.rs:68` and builder-API users) has no
  persistent consumer: if it holds nothing, nothing needs reserving.
  So `None` counts as live only while `in_queue + in_flight > 0` for the
  bucket, following the same formula once counted. This is also what
  keeps the parity promise honest: a single host call at startup must
  not turn a single-window app into a two-"window" app forever.

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
  the kind and counts the task against a per-owner isolated ceiling
  (`min_reserve = min(1, ...) = 1` for `cap = 8`) alongside the task
  ceiling. Unknown fn -> treated as non-isolated; Python's
  `FUNCTION_NOT_FOUND` sorts it out anyway.
- The Python cap (`_tasks.py:111`) stays as the global backstop - host
  admission is best-effort because the worker is counted only when the
  runtime registers the task; a same-instant double-admit can still
  overbook by the race width. Acceptable for fairness (admission only),
  unacceptable for safety (cap stays).

Not a wire change. Cost: one small retained map + one counter. If we
wanted zero new state, the honest fallback is: isolated calls share the
*task* ceiling (a window can still monopolize all 8 workers within its
task budget - weaker but strictly better than today). Recommend
including it; it is the heaviest resource on the list.

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
`LimitExceededError` already tells the client to handle.

**The consequence to state plainly:** when a window joins while another
is over share, the over-share window's *new* calls are refused until it
drains below its new ceiling - and nothing already admitted is
affected. Queued commands still execute, running tasks still finish,
buffers still flush. The guarantee is forward-looking only: freed
capacity goes to the under-minimum window, not back to the hog.

The one exemption, same as the control lane: `cancel`, `grant_credit`,
`retry`, `shutdown`, `cancel_owned` bypass the share check - they cannot
create load (credit is per-task capped already; cancel relieves it), and
an over-share window must always be able to cancel its own flood.

If real apps later show bursts where rejection hurts, the alternative
is a per-window bounded wait-list with a deadline and cancel-while-queued -
new machinery that needs its own decision record. Stop there.

## 5.1 Per-window stream bytes - dropped, and why

The original plan (per-owner byte counter checked at `chunk()` time)
contradicts "admission only": a second window joining lowers A's
ceiling mid-stream, so A's *existing* streams would end with
`LIMIT_EXCEEDED` at their next chunk - losing admitted work to a policy
event, which this design forbids. Fixing a byte allowance at stream
admission instead would re-introduce the non-work-conserving
reservation this revision removed (an early A could pin a huge
allowance; a late B's allowance would stay tiny after A drained).

So `stream_buffer_bytes_total` stays a **global backstop**, exactly as
today: a total-memory tripwire that ends whichever stream trips it -
not a fairness mechanism. Per-window fairness is still real here: a
window's worst-case byte footprint is `its streams x 16 MiB`, its
stream count is already admission-limited, and `max_stream_credit`
already bounds every individual pump. Documented boundary: the backstop
can still end an innocent stream when the aggregate is exceeded - same
as today, now noted rather than assumed.

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
- **No share knob.** `min_reserve = min(max(1, cap / 8),
  floor(cap / live))` is a derived constant per resource (tasks 8,
  streams 4, queue 8, workers 1 at the defaults), recomputed at each
  admission from the live set. A percentage knob was considered and
  rejected - it only creates ways to be unfair by accident.
- Defaults cannot reduce a single-window app: `live = 1` leaves no
  "other live windows", so every ceiling is the full cap and every
  admission path is byte-identical to today. (The `None` bucket's
  transient liveness in §3 is what keeps this true when the app itself
  makes host-side calls.) The env kill switch covers any field surprise.
- Error shape: `LIMIT_EXCEEDED` with `data: {"limit": "window_share",
  "max": <the caller's ceiling>, "window": "<label>"}` - the `window`
  field mirrors `PERMISSION_DENIED`'s `{fn, window}` precedent.
  Client-side it surfaces as the existing `LimitExceededError` with
  `.limit == "window_share"`; no client change needed.

## 7. Test plan

Spike proof in this PR (`tests/window_fairness.rs`): a fake runtime that
answers `hello`/`pong`/`shutdown` but never answers calls, so every
admitted call holds its task slot forever. Owner `a` fills
`max_concurrent_tasks`; owner `b`'s call then fails
`max_concurrent_tasks` - the starvation, deterministically, today.
(The queue-flood variant is provable only by outracing the actor and is
kept out of the suite; §1/§2 show it analytically.)

Implementation PRs add, all under the revised formula:

- `tests/window_fairness.rs` (spike assertions flip):
  - starvation repro flipped: `a` holds `cap = 4` with a fixture that
    answers calls after a short delay, so slots free; `b` is admitted on
    a freed slot while `a`'s resubmits fail
    `LIMIT_EXCEEDED{limit:"window_share"}` - freed capacity goes to the
    under-minimum window, not back to the hog
  - single-window parity: one live owner reaches the full global cap
    (`live = 1` -> ceiling = cap) before any rejection
  - single-window + one host call parity: with `owner: None` in flight,
    `live = {win, host}` and the window's ceiling is
    `cap - (min_reserve - 1)`; after the host call completes the bucket
    leaves the live set and the window's ceiling is `cap` again
  - settings-window scenario: `cap = 64`, settings completes one call
    and idles; `main` floods to ceiling 56, not 32 and not 64; a new
    settings call is admitted on the first freed slot
  - work conservation past the minimum: `b` holding `>= min_reserve`
    contributes 0 - `a` resumes admission up to the global cap once `b`
    has its minimum
  - same shape for `max_concurrent_streams` (`min_reserve = 4` at
    `cap = 32`) and for queue admission (`max_queued_commands` small;
    `a` floods `call_task` try_sends, `b`'s call still lands)
  - `Destroyed`-equivalent release: `cancel_owner("a")` drains `a`'s
    slots and `b` fills the freed capacity
  - generation reset: crash the runtime mid-flood, restart, verify
    `in_flight` did not carry over and a solo window still reaches the
    cap
- Selftest/acceptance: the `PYLAIR_SELF_TEST_WEBVIEW` two-window
  harness already exists (`win-b`, `builder.rs:258`). Recommended, not
  required: `main` floods streams while `win-b` proves a call lands -
  the Rust tests exercise owners directly on the `Handle`; only the
  installed-app leg proves the label plumbing end to end.

## Implementation plan (one concern per PR)

1. **`Fairness` scaffolding + live set.** `Arc<Fairness>` in `spawn()`;
   `WindowRegistry` delegates `join`/`leave` so `subscribed` and
   `drop_label` maintain the live set; the `None` bucket joins only
   while it holds work; the `per_window_fairness` config + env +
   startup log. No enforcement yet - counters only, tests cover
   join/leave/reset and the `None` bucket's transient liveness.
2. **Queue admission.** `in_queue` on Call/CallStream (+release at the
   three `cmd_rx.recv()` sites); `window_share` error; the queue-flood
   test.
3. **Task/stream admission.** Per-owner `in_flight` inside `Session`
   (dies with the generation), tasks + streams ceilings under the
   revised formula, the flipped starvation test, both parity tests.
4. **Isolated workers.** Retained per-generation `fn_name -> kind`;
   per-owner isolated ceiling (`min_reserve = 1`); Python cap untouched
   as backstop.
5. **Docs + selftest leg.** This doc -> `accepted`; `security.md` §6 and
   `protocol-v1.md` §8 gap lines updated to name `window_share` and the
   stream-bytes boundary; README known-limitations line removed;
   optional `win-b` flood-survival selftest.

PRs 2-4 land behind the switch; PR 5 can merge at any point after PR 3.
Per-owner stream byte accounting was considered and dropped - see §5.1.

## Consequences

- A hostile or runaway window gets a hard, typed failure instead of a
  runtime-wide denial of service - and the failure names its ceiling.
- A window joining while another is over share stops the over-share
  window's **new** admissions until it drains; nothing already admitted
  is affected - no preemption, no lost work, no reordering. Freed
  capacity goes to whoever is under its minimum.
- The guarantee is a minimum, not a share: an idle live window costs
  everyone else `min_reserve` of each resource, and a window past its
  minimum competes for the global pool like today.
- `stream_buffer_bytes_total` stays a global backstop (§5.1); a window's
  byte footprint is bounded by its admission-limited stream count times
  the per-stream cap.
- New shared state: one `Arc<Fairness>` (label ->
  `{in_queue, in_flight}` counters + live membership). Locking is a
  `Mutex` around the map on the command-admission path only - no
  await under it.
- The `in_queue` release contract is the fragile part: every future
  `cmd_rx.recv()` site inherits the obligation to decrement. Mitigation:
  the recv sites are few and centralized (§2); the PR 2 test covers the
  restarting/failed-park paths.
- Per-window `pylair://stderr`-style accounting (stderr volume, event
  fan-out cost) stays out of scope: those lanes are already bounded and
  drop-oldest, and a window cannot grow them.
