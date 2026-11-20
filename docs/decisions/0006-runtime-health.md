# 0006: Runtime health API

Status: accepted, implemented (PRs #119-#123, #124)

## Question

Apps observe the runtime through exactly two shapes today: `state()`
(one word — `python_state` → `state_reply`, `commands.rs:226`,
`util.rs:80`) and `onState` (`pylair://state`, `{from, to, detail}`,
`util.rs:70`). The supervisor knows far more and throws it away: the
pong's echoed `seq`, its `loop_stale_ms`, the current miss count, the
restart accounting and the last exit reason. Meanwhile `unresponsive`
conflates two different faults — a hung process (missed pongs,
`session.rs:110`) and a busy event loop (`loop_stale_ms`,
`session.rs:247`) — which `docs/ideas.md:23-26` lists as a known gap.
Half of that idea already shipped: `pong` does carry
`loop_stale_ms` (`protocol-v1.md:357`, `_server.py:264`), the host just
never surfaces it typed.

**Chosen direction (v1, host-observed only — no wire change).** A
versioned, pull-based snapshot: `Handle::health(owner)` in Rust and
`health()` in `@pylair/client` via a new `python_health` command.
Contents: state + time in state, generation uptime, restart/breaker
accounting, last exit, liveness (pong age, RTT, misses, loop staleness)
with two derived verdicts — `transport: ok|missing` and
`loop: ok|busy` — plus load totals and **the calling window's own**
usage and ceilings, never another window's. `url` for the web kind.
Plus **one** new broadcast event, `pylair://health`, emitted only when
a verdict changes (justified in §5). And the observer effect is *fixed*,
not documented: event subscription and fairness liveness are split so
a state/health poller reserves no capacity (§3; lands as PR 0 in §7).

Scope boundary: this is *host observability of the supervised child*,
not web-server health — `protocol-v1.md:305` already decides that a
wrapped server's HTTP health is the launcher's job (the host sees an
ordinary child exit there). App-defined readiness is a separate future
decision (§6).

## 1. Field provenance — what exists, what is discarded, what is new

Already retained (mostly actor-side, none reachable by `Handle`
today):

| Field | Source today | Retained? |
|---|---|---|
| `state` | `state_tx` watch (`run.rs:147`, `195`) → `Handle::state` (`handle.rs:150`) | Yes, shared |
| `kind` | `Handle.kind` (`handle.rs:125`) | Yes, shared |
| `url` (web) | `ready_url` watch (`run.rs:148`, field `:198`) → `Handle::ready_url` (`handle.rs:163`) | Yes, shared |
| `generation` | `Supervisor.generation` (`run.rs:199`, `++` at `:378`, `:388`, `:423`, `:432`) | Actor-side only |
| `ready_since` | `Option<Instant>` (`run.rs:215`), set at `handshake.rs:116` (web) / `:123` (bridge), cleared `run.rs:348` | Actor-side only |
| crash window | `crashes: VecDeque<Instant>` (`run.rs:345`, slid at `:402-408`, checked `:409`) | Actor-side only |
| breaker limits | `config.breaker_crashes` / `breaker_window` (`config.rs:340-342`; defaults 5/60s at `:383-384`) | Retained config |
| `backoff` | `run_loop` local, doubled at `run.rs:431` | Actor-side only |
| `shutdown_outcome` | `Supervisor.shutdown_outcome` (`run.rs:219`) — already returned by `python_shutdown` | Actor-side; omit from health |
| task registry | `Session.tasks`/`pending` (`session.rs:42-45`) — per generation | Actor-side only |
| per-owner counts | `Fairness` map (`fairness.rs:76-79`) — **already shared** Handle/actor (`run.rs:145`, `handle.rs:138`) | Yes, shared |
| buffered stream bytes | `stream_bytes_total` `Arc<AtomicU64>` (`run.rs:152`, `handle.rs:130`) | Yes, shared |
| limits | `Arc<Limits>` (`run.rs:144`, `handle.rs:135`) | Yes, shared |

Measured today, then discarded:

| Field | Where it dies |
|---|---|
| pong `seq` | Echoed on the wire (`protocol-v1.md:143`; `_server.py:264`; `_launcher.py:504`); `session.rs:221` only does `pongs_received += 1` — the seq is never looked at, no send time kept → no RTT |
| `loop_stale_ms` | Parsed `session.rs:223`, classified `session.rs:224` (`classify_pong`, `run.rs:53`), then dropped — only a `detail` string survives |
| outstanding misses | `pongs_outstanding` computed per tick (`session.rs:88-92`), used, dropped |
| last exit | `ExitCause` (`run.rs:274-285`) is consumed by the `run_loop` match (`:371-433`) into `set_state` detail text and thrown away |
| state-entry time | `set_state` (`run.rs:288`) writes the watch on `from != to` but keeps no `Instant` — "time in state" is unmeasurable |

New state to keep — one shared block, mirroring the `FairnessRef`
precedent (`Arc<Mutex<…>>` created in `spawn()`, `run.rs:145`; Handle
gets the same `Arc`, `handle.rs:138`):

```rust
struct HealthShared {
    state_since: Instant,          // written in set_state on a real transition
    generation: u64,               // mirrored at the four ++ sites
    ready_at: Option<Instant>,     // mirrored with ready_since
    last_exit: Option<LastExit>,   // {kind, detail, at} — written in the cause match
    crashes_in_window: usize,      // mirrored from crashes.len()
    backoff: Duration,             // mirrored on change
    // per-generation liveness; reset when the generation ends:
    last_pong_at: Option<Instant>,
    last_rtt: Option<Duration>,
    last_loop_stale: Option<Duration>,
    missed_pongs: u32,             // current outstanding count
    ping_sent: BTreeMap<u64, Instant>, // seq -> queued-at; bounded (~missed_pongs_to_kill entries)
}
```

Write sites are the *existing* bookkeeping points: `set_state` (one
line), the `run_loop` cause match (one struct write per generation
end), the pong arm (`session.rs:220`), the ping tick (`session.rs:117`,
record `seq → Instant` when the body lands in `ping_slot` at `:129`). All are
rare or on the 5s `ping_interval` cadence (`config.rs:379`). **Hot-path
cost is zero**: no write on the per-call, per-chunk or per-frame path.
`state_since` rides `set_state`'s `from != to` guard, so the
informational `starting → starting` half-deadline event
(`handshake.rs:37-49`, which bypasses `set_state` entirely) does not
reset it — `in_state_ms` measures the real transition, not the last
detail refresh.

RTT semantics: `rtt = pong_arrival − ping_queued_at(seq)` — the
*observed* round trip including any writer-side stall, which is
exactly the number a health UI wants (a stalled writer is itself the
symptom). Recording send time inside the writer task instead is a
possible refinement; it would need to parse `seq` out of the queued
bytes and is not worth v1. A runtime that omits `seq` (pre-flag runtimes)
yields `rtt: null` — additive-optional, same precedent as
`loop_stale_ms` itself (`protocol-v1.md:367`).

Load totals — the subtlety: `Session.tasks` is actor-private and the
`Fairness` owner map is empty when `per_window_fairness` is off
(`fairness.rs:127`, `:164`). Two options:

- **(chosen) Unconditional totals inside `Fairness`.** A `totals:
  Counts` updated *before* the `enabled` early-returns in
  `note_started`/`note_finished`/`try_admit_queue`/`release_queue`/
  `end_generation` — methods that are already invoked unconditionally
  at every enumerated site (`session_commands.rs:89`, `:244`;
  `session.rs:205`, `:339`; `run.rs:620`; `handle.rs:197`, `:258`; the
  four `cmd_rx.recv()` release sites; `Session::drop` →
  `end_generation`, `session.rs:72`). Zero new call sites, zero new
  locks, zero new types — `FairnessSnapshot` gains a `totals` row.
  The `enabled` flag stays what it semantically is: *per-owner
  attribution*, not a counting switch.
- (rejected) Mirror totals in `HealthShared` at the same sites: a
  second lock and ~9 new write sites for identical data.
- (rejected for `queued` specifically) `cmd.capacity()` —
  `max_queued_commands − capacity` is a real answer but reads a
  different counter than admission uses; keep one source.

The caller's own row and its ceilings come from the existing map:
`FairnessSnapshot`/`OwnerUsage` (`fairness.rs:389-453`) already exposes
`usage`, `stream_usage`, `isolated_usage`, `is_live`; `ceiling_of`
(`fairness.rs:287`) needs a `pub(crate)` accessor. Under
`per_window_fairness=0` the map is empty and no per-owner accounting
exists to report, so `self` is `null` outright — honest absence, not
fabricated zeros (§4, §7 agree).

## 2. Busy vs hung — decomposing `unresponsive`

Two verdicts, each derived from data the pong path already produces,
each mapping onto one existing `Unresponsive` entry point:

| Verdict | Rule | Source / threshold |
|---|---|---|
| `transport: ok` | `missed_pongs == 0` | `pongs_outstanding` (`session.rs:88-92`) |
| `transport: missing` | `missed_pongs >= 1` | the existing real-miss rule (`session.rs:110-116`) |
| `loop: ok` | `last_loop_stale < 1s` | `LOOP_STALL_WARN` (`run.rs:39`) — below it the Python monitor's own 1s line would flap |
| `loop: busy` | `last_loop_stale >= 1s` | `classify_pong` → `StallWarn` (`run.rs:53-61`, `session.rs:247-250`) |
| `loop: unknown` | web kind | the launcher's pong hardcodes `loop_stale_ms: 0` — it cannot see the server's loop (`_launcher.py:504`) |

Mapping to `State`: `unresponsive` ⟺ `transport:missing` ∨
`loop:busy` — precisely today's two set-points (`session.rs:112`,
`session.rs:248`); `ready` requires both `ok`. The verdicts are a
*decomposition* of the state, not a replacement.

**`onState` semantics do not change.** No new states, no removed
transitions, `pylair://state` payload untouched; a client that only
watches states sees byte-identical behaviour. Two deliberate
fine-grain divergences, stated plainly:

- A `Ready`-verdict pong flips `Unresponsive → Ready` today even with
  misses still outstanding (`session.rs:251`); a health read then shows
  `state: "ready"` with `transport: "missing"` until the pipe drains.
  That is the finer signal doing its job — the verdict is per-pong
  truth, the state is the coarser summary. The state machine is *not*
  tightened to compensate.
- `set_state` dedupes (`from == to` → no event, `run.rs:289`), so a
  cause flip *within* `Unresponsive` — a missed-pong window becoming a
  stalled-loop window — emits nothing today. §5's event is what makes
  that visible.

Kill paths untouched: `missed_pongs_to_kill` (default 3,
`config.rs:380`) still ends the runtime as a crash; `loop_stall_kill`
(default off, `config.rs:358`, `:388`) still converts `StallKill` into
the same. A stall past a configured kill limit reads `loop:"busy"`
briefly before the generation dies — correct, not a third verdict.

## 3. Permissions: `pylair:default`, deniable, self-scoped

- **In `pylair:default` — yes.** `python_health` is the same class as
  `python_state`: read-only self-observation, no control power.
  `default.toml` already grants `allow-python-state` (`default.toml:13`);
  `security.md:60` reserves exclusion for supervisor *control*
  (`python_retry`/`python_shutdown`). A read cannot hold work, so it
  cannot starve anything.
- **Deniable — per origin, not per window** (corrected in PR 3, from
  the #121 test finding): `build.rs`'s `COMMANDS` list
  (`build.rs:4-14`) auto-generates `allow-`/`deny-python-health`
  permissions, but Tauri resolves a command deny on the execution
  context — `tauri::ipc::Authority::resolve_access` runs `is_denied`
  before label matching, and "window and webview labels are not taken
  into account for denial". A capability carrying
  `"pylair:deny-python-health"` therefore refuses the command on
  **every local webview**, whatever its `windows:` list names — the
  same origin-wide semantics the emit-deny analysis already documents
  (`docs/design/event-scoping.md:98-106`). Per-window denial is still
  achievable the allow-way: withhold `pylair:default` from that
  window's capability and grant the remaining commands one by one
  (an uncovered command answers "not allowed").
- **No `FnScope`.** Command scopes bind *function names* to the two
  call commands (`fn_scope.rs:60-73`; `security.md:121-138`); health
  invokes no function. It also stays out of the
  `pylair build`/`doctor` call-vs-call-stream asymmetry check
  (`security.md:191-193`) — that pairing does not apply.
- **Trust boundary.** `self.window` is `window.label()` from Tauri
  (`commands.rs:25`), never a JS-supplied value — the same rule
  `TaskMeta.owner` follows (`handle.rs:179-181`). The snapshot returns
  the *caller's* usage and ceilings; other windows' rows are never
  serialized (same containment doctrine as per-call channels and
  `stderr_targets`, `registry.rs:49-59`). Ceilings implicitly leak the
  live-window count (`ceiling_of` is a function of `live_count`,
  `fairness.rs:287-298`) — a number, not identities; accepted and
  documented.
- **Subscription is split: event delivery vs fairness liveness.**
  Today `subscribed()` does two jobs at once — marks the window a
  broadcast target *and* joins the fairness live set
  (`registry.rs:32-37`) — so a window that only polls `python_state`
  reserves `min_reserve` of every cap forever: the observer effect.
  The fix splits them. `subscribed()` (broadcast target) stays on
  every bridge command, unchanged — a health-only monitor window still
  receives `pylair://health` events. Joining the live set moves to the
  two call commands, `python_call` and `python_call_stream`, at their
  existing subscription site (`commands.rs:21`, `commands.rs:88`,
  before `rt.0.call*` at `:22`/`:91`): a window is live once it can
  admit work, not once it can look. A state/health/cancel/retry-only
  window receives events and reserves nothing; `python_cancel` and
  `grant_credit` need no join because a window can own tasks only via
  a call command. 0005's first-call guarantee is preserved by
  construction — the join still lands before the window's first
  admission check, so "its guarantee applies to the very first call"
  (0005 §3) holds exactly. `WindowRegistry::drop_label` still leaves
  both sets on `Destroyed` (`registry.rs:64-69`); `leave_window` on a
  label that never joined is a no-op (`fairness.rs:99-103`), so no
  special-casing is needed. The existing two-window fairness selftest
  stays green because `b_ping` is a `python_call` — it remains win-b's
  join point. This change is PR 0 in §7 and amends 0005 §3.

## 4. Snapshot shape (v1) — minimal, versioned, kind-aware

```jsonc
{
  "version": 1,
  "kind": "bridge",                  // "bridge" | "web"
  "state": "ready",                  // fmt_state names (util.rs:87-96)
  "in_state_ms": 12000,
  "generation": 2,
  "uptime_ms": 11800,                // null until this generation is Ready
  "url": null,                       // web kind only: present, null until ready
  "restarts": {
    "crashes_in_window": 1,          // crashes.len() after window slide
    "breaker": { "crashes": 5, "window_ms": 60000 },
    "backoff_ms": 500,               // the backoff that will apply next
    "last_exit": {                   // null until the first generation ends
      "kind": "crashed",             // crashed | fatal | dev_restart
      "detail": "exited code=1",     // free text — informational only (protocol-v1:233)
      "ms_ago": 3100
    }
  },
  "liveness": {
    "last_pong_age_ms": 2100,        // null before the first pong
    "rtt_ms": 3,                     // null if seq unechoed/unmatched
    "missed_pongs": 0,
    "missed_pongs_to_kill": 3,
    "loop_stale_ms": 0,              // bridge kind only (absent for web)
    "transport": "ok",               // ok | missing | unknown
    "loop": "ok"                     // ok | busy | unknown
  },
  "load": {
    "tasks": 3, "streams": 1, "isolated": 0, "queued": 0,
    "stream_bytes_total": 4096
  },
  "self": {                          // null for host-side health() and
    "window": "main",                // when per_window_fairness is off
    "tasks":    { "in_use": 3, "ceiling": 56, "cap": 64 },
    "streams":  { "in_use": 1, "ceiling": 28, "cap": 32 },
    "queued":   { "in_use": 0, "ceiling": 56, "cap": 64 },
    "isolated": { "in_use": 0, "ceiling": 1,  "cap": 8 }
  }
}
```

Optional-per-kind rules: `url` present only for `kind:"web"` (same
shape `python_state` already answers, `util.rs:80-85`); `liveness.loop`
/ `loop_stale_ms` present only for `kind:"bridge"` — the web launcher's
pong reports `loop_stale_ms: 0` because it cannot measure the wrapped
server's loop (`_launcher.py:501-505`), so `loop` reads `"unknown"`,
not a fabricated `"ok"`. Outside a live generation (`starting`,
`restarting`, `failed`, `stopped`) the liveness fields null out and
both verdicts read `"unknown"`. `self` is `null` in two cases —
`per_window_fairness` is off (no per-owner accounting exists to
report; §1), or the caller is host-side (`Handle::health(None)`).

Durations are milliseconds-suffix numbers measured at snapshot time —
no epoch stamps, no clock agreement to negotiate (`*_ms` matches
`loop_stale_ms`, `grace_ms`, `hello_timeout_ms` conventions).

**Deliberately left out:** other windows' usage (containment); `detail`
text as data (protocol-v1:233 — informational only, never API);
per-task enumeration and `done_tasks` (privacy + unbounded); raw
`pings_sent`/`pongs_received` counters (`missed_pongs` carries the
signal); `shutdown_outcome` (`python_shutdown` already returns it);
`fn_kinds` (already on `pylair://exposes`); stderr lines (own channel);
stream credit and per-stream byte breakdowns. `version` is the only
escape hatch: additive fields are allowed inside v1; a shape change
bumps it.

`Handle::health(owner: Option<&str>) -> HealthSnapshot` is a new
**public**, typed API: a `#[non_exhaustive]` `HealthSnapshot` struct —
the `Limits`/`SupervisorConfig` precedent (`config.rs:79`, `:322`) —
serde-serialized by the command, not a `serde_json::Value` handed
through. `self` is `null` for `None`; the command layer passes
`Some(label)`. `#[non_exhaustive]` is what makes "additive fields are
allowed inside v1" semver-safe for *Rust* callers too — a downstream
`match` or struct literal cannot pin the v1 shape — on top of the
JSON-side rule above. This is a real semver surface, unlike the
`#[doc(hidden)]` `fairness_snapshot` (`handle.rs:356-359`).

## 5. The `pylair://health` event — justified, one event, transitions only

Emit `{version, generation, liveness:{…}}` to `broadcast_targets`
(`registry.rs:41-43`) when either verdict changes, detected at the two
sites that move them (the pong arm and the ping tick).

**Why it earns its existence:**

- The motivating use is a "the runtime is busy, calls are slow" banner.
  Driven by polling `health()` it costs an invoke per window per
  second, forever; driven by the event it costs one emit per *change*
  — in practice a handful per session.
- Polling *cannot* substitute for one case: the intra-state verdict
  flip. `set_state` dedupes, so `unresponsive(miss) →
  unresponsive(stall)` is invisible to `onState`; only a verdict event
  or a poller sees it.
- It is strictly additive: a new `RuntimeEvent` variant
  (`config.rs:36-46`) plus an `events::HEALTH` constant
  (`lib.rs:34-41`); the `builder.rs:208-217` match is exhaustive, so
  the compiler forces the emit wiring — it cannot be forgotten.
  Rust-side `emit_to` is not subject to webview `deny-emit`
  (`security.md:265-270`).

**Rejected alternatives:** fold the verdicts into `pylair://state` —
the payload would only change at transitions, and verdicts can flip
*without* one (the case above); or no event at all — forces every
banner onto a poll loop.

**Emission baseline.** "One per flip" needs a remembered last-emitted
pair, generation-scoped:

- Generation 1's baseline is the *assumed* steady value for the
  runtime kind — `{ok, ok}` for bridge, `{ok, unknown}` for web
  (its loop is never measured) — since the runtime just reached
  Ready and a literal steady-state first emit carries no news. The
  first verdict emits only when it differs (e.g. `{ok, busy}` for
  bridge); the `0 → 2500 → 0` staleness flap therefore emits
  exactly twice, busy then ok, and a wrapped app's first pong
  emits nothing.
- Teardown emits `{unknown, unknown}` only when the generation
  actually emitted a verdict — "a real change from the last emitted
  value"; a generation that never emitted stays silent.
- The reset then reseeds the baseline to the `{unknown, unknown}`
  observers last saw, so a restarted generation's first verdict always
  emits — even when it reads `{ok, ok}` — because it *is* news to an
  observer still showing the teardown verdict.

## 6. App-defined readiness — a separate future decision

Not in v1. Sketch only: a Python-side `pylair.report(...)` /
`readiness` notification carrying `{phase, progress}` would give apps a
typed "model loading 40%" channel — the `ready`-notification precedent
(`protocol-v1.md:145`, `:301`) shows the protocol absorbs additive
runtime→host notifications without a version bump, and unknown
notifications are already ignored (`session.rs:365`). The host would
store the latest report per generation and surface it as
`health().app`. It needs its own decision record: it *is* a wire
addition (unlike everything else here), it wants a Python API surface,
and it interacts with §5's event design. Do not design it in this PR.

## 7. Test plan

- **`tests/runtime_health.rs`** (new), reusing the `spawn_h` +
  inline-script fake harness (`window_fairness.rs:19-66`):
  - a fake that pongs `loop_stale_ms: 2500` → `loop:"busy"`,
    `loop_stale_ms:2500`, `state:"unresponsive"`, and a
    `pylair://health` event on the flip — the busy-vs-hung split
    proven at the boundary;
  - a fake that stops ponging → `transport:"missing"`, `missed_pongs`
    climbing at tick cadence, state `unresponsive`, then the existing
    kill path;
  - seq-echoing fake → `rtt_ms` non-null; a fake that omits `seq` →
    `rtt_ms` null (optional-field behaviour);
  - crash-and-restart → `last_exit.kind:"crashed"`, non-empty detail,
    `crashes_in_window:1`, `generation` incremented, `uptime_ms` reset;
  - `per_window_fairness` **off** → `self:null` and `load.*` totals
    still counting — the unconditional-totals assertion of §1;
  - `self` ceilings shrink for a second owner (the
    `join_window`-in-test path, `handle.rs:365-373`);
  - verdict-change event: exactly one `pylair://health` per flip, none
    on steady state.
- **PR-0 liveness split** (command path, `call_via_command.rs`
  precedent): a window that only invokes `python_state` does not
  shrink another owner's ceiling, and its first `python_call` joins
  the live set — `python_health` inherits the same non-joining rule
  once it exists;
- **Command path** (`call_via_command.rs` precedent): `python_health`
  returns caller-scoped `self` — win-b's answer names `win-b`, shows
  its own usage, never main's row.
- **ACL**: a capability denying `python-health` refuses the invoke at
  the Tauri layer (same class of test as the existing per-command ACLs
  in `event_scoping.rs`/`plugin_setup.rs`).
- **Client** (`selftest-lazy.test.js` precedent, mock bridge):
  `health()` returns the typed snapshot; `onHealth` subscribes and
  unlistens; against an *old* plugin the invoke rejection propagates
  as a typed failure, never a hang (same contract as `retry()`).
- **Installed selftest — yes, assert it end to end.** Add a `health`
  check to `selftest.ts` beside `window-fairness`
  (`selftest.ts:224`): `version==1`, `state=="ready"`,
  `transport=="ok"`, `self.window=="main"`. It is the same class of
  proof the fairness leg added — the real bridge, real ACL, real
  snapshot — on all three OS acceptance legs at near-zero cost.

## Implementation plan (one concern per PR)

0. **Observer-effect fix (lands before health PR 1).** Split
   `WindowRegistry` subscription into event subscription (every bridge
   command, unchanged) and fairness liveness (joined only by
   `python_call`/`python_call_stream`, before dispatch). Amend 0005 §3
   with a one-line note recording the split. Test (command path,
   `call_via_command.rs` precedent): a window that only polls
   `python_state` does not shrink another window's ceiling, and its
   first `python_call` joins it. The existing
   two-window fairness selftest stays green — `b_ping` is a
   `python_call`.
1. **Collection.** `HealthShared` + actor writes + unconditional
   `Fairness` totals + `Handle::health(owner)` +
   `tests/runtime_health.rs` (snapshot assertions only — no command,
   no event; the 0005-PR-1 pattern: count and observe before
   surfacing).
2. **Command.** `python_health` in `commands.rs` + `build.rs`
   `COMMANDS` + `pylair:default` entry + `security.md` table row +
   command/ACL tests (caller-scoped `self`). Depends on PR 0: the
   health poller must subscribe (for events) without joining the live
   set.
3. **Event.** `RuntimeEvent::HealthChanged` + `events::HEALTH` +
   builder emit + transition tests.
4. **Client.** `health()`/`onHealth` + `HealthSnapshot` types +
   `CMD`/`EVT` entries (`internal.ts:12-21`, `index.ts:155-159`) +
   mock tests + `packages/client` docs.
5. **Selftest + docs.** `selftest.ts` `health` check through the real
   webview; `protocol-v1.md` §5 cross-reference (host observability
   surface — the wire is untouched, so a doc *note*, not a contract
   change) and README capability list.
