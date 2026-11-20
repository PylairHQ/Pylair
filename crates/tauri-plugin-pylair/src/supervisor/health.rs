//! Host-observed runtime health (docs/decisions/0006 §1, §2, §4): one
//! shared block the actor writes ONLY at the bookkeeping points the
//! decision lists — state transitions, generation/restart accounting,
//! the ping tick, the pong arm, generation teardown — and
//! `Handle::health` reads into a typed, versioned snapshot. Nothing is
//! written on the per-call, per-chunk or per-frame hot path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Shared with the actor — the `FairnessRef` precedent
/// (docs/decisions/0006 §1).
pub(super) type HealthRef = Arc<Mutex<HealthShared>>;

/// How a generation ended — the `last_exit.kind` vocabulary of the §4
/// schema. `CleanShutdown` is deliberately absent: the `restarts`
/// block reports restart/crash accounting, and an operator stop is
/// terminal, not an exit cause in that sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitKind {
    /// The child died on its own (EOF, signal, protocol violation).
    Crashed,
    /// Unrecoverable (protocol mismatch): supervisor parked `failed`.
    Fatal,
    /// `PYLAIR_DEV_RESTART` file trigger fired.
    DevRestart,
}

/// The last generation's end cause and when it happened.
#[derive(Debug, Clone)]
pub(super) struct LastExit {
    pub(super) kind: ExitKind,
    pub(super) detail: String,
    pub(super) at: Instant,
}

/// The actor-side health block. Field-for-field the 0006 §1 proposal:
/// `state_since`/`generation`/`ready_at`/`last_exit`/`crashes_in_window`/
/// `backoff` are lifecycle mirrors; the rest are per-generation
/// liveness, cleared by [`HealthShared::reset_generation`].
#[derive(Debug)]
pub(super) struct HealthShared {
    /// `set_state` last made a REAL transition (`in_state_ms` reads
    /// this). The informational `starting → starting` detail refresh
    /// bypasses `set_state`, so it cannot reset the clock.
    pub(super) state_since: Instant,
    /// The actor's `generation` counter, mirrored at each increment.
    pub(super) generation: u64,
    /// `ready_since` mirrored — the current generation's Ready time;
    /// `None` until it is Ready and once it ends.
    pub(super) ready_at: Option<Instant>,
    /// How the last generation ended; `None` until the first does.
    pub(super) last_exit: Option<LastExit>,
    /// `crashes.len()` after the breaker window slide.
    pub(super) crashes_in_window: usize,
    /// The backoff that will apply to the next crash, mirrored on
    /// change.
    pub(super) backoff: Duration,
    /// Arrival time of the newest pong.
    pub(super) last_pong_at: Option<Instant>,
    /// Queued-to-arrival RTT of the newest pong's echoed seq; `None`
    /// when the pong echoes no matchable seq (§4: "null if seq
    /// unechoed/unmatched").
    pub(super) last_rtt: Option<Duration>,
    /// `loop_stale_ms` of the newest pong, verbatim.
    pub(super) last_loop_stale: Option<Duration>,
    /// Outstanding pings the writer actually sent, mirrored each
    /// ping tick and each pong.
    pub(super) missed_pongs: u32,
    /// seq -> when its ping frame was queued to the writer. Bounded
    /// at ~`missed_pongs_to_kill` entries — a wedged runtime cannot
    /// grow it.
    pub(super) ping_sent: BTreeMap<u64, Instant>,
}

/// The shared block, created in `spawn()` beside the fairness map.
pub(super) fn new(backoff: Duration) -> HealthRef {
    Arc::new(Mutex::new(HealthShared {
        state_since: Instant::now(),
        generation: 0,
        ready_at: None,
        last_exit: None,
        crashes_in_window: 0,
        backoff,
        last_pong_at: None,
        last_rtt: None,
        last_loop_stale: None,
        missed_pongs: 0,
        ping_sent: BTreeMap::new(),
    }))
}

impl HealthShared {
    /// `set_state` made a real transition.
    pub(super) fn note_transition(&mut self) {
        self.state_since = Instant::now();
    }

    /// The actor bumped `generation`.
    pub(super) fn note_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    /// `ready_since` mirrored (set at the Ready transition, cleared at
    /// run_loop top and at generation teardown).
    pub(super) fn note_ready(&mut self, at: Option<Instant>) {
        self.ready_at = at;
    }

    /// The generation ended with a restart-relevant cause (crashed,
    /// fatal, dev restart).
    pub(super) fn note_exit(&mut self, kind: ExitKind, detail: String) {
        self.last_exit = Some(LastExit {
            kind,
            detail,
            at: Instant::now(),
        });
    }

    /// `crashes`/`backoff` mirrored from `run_loop`'s restart
    /// accounting.
    pub(super) fn note_restart_accounting(&mut self, crashes_in_window: usize, backoff: Duration) {
        self.crashes_in_window = crashes_in_window;
        self.backoff = backoff;
    }

    /// The ping tick computed the outstanding-miss count.
    pub(super) fn note_missed(&mut self, missed: u32) {
        self.missed_pongs = missed;
    }

    /// The writer queued a ping for `seq`.
    pub(super) fn note_ping(&mut self, seq: u64, bound: usize) {
        self.ping_sent.insert(seq, Instant::now());
        while self.ping_sent.len() > bound {
            self.ping_sent.pop_first();
        }
    }

    /// A pong arrived: stamp its age/RTT/loop staleness and the
    /// miss count it just settled.
    pub(super) fn note_pong(&mut self, seq: Option<u64>, loop_stale: Duration, missed: u32) {
        let now = Instant::now();
        self.last_pong_at = Some(now);
        self.last_rtt = seq
            .and_then(|s| self.ping_sent.remove(&s))
            .map(|sent| now.saturating_duration_since(sent));
        self.last_loop_stale = Some(loop_stale);
        self.missed_pongs = missed;
    }

    /// Generation teardown (`Session::drop`): the per-generation
    /// liveness block dies with it. Outside a live generation
    /// `Handle::health` reports these null and both verdicts
    /// `"unknown"` regardless — the reset keeps the shared block
    /// honest for reads in between.
    pub(super) fn reset_generation(&mut self) {
        self.ready_at = None;
        self.last_pong_at = None;
        self.last_rtt = None;
        self.last_loop_stale = None;
        self.missed_pongs = 0;
        self.ping_sent.clear();
    }
}

// ------------------------------------------------------------------
// `Handle::health`'s answer — the 0006 §4 v1 schema, typed. Serde
// field names ARE the schema; `#[non_exhaustive]` makes additive v1
// fields semver-safe for Rust callers too.
// ------------------------------------------------------------------

/// A point-in-time observability snapshot of the supervised runtime
/// (docs/decisions/0006 §4).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct HealthSnapshot {
    /// Schema version — `1`.
    pub version: u32,
    /// `"bridge"` or `"web"`.
    pub kind: &'static str,
    /// The `fmt_state` state name.
    pub state: &'static str,
    /// Milliseconds since `state` was entered.
    pub in_state_ms: u64,
    /// Runtime generation — bumps on every restart.
    pub generation: u64,
    /// Milliseconds since this generation went `ready`; `null` until
    /// it does.
    pub uptime_ms: Option<u64>,
    /// Web kind only: `Some(None)` reads as `"url": null` until the
    /// server reports `ready`; the field is absent entirely for the
    /// bridge kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<Option<String>>,
    pub restarts: RestartsSnapshot,
    pub liveness: LivenessSnapshot,
    pub load: LoadSnapshot,
    /// The calling window's own usage and ceilings — `null` for
    /// `health(None)` and when `per_window_fairness` is off. Never
    /// another window's data.
    #[serde(rename = "self")]
    pub self_usage: Option<SelfSnapshot>,
}

/// Crash-loop accounting (`restarts` block).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct RestartsSnapshot {
    /// Crashes within the breaker window.
    pub crashes_in_window: u64,
    pub breaker: BreakerSnapshot,
    /// The backoff that will apply to the next crash.
    pub backoff_ms: u64,
    /// `null` until the first generation ends.
    pub last_exit: Option<LastExitSnapshot>,
}

/// The configured crash-loop breaker.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct BreakerSnapshot {
    /// Crashes inside `window_ms` that park the supervisor `failed`.
    pub crashes: u64,
    /// Breaker window length.
    pub window_ms: u64,
}

/// How the last generation ended.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct LastExitSnapshot {
    /// `crashed` | `fatal` | `dev_restart`.
    pub kind: ExitKind,
    /// Free-text detail — informational only.
    pub detail: String,
    /// Milliseconds since it happened.
    pub ms_ago: u64,
}

/// Ping/pong liveness and the two derived verdicts
/// (docs/decisions/0006 §2).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct LivenessSnapshot {
    /// `null` before the first pong.
    pub last_pong_age_ms: Option<u64>,
    /// `null` when the newest pong echoed no matchable `seq`.
    pub rtt_ms: Option<u64>,
    /// Pings the writer sent with no pong yet.
    pub missed_pongs: u32,
    /// The kill threshold of the same name.
    pub missed_pongs_to_kill: u32,
    /// Bridge kind only — absent for web (a launcher pong cannot see
    /// the wrapped server's loop). `Some(None)` reads as `null`
    /// until the first pong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_stale_ms: Option<Option<u64>>,
    /// `ok` when no ping is unanswered; `missing` once one is;
    /// `unknown` outside a live generation.
    pub transport: &'static str,
    /// `ok` under 1s of loop staleness, `busy` at or over it;
    /// `unknown` for the web kind and outside a live generation.
    #[serde(rename = "loop")]
    pub loop_verdict: &'static str,
}

/// Global in-flight totals — populated even when
/// `per_window_fairness` is off (docs/decisions/0006 §1).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct LoadSnapshot {
    /// All in-flight tasks (plain calls, streams, isolated workers).
    pub tasks: u64,
    /// Of `tasks`, how many are streams.
    pub streams: u64,
    /// Of `tasks`, how many are isolated workers.
    pub isolated: u64,
    /// Commands waiting in the actor queue.
    pub queued: u64,
    /// Bytes buffered for ALL stream consumers.
    pub stream_bytes_total: u64,
}

/// One `{in_use, ceiling, cap}` cell of `self`.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SelfResource {
    /// The caller's own count of the resource.
    pub in_use: u64,
    /// Its decisions-0005 ceiling right now.
    pub ceiling: u64,
    /// The configured global cap.
    pub cap: u64,
}

/// The calling window's own usage and ceilings.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SelfSnapshot {
    /// The webview label the command layer passed.
    pub window: String,
    pub tasks: SelfResource,
    pub streams: SelfResource,
    pub queued: SelfResource,
    pub isolated: SelfResource,
}
