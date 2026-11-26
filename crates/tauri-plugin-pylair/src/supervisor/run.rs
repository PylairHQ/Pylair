//! The supervisor actor: `spawn()`, the `Supervisor` event loop and
//! restart policy (docs/protocol-v1.md section 7), `run_child` (one runtime
//! generation from hello to teardown; its phases are listed at its top)
//! and the helpers that fail or drain pending work.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, timeout};

use super::config::{
    clamp_hello_timeout_ms, clamp_ready_timeout_ms, parse_hello_timeout_ms, RuntimeEvent,
    SpawnSpec, State, SupervisorConfig,
};
use super::fairness::{release_queued, Fairness, FairnessRef};
use super::handle::{CancelOutcome, Command, Handle, ShutdownOutcome};
use super::health::{self, ExitKind, HealthRef};
#[cfg(target_os = "linux")]
use super::platform_unix::PdeathAnchor;
use super::spawn::spawn_child;
use super::stream::StreamSink;
use super::wire::{cancel_outcome, jitter, mtime, Ctrl};
use crate::blobs::{BlobLimits, BlobRegistry, BlobRegistryRef};
use crate::protocol::{self as proto, ErrorObj, InMsg};

mod handshake;
mod session;
mod session_commands;

use session::Session;

/// Stale-loop duration at which pongs surface an `Unresponsive` diagnostic.
/// Below the Python lag monitor's 1s threshold this would flap on jitter.
pub(super) const LOOP_STALL_WARN: Duration = Duration::from_secs(1);

/// What a pong's `loop_stale_ms` means for the runtime. Pure so the
/// verdict boundaries are unit-testable without a clock or a process:
/// a configured kill wins over the warning, and with no kill configured
/// a stalled loop is only ever a diagnostic (P1-7: model loads can block
/// the loop for minutes on a healthy app).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PongVerdict {
    Ready,
    StallWarn,
    StallKill,
}

/// Release-mode defense in depth: remove PYLAIR_DEV_TRACEBACKS from both
/// channels a child could receive it through — any value the spec itself
/// carries, and inheritance from this process's environment (a release
/// app launched from a dev shell). Not cfg-gated so tests can drive it
/// directly; the only caller is the !debug_assertions branch above.
pub fn strip_dev_tracebacks(spec: &mut crate::supervisor::config::SpawnSpec) {
    spec.env.retain(|(k, _)| k != "PYLAIR_DEV_TRACEBACKS");
    if !spec.env_remove.iter().any(|k| k == "PYLAIR_DEV_TRACEBACKS") {
        spec.env_remove.push("PYLAIR_DEV_TRACEBACKS".into());
    }
}

pub(super) fn classify_pong(stale: Duration, kill: Option<Duration>) -> PongVerdict {
    if kill.is_some_and(|l| stale >= l) {
        PongVerdict::StallKill
    } else if stale >= LOOP_STALL_WARN {
        PongVerdict::StallWarn
    } else {
        PongVerdict::Ready
    }
}

/// Per-task metadata for ownership, kind and credit tracking (RC.6A/RC.8A).
struct TaskMeta {
    call_id: u64,
    /// The exposed function the call named — host-side bookkeeping only
    /// (already on the wire frame), kept so a stall warning can name the
    /// in-flight call(s) likely holding the loop.
    fn_name: String,
    owner: Option<String>,
    /// Streaming tasks count toward `max_concurrent_streams`.
    is_stream: bool,
    /// Isolated-worker tasks count toward `max_isolated_workers`
    /// (per owner; the Python-side cap stays the global backstop).
    is_isolated: bool,
    /// Outstanding pull-credit: initial grant + `GrantCredit`s minus
    /// delivered chunks (RC.8A cap).
    credit: u64,
}

enum PendingKind {
    Call(oneshot::Sender<Result<Value, ErrorObj>>),
    Stream(StreamSink),
    Cancel {
        respond: oneshot::Sender<CancelOutcome>,
    },
}

/// Create the supervisor actor. Returns the handle plus the actor future —
/// the CALLER drives it (tokio::spawn / tauri::async_runtime::spawn) because
/// bare `tokio::spawn` panics outside an entered runtime, and plugin `setup`
/// runs on the main thread where no reactor is entered.
pub fn spawn(
    spec: SpawnSpec,
    mut config: SupervisorConfig,
    events: crate::eventbus::EventBus,
) -> (
    Handle,
    impl std::future::Future<Output = ()> + Send + 'static,
) {
    let (limits, warns) = config.limits.validated();
    for w in &warns {
        events.emit(RuntimeEvent::Stderr(format!("pylair limits: {w}")));
    }
    // hello_timeout: surface a malformed knob (the value was already
    // resolved inside SupervisorConfig::default()) and clamp whatever
    // source the millis came from — env, Builder, or a hand-built config.
    if let (_, Some(w)) = parse_hello_timeout_ms(std::env::var("PYLAIR_HELLO_TIMEOUT_MS").ok()) {
        events.emit(RuntimeEvent::Stderr(format!("pylair config: {w}")));
    }
    let (hello_ms, warn) = clamp_hello_timeout_ms(config.hello_timeout.as_millis() as u64);
    config.hello_timeout = Duration::from_millis(hello_ms);
    if let Some(w) = warn {
        events.emit(RuntimeEvent::Stderr(format!("pylair config: {w}")));
    }
    // ready_timeout only matters for the Web kind, but a bad value is
    // clamped (and reported) whichever kind is configured.
    let (ready_ms, warn) = clamp_ready_timeout_ms(config.ready_timeout.as_millis() as u64);
    config.ready_timeout = Duration::from_millis(ready_ms);
    if let Some(w) = warn {
        events.emit(RuntimeEvent::Stderr(format!("pylair config: {w}")));
    }
    // An initial grant above the cap would violate the cap immediately.
    if config.initial_stream_credit > limits.max_stream_credit {
        events.emit(RuntimeEvent::Stderr(format!(
            "pylair limits: initial_stream_credit={} exceeds max_stream_credit={} — clamped",
            config.initial_stream_credit, limits.max_stream_credit
        )));
        config.initial_stream_credit = limits.max_stream_credit;
    }
    events.emit(RuntimeEvent::Stderr(format!(
        "pylair limits: tasks={} streams={} queue={} stream_bytes={}/{} credit={} workers={} frame={} window_share={}",
        limits.max_concurrent_tasks,
        limits.max_concurrent_streams,
        limits.max_queued_commands,
        limits.stream_buffer_bytes,
        limits.stream_buffer_bytes_total,
        limits.max_stream_credit,
        limits.max_isolated_workers,
        config.max_frame,
        if limits.per_window_fairness {
            "on"
        } else {
            "off"
        },
    )));
    config.limits = limits.clone();
    let limits = Arc::new(limits);
    let fair = Fairness::new(limits.per_window_fairness);
    // The blob registry (decision 0010) — shared with the scheme handler
    // and window-destroy hook through `Handle`, same as `fair`/`health`.
    let (blobs, blob_warns) = BlobRegistry::new(
        config
            .blob_limits
            .clone()
            .unwrap_or_else(BlobLimits::from_env),
    );
    for w in blob_warns {
        events.emit(RuntimeEvent::Stderr(format!("pylair limits: {w}")));
    }
    // Crash residue from a previous boot: no generation owns the
    // `gen-*` dirs under the root yet (decision 0010 §6 startup sweep).
    if let Some(root) = &config.blob_root {
        let removed = BlobRegistry::sweep(root);
        if removed > 0 {
            events.emit(RuntimeEvent::Stderr(format!(
                "pylair: swept {removed} stale blob generation dir(s) under {}",
                root.display()
            )));
        }
    }
    // The health block (decisions 0006 §1) — shared with Handle like
    // the fairness map; written only at bookkeeping points, never on
    // a hot path.
    let health = health::new(config.backoff_base);
    // The dev-only inspector tap (docs/inspector.md): compiled to a
    // no-op shell in release, inert in debug unless PYLAIR_INSPECT=1.
    // Never feeds the events channel — the page polls it directly.
    let inspect = crate::inspect::InspectRef::new(
        std::env::var_os("PYLAIR_INSPECT").is_some_and(|v| v == "1"),
        std::env::var_os("PYLAIR_INSPECT_PAYLOADS").is_some_and(|v| v == "1"),
    );
    let (cmd_tx, cmd_rx) = mpsc::channel(limits.max_queued_commands);
    let (state_tx, state_rx) = watch::channel(State::Starting);
    let (ready_url_tx, ready_url_rx) = watch::channel(None::<String>);
    let kind = config.kind;
    let stream_buffer = config.stream_buffer;
    let cancel_timeout = config.cancel_timeout;
    let missed_pongs_to_kill = config.missed_pongs_to_kill;
    let breaker_crashes = config.breaker_crashes;
    let breaker_window = config.breaker_window;
    let stream_bytes_total = Arc::new(AtomicU64::new(0));
    let next_task = Arc::new(AtomicU64::new(1));
    let actor = Supervisor {
        spec,
        config,
        events,
        cmd_rx,
        state_tx,
        ready_url_tx,
        generation: 0,
        next_id: AtomicU64::new(1),
        next_task: next_task.clone(),
        fair: fair.clone(),
        health: health.clone(),
        inspect: inspect.clone(),
        fn_kinds: HashMap::new(),
        fn_warm: std::collections::HashSet::new(),
        ready_since: None,
        shutdown_acks: Vec::new(),
        shutdown_outcome: None,
        blobs: blobs.clone(),
        #[cfg(target_os = "linux")]
        pdeath_anchor: None,
    }
    .run();
    (
        Handle {
            cmd: cmd_tx,
            state: state_rx,
            ready_url: ready_url_rx,
            kind,
            stream_buffer,
            cancel_timeout,
            stream_bytes_total,
            next_task,
            missed_pongs_to_kill,
            breaker_crashes,
            breaker_window,
            limits,
            fair,
            health,
            inspect,
            blobs,
        },
        actor,
    )
}

struct Supervisor {
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: crate::eventbus::EventBus,
    cmd_rx: mpsc::Receiver<Command>,
    state_tx: watch::Sender<State>,
    /// The current generation's validated `ready` url (Web kind): set just
    /// before the Ready transition, cleared when the generation ends.
    ready_url_tx: watch::Sender<Option<String>>,
    generation: u64,
    next_id: AtomicU64,
    /// Shared with `Handle` so a page-load can watermark "tasks that
    /// existed before this navigation" — see `cancel_owner_before`.
    next_task: Arc<AtomicU64>,
    /// Per-owner fairness counters shared with `Handle`
    /// (docs/decisions/0005). The actor releases `in_queue` at every
    /// `cmd_rx.recv()` site and tracks `in_flight` via the Session.
    fair: FairnessRef,
    /// Host-observed health mirrors shared with `Handle`
    /// (docs/decisions/0006 §1) — written only at bookkeeping points.
    health: HealthRef,
    /// The dev-only inspector tap (docs/inspector.md) — see `inspect.rs`.
    pub(super) inspect: crate::inspect::InspectRef,
    /// The current generation's `fn_name -> kind` map, parsed out of
    /// `hello["exposes"]` when the handshake succeeds and replaced on
    /// every new generation (a dev reload's manifest can differ). An
    /// absent name counts as non-isolated.
    fn_kinds: HashMap<String, String>,
    /// Warm-pooled fns of the current generation (0008) — inspector mode.
    pub(super) fn_warm: std::collections::HashSet<String>,
    /// When the current generation reached Ready — a crash after a sustained
    /// ready period is a fresh incident, not a boot loop.
    ready_since: Option<Instant>,
    /// Waiters for a completed shutdown, released when the actor stops.
    shutdown_acks: Vec<oneshot::Sender<ShutdownOutcome>>,
    /// What the most recent shutdown actually achieved (RC.6C).
    shutdown_outcome: Option<ShutdownOutcome>,
    /// Live blob handles (decision 0010): `blob.register` resolves the
    /// owner through `Session.tasks`, `Session::drop` wipes the
    /// generation's entries and dir.
    blobs: BlobRegistryRef,
    /// Linux only: the one long-lived pdeathsig anchor thread every spawn
    /// in this supervisor's lifetime goes through (created lazily).
    /// Dropping it at actor exit closes the job channel; the thread then
    /// exits — its parked children were already reaped, and any that were
    /// not get their SIGKILL from the anchor's death either way.
    #[cfg(target_os = "linux")]
    pdeath_anchor: Option<PdeathAnchor>,
}

/// Per-child IO plumbing, owned by the per-spawn scope.
pub(super) struct ChildIo {
    /// Push raw frame bodies to write to child stdin.
    pub(super) frame_out: mpsc::Sender<Vec<u8>>,
    /// Control frames (cancel / credit / shutdown) — a separate bounded
    /// lane the writer task always drains first, so a data flood cannot
    /// starve or silently drop control (RC.6B).
    pub(super) ctrl_out: mpsc::Sender<Ctrl>,
    /// Liveness slot: the actor overwrites it each ping tick and fires
    /// `ping_notify`; the writer takes and writes the latest. One slot,
    /// never a queue - a stalled writer cannot pile up pings and crowd
    /// out real control frames.
    pub(super) ping_slot: Arc<Mutex<Option<Vec<u8>>>>,
    /// Wakes the writer so it drains `ping_slot` - the frame select alone
    /// would sleep through a ping when no other traffic exists.
    pub(super) ping_notify: Arc<tokio::sync::Notify>,
    /// Ping frames actually written to the child, bumped by the writer
    /// after each successful write. Only a ping the runtime could have
    /// answered counts toward the missed-pong kill - a ping still
    /// sitting in the slot behind a full pipe is backpressure, not a
    /// miss.
    pub(super) pings_sent: Arc<AtomicU64>,
    /// Received decoded incoming messages / transport errors.
    pub(super) msg_in: mpsc::Receiver<Result<InMsg, ChildReadErr>>,
    /// Exit status of the child process.
    pub(super) exited: oneshot::Receiver<io::Result<std::process::ExitStatus>>,
    /// Signal the monitor to kill the child process tree.
    pub(super) kill: mpsc::Sender<()>,
    /// The child pid as the spawn reported it — the inspector's spawn
    /// timeline entry; not read by any supervision logic.
    pub(super) pid: Option<u32>,
    // stderr is drained by a detached task -> events.
}

pub(super) enum ChildReadErr {
    Io(io::Error),
    Malformed(serde_json::Error),
}

impl std::fmt::Display for ChildReadErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Malformed(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

enum ExitCause {
    /// `shutdown` requested and honoured (or stdin-EOF exit) — stop cleanly.
    CleanShutdown,
    /// Runtime exited or became unusable unexpectedly — crash accounting applies.
    Crashed(String),
    /// Unrecoverable without operator action (e.g. protocol version mismatch)
    /// — straight to Failed, no restart loop.
    Fatal(String),
    /// Dev-mode hot reload: PYLAIR_RESTART_TRIGGER file changed — respawn
    /// immediately, no crash accounting.
    DevRestart,
}

impl Supervisor {
    fn set_state(&self, to: State, detail: impl Into<String>) {
        let from = *self.state_tx.borrow();
        if from != to {
            self.health.lock().unwrap().note_transition();
            let _ = self.state_tx.send(to);
            self.events.emit(RuntimeEvent::StateChanged {
                from,
                to,
                detail: detail.into(),
            });
        }
    }

    fn set_ready_url(&self, url: Option<String>) {
        self.ready_url_tx.send_if_modified(|cur| {
            let changed = *cur != url;
            *cur = url;
            changed
        });
    }

    async fn run(mut self) {
        self.run_loop().await;
        let outcome = self.shutdown_outcome.unwrap_or(ShutdownOutcome::Unknown);
        for ack in self.shutdown_acks.drain(..) {
            let _ = ack.send(outcome);
        }
    }

    /// Wait out a restart backoff, but honour Shutdown immediately and fail
    /// other commands fast (the runtime is down; callers must not hang).
    /// Returns true if shutdown was requested.
    async fn sleep_or_shutdown(&mut self, d: Duration) -> bool {
        let deadline = tokio::time::sleep(d);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return false,
                cmd = self.cmd_rx.recv() => match cmd {
                    None => return true,
                    Some(cmd) => {
                        // Every dequeued Call/CallStream releases its
                        // queue slot before any handling (decisions 0005).
                        release_queued(&self.fair, &cmd);
                        match cmd {
                            Command::Shutdown { ack } => {
                                self.shutdown_acks.push(ack);
                                return true;
                            }
                            c => fail_command_fast(c, "runtime is restarting"),
                        }
                    }
                },
            }
        }
    }

    async fn run_loop(&mut self) {
        let mut crashes: VecDeque<Instant> = VecDeque::new();
        let mut backoff = self.config.backoff_base;
        loop {
            self.ready_since = None;
            self.health.lock().unwrap().note_ready(None);
            self.set_state(
                State::Starting,
                format!("spawning (gen {})", self.generation),
            );
            // Per-generation blob dir (decision 0010): the spec's env is
            // host-authoritative — strip any PYLAIR_BLOB_DIR the spec
            // carried (the capability bit is the host's to give), then
            // export this generation's own dir when the root resolved.
            let mut spec = self.spec.clone();
            spec.env.retain(|(k, _)| k != crate::blobs::BLOB_DIR_ENV);
            // Release builds must never carry tracebacks: the constructors
            // only SET PYLAIR_DEV_TRACEBACKS under cfg(debug_assertions),
            // but the child also inherits this process's environment — a
            // release app launched from a dev shell would otherwise leak
            // source paths into the webview. Strip both channels. Isolated
            // and warm workers inherit the runtime's env, so a clean
            // runtime env is the whole defense in depth.
            if !cfg!(debug_assertions) {
                strip_dev_tracebacks(&mut spec);
            }
            let blob_dir =
                BlobRegistry::generation_dir(self.config.blob_root.as_deref(), self.generation);
            match &blob_dir {
                Some((dir, _)) => {
                    spec.env.push((
                        crate::blobs::BLOB_DIR_ENV.into(),
                        dir.to_string_lossy().into_owned(),
                    ));
                }
                None => {
                    if self.config.blob_root.is_some() {
                        self.events.emit(RuntimeEvent::Stderr(format!(
                            "pylair: could not create blob dir for gen {} — PYLAIR_BLOB_DIR unset",
                            self.generation
                        )));
                    }
                }
            }
            #[cfg(target_os = "linux")]
            let spawned = spawn_child(
                &spec,
                &self.config,
                &self.events,
                &self.inspect,
                self.generation,
                &mut self.pdeath_anchor,
            )
            .await;
            #[cfg(not(target_os = "linux"))]
            let spawned = spawn_child(
                &spec,
                &self.config,
                &self.events,
                &self.inspect,
                self.generation,
            )
            .await;
            let cause = match spawned {
                Ok(io) => {
                    // Inspector lifecycle (docs/inspector.md): the spawn
                    // opens the generation's timeline — exit/restart
                    // entries below close it. Inert in release.
                    self.inspect
                        .note_event(crate::inspect::TimelineEvent::Spawn {
                            generation: self.generation,
                            pid: io.pid,
                        });
                    self.run_child(io, blob_dir).await
                }
                Err(e) => {
                    // The dir was created for a generation that never ran —
                    // don't leave it for the sweep to find later.
                    if let Some((dir, _)) = &blob_dir {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    ExitCause::Crashed(format!("spawn failed: {e}"))
                }
            };
            // The generation is over, so its url is stale: a restart is a
            // new process on a new port (protocol-v1 section 4.1).
            self.set_ready_url(None);

            match cause {
                // No `last_exit` for a clean stop: the schema's kinds are
                // restart accounting (crashed | fatal | dev_restart), and
                // an operator stop is terminal, not an exit cause.
                ExitCause::CleanShutdown => {
                    self.set_state(State::Stopped, "shutdown");
                    return;
                }
                ExitCause::DevRestart => {
                    self.health
                        .lock()
                        .unwrap()
                        .note_exit(ExitKind::DevRestart, "source changed".into());
                    self.set_state(State::Restarting, "source changed");
                    self.generation += 1;
                    self.health.lock().unwrap().note_generation(self.generation);
                }
                ExitCause::Fatal(reason) => {
                    self.health
                        .lock()
                        .unwrap()
                        .note_exit(ExitKind::Fatal, reason.clone());
                    self.inspect
                        .note_event(crate::inspect::TimelineEvent::Exit {
                            generation: self.generation,
                            detail: reason.clone(),
                        });
                    self.set_state(State::Failed, reason);
                    if !self.park_failed().await {
                        return;
                    }
                    // operator retried: fresh counters, spawn again
                    crashes.clear();
                    backoff = self.config.backoff_base;
                    self.health
                        .lock()
                        .unwrap()
                        .note_restart_accounting(0, backoff);
                    self.generation += 1;
                    self.health.lock().unwrap().note_generation(self.generation);
                }
                ExitCause::Crashed(reason) => {
                    self.health
                        .lock()
                        .unwrap()
                        .note_exit(ExitKind::Crashed, reason.clone());
                    self.inspect
                        .note_event(crate::inspect::TimelineEvent::Exit {
                            generation: self.generation,
                            detail: reason.clone(),
                        });
                    // Surviving Ready for a whole breaker window counts as
                    // healthy — the crash that follows is a fresh incident,
                    // not evidence of a boot loop.
                    if self
                        .ready_since
                        .is_some_and(|t| t.elapsed() >= self.config.breaker_window)
                    {
                        crashes.clear();
                        backoff = self.config.backoff_base;
                        self.health
                            .lock()
                            .unwrap()
                            .note_restart_accounting(0, backoff);
                    }
                    let now = Instant::now();
                    while crashes
                        .front()
                        .is_some_and(|t| now.duration_since(*t) > self.config.breaker_window)
                    {
                        crashes.pop_front();
                    }
                    crashes.push_back(now);
                    self.health
                        .lock()
                        .unwrap()
                        .note_restart_accounting(crashes.len(), backoff);
                    if crashes.len() >= self.config.breaker_crashes {
                        self.inspect
                            .note_event(crate::inspect::TimelineEvent::Breaker {
                                generation: self.generation,
                                detail: format!(
                                    "crash loop: {} crashes in {:?}",
                                    crashes.len(),
                                    self.config.breaker_window
                                ),
                            });
                        self.set_state(
                            State::Failed,
                            format!(
                                "crash loop: {reason} ({} crashes in {:?})",
                                crashes.len(),
                                self.config.breaker_window
                            ),
                        );
                        if !self.park_failed().await {
                            return;
                        }
                        crashes.clear();
                        backoff = self.config.backoff_base;
                        self.health
                            .lock()
                            .unwrap()
                            .note_restart_accounting(0, backoff);
                        self.generation += 1;
                        self.health.lock().unwrap().note_generation(self.generation);
                        continue;
                    }
                    self.inspect
                        .note_event(crate::inspect::TimelineEvent::Restart {
                            generation: self.generation,
                            detail: reason.clone(),
                        });
                    self.set_state(State::Restarting, reason);
                    if self.sleep_or_shutdown(jitter(backoff)).await {
                        self.set_state(State::Stopped, "shutdown");
                        return;
                    }
                    backoff = (backoff * 2).min(self.config.backoff_cap);
                    self.health
                        .lock()
                        .unwrap()
                        .note_restart_accounting(crashes.len(), backoff);
                    self.generation += 1;
                    self.health.lock().unwrap().note_generation(self.generation);
                }
            }
        }
    }

    /// Failed is a parked state, not an exit — wait for Retry or Shutdown.
    /// Incoming calls are failed fast so callers never hang.
    async fn park_failed(&mut self) -> bool {
        loop {
            match self.cmd_rx.recv().await {
                None => {
                    self.set_state(State::Stopped, "shutdown");
                    return false;
                }
                Some(cmd) => {
                    release_queued(&self.fair, &cmd);
                    match cmd {
                        Command::Shutdown { ack } => {
                            self.shutdown_acks.push(ack);
                            self.set_state(State::Stopped, "shutdown");
                            return false;
                        }
                        Command::Retry => return true,
                        c => fail_command_fast(c, "runtime is stopped"),
                    }
                }
            }
        }
    }

    /// One runtime lifecycle: handshake, then serve until exit/kill.
    /// `blob_dir` is this generation's `PYLAIR_BLOB_DIR` (raw path and its
    /// canonical form for registration containment) — the Session owns
    /// its lifecycle and deletes it at teardown.
    async fn run_child(&mut self, io: ChildIo, blob_dir: Option<(PathBuf, PathBuf)>) -> ExitCause {
        // Phases. Each phase starts at a marker comment; the code lives in:
        //
        // 1. Spawn and hello (`handshake.rs`, "---- handshake: hello within
        //    deadline ----"). The child was already spawned by spawn_child
        //    from run_loop. Wait for `hello` within `hello_timeout`, check
        //    the protocol version, then go Ready.
        // 2. Steady state (`session.rs` and `session_commands.rs`, here the
        //    "---- serving ----" marker). Task, credit and liveness
        //    bookkeeping is `Session`; this function owns the one
        //    `tokio::select!` loop with five arms: "--- incoming frame ---"
        //    (`Session::on_frame`), "--- app command ---"
        //    (`Session::on_command`, which dispatches to `call`,
        //    `call_stream`, `cancel`, `grant_credit`, `cancel_owned` and
        //    `shutdown`), "--- liveness ping ---"
        //    (`Session::on_ping_tick`), "--- dev restart trigger ---"
        //    (`Session::on_trigger_tick`) and "--- child exit ---"
        //    (`Session::on_child_exit`).
        // 3. Shutdown and teardown. A requested shutdown is
        //    `Session::shutdown` (`Command::Shutdown { ack }`): shutdown
        //    frame on the control lane after committed data, wait out the
        //    grace budget, kill and reap if needed, deliver late replies,
        //    fail the rest. An unexpected exit is `Session::on_child_exit`.
        //    Every other exit (hello failure, wedged loop, missed pongs,
        //    protocol violation, dev restart, all handles dropped) kills
        //    and reaps where it is detected and returns its ExitCause.
        let io = match self.handshake(io).await {
            ControlFlow::Continue(io) => io,
            ControlFlow::Break(cause) => return cause,
        };

        // ---- serving ----
        let mut session = Session::new(self, io, blob_dir);
        let mut ping_tick = interval(session.sup.config.ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Dev-mode hot reload: the CLI touches the trigger file when source
        // changes; we poll its mtime (no fs-watch dep). Config wins over env.
        let restart_trigger = session
            .sup
            .config
            .restart_trigger
            .clone()
            .or_else(|| std::env::var_os("PYLAIR_RESTART_TRIGGER").map(PathBuf::from));
        let trigger_mtime = restart_trigger.as_deref().and_then(mtime);
        let mut trigger_tick = interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                // --- incoming frame ---
                msg = session.io.msg_in.recv() => {
                    if let ControlFlow::Break(cause) = session.on_frame(msg).await {
                        return cause;
                    }
                }
                // --- app command ---
                cmd = session.sup.cmd_rx.recv() => {
                    if let ControlFlow::Break(cause) = session.on_command(cmd).await {
                        return cause;
                    }
                }
                // --- liveness ping ---
                _ = ping_tick.tick() => {
                    if let ControlFlow::Break(cause) = session.on_ping_tick().await {
                        return cause;
                    }
                }
                // --- dev restart trigger ---
                _ = trigger_tick.tick(), if restart_trigger.is_some() => {
                    if let ControlFlow::Break(cause) = session.on_trigger_tick(&restart_trigger, trigger_mtime).await {
                        return cause;
                    }
                }
                // --- child exit ---
                status = &mut session.io.exited => {
                    return session.on_child_exit(status).await;
                }
            }
        }
    }
}

/// A streaming call refused before dispatch: the typed error reaches both
/// the task-id waiter and the stream's terminal (RC.8A).
fn reject_stream(items: StreamSink, task_out: oneshot::Sender<Result<u64, ErrorObj>>, e: ErrorObj) {
    let _ = task_out.send(Err(e.clone()));
    items.finish(Err(e));
}

/// Answer a command immediately when there is no runtime to run it.
fn fail_command_fast(c: Command, why: &str) {
    let err = || ErrorObj {
        code: proto::RUNTIME_UNAVAILABLE,
        message: why.into(),
        data: None,
    };
    match c {
        Command::Call { respond, .. } => {
            let _ = respond.send(Err(err()));
        }
        Command::CallStream { items, .. } => items.finish(Err(err())),
        Command::Cancel { respond, .. } => {
            let _ = respond.send(CancelOutcome::NotFound);
        }
        Command::GrantCredit { respond, .. } => {
            let _ = respond.send(Err(err()));
        }
        Command::CancelOwned { .. } | Command::Retry => {}
        Command::Shutdown { ack } => {
            let _ = ack.send(ShutdownOutcome::Unknown);
        }
    }
}

/// Deliver every response the runtime managed to write before it exited.
/// `wait()` can report the exit while bytes still sit unread in the pipe —
/// without this drain, `fail_all` would overwrite the runtime's actual
/// last words (TASK_CANCELLED, real results) with a generic code.
/// Bounded: at most 250ms of parsing whatever the pipe still holds.
async fn drain_exited_inbox(
    io: &mut ChildIo,
    pending: &mut HashMap<u64, PendingKind>,
    tasks: &mut HashMap<u64, TaskMeta>,
    task_of_call: &mut HashMap<u64, u64>,
    done_tasks: &mut HashSet<u64>,
    fair: &FairnessRef,
) {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let msg = match timeout(left, io.msg_in.recv()).await {
            Ok(Some(m)) => m,
            _ => return, // channel closed (reader hit EOF) or nothing left
        };
        let Ok(InMsg::Response { id, result, error }) = msg else {
            continue; // notifications from a dead runtime are moot
        };
        let out = match (result, error) {
            (_, Some(e)) => Err(e),
            (r, None) => Ok(r.unwrap_or(Value::Null)),
        };
        if let Some(kind) = pending.remove(&id) {
            match kind {
                PendingKind::Call(tx) => {
                    let _ = tx.send(out);
                }
                PendingKind::Stream(sink) => sink.finish(out),
                PendingKind::Cancel { respond } => {
                    let _ = respond.send(cancel_outcome(&out));
                }
            }
        }
        if let Some(task) = task_of_call.remove(&id) {
            if let Some(meta) = tasks.remove(&task) {
                fair.lock()
                    .unwrap()
                    .note_finished(&meta.owner, meta.is_stream, meta.is_isolated);
            }
            if done_tasks.len() >= 1024 {
                done_tasks.clear();
            }
            done_tasks.insert(task);
        }
    }
}

async fn fail_all(pending: &mut HashMap<u64, PendingKind>, code: i64, msg: &str) {
    for (_, kind) in pending.drain() {
        let err = ErrorObj {
            code,
            message: msg.into(),
            data: None,
        };
        match kind {
            PendingKind::Call(tx) => {
                let _ = tx.send(Err(err));
            }
            PendingKind::Stream(sink) => sink.finish(Err(err)),
            PendingKind::Cancel { respond } => {
                let _ = respond.send(CancelOutcome::TerminationUncertain);
            }
        }
    }
}

/// Signal the kill, then wait for the monitor to reap — the next spawn must
/// not overlap a still-alive previous generation. Returns whether the exit
/// was confirmed inside the wait window.
async fn kill_and_reap(io: &mut ChildIo) -> bool {
    let _ = io.kill.try_send(());
    timeout(Duration::from_secs(5), &mut io.exited)
        .await
        .is_ok()
}
