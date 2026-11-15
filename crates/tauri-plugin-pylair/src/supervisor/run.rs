//! The supervisor actor: `spawn()`, the `Supervisor` event loop and
//! restart policy (docs/protocol-v1.md section 7), `run_child` (one runtime
//! generation from hello to teardown; its phases are listed at its top)
//! and the helpers that fail or drain pending work.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, timeout};

use super::config::{
    clamp_hello_timeout_ms, parse_hello_timeout_ms, RuntimeEvent, SpawnSpec, State,
    SupervisorConfig,
};
use super::handle::{limit_err, CancelOutcome, Command, Handle, ShutdownOutcome};
#[cfg(target_os = "linux")]
use super::platform_unix::PdeathAnchor;
use super::spawn::spawn_child;
use super::stream::{ChunkFail, StreamSink};
use super::wire::{
    cancel_outcome, jitter, mtime, owner_allows, send_ctrl, send_ctrl_flushed, send_frame, Ctrl,
    SendFail,
};
use crate::protocol::{self as proto, ErrorObj, InMsg};

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
    owner: Option<String>,
    /// Streaming tasks count toward `max_concurrent_streams`.
    is_stream: bool,
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
    events: mpsc::Sender<RuntimeEvent>,
) -> (
    Handle,
    impl std::future::Future<Output = ()> + Send + 'static,
) {
    let (limits, warns) = config.limits.validated();
    for w in &warns {
        let _ = events.try_send(RuntimeEvent::Stderr(format!("pylair limits: {w}")));
    }
    // hello_timeout: surface a malformed knob (the value was already
    // resolved inside SupervisorConfig::default()) and clamp whatever
    // source the millis came from — env, Builder, or a hand-built config.
    if let (_, Some(w)) = parse_hello_timeout_ms(std::env::var("PYLAIR_HELLO_TIMEOUT_MS").ok()) {
        let _ = events.try_send(RuntimeEvent::Stderr(format!("pylair config: {w}")));
    }
    let (hello_ms, warn) = clamp_hello_timeout_ms(config.hello_timeout.as_millis() as u64);
    config.hello_timeout = Duration::from_millis(hello_ms);
    if let Some(w) = warn {
        let _ = events.try_send(RuntimeEvent::Stderr(format!("pylair config: {w}")));
    }
    // An initial grant above the cap would violate the cap immediately.
    if config.initial_stream_credit > limits.max_stream_credit {
        let _ = events.try_send(RuntimeEvent::Stderr(format!(
            "pylair limits: initial_stream_credit={} exceeds max_stream_credit={} — clamped",
            config.initial_stream_credit, limits.max_stream_credit
        )));
        config.initial_stream_credit = limits.max_stream_credit;
    }
    let _ = events.try_send(RuntimeEvent::Stderr(format!(
        "pylair limits: tasks={} streams={} queue={} stream_bytes={}/{} credit={} workers={} frame={}",
        limits.max_concurrent_tasks,
        limits.max_concurrent_streams,
        limits.max_queued_commands,
        limits.stream_buffer_bytes,
        limits.stream_buffer_bytes_total,
        limits.max_stream_credit,
        limits.max_isolated_workers,
        config.max_frame,
    )));
    config.limits = limits.clone();
    let limits = Arc::new(limits);
    let (cmd_tx, cmd_rx) = mpsc::channel(limits.max_queued_commands);
    let (state_tx, state_rx) = watch::channel(State::Starting);
    let stream_buffer = config.stream_buffer;
    let cancel_timeout = config.cancel_timeout;
    let stream_bytes_total = Arc::new(AtomicU64::new(0));
    let next_task = Arc::new(AtomicU64::new(1));
    let actor = Supervisor {
        spec,
        config,
        events,
        cmd_rx,
        state_tx,
        generation: 0,
        next_id: AtomicU64::new(1),
        next_task: next_task.clone(),
        ready_since: None,
        shutdown_acks: Vec::new(),
        shutdown_outcome: None,
        #[cfg(target_os = "linux")]
        pdeath_anchor: None,
    }
    .run();
    (
        Handle {
            cmd: cmd_tx,
            state: state_rx,
            stream_buffer,
            cancel_timeout,
            stream_bytes_total,
            next_task,
            limits,
        },
        actor,
    )
}

struct Supervisor {
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
    cmd_rx: mpsc::Receiver<Command>,
    state_tx: watch::Sender<State>,
    generation: u64,
    next_id: AtomicU64,
    /// Shared with `Handle` so a page-load can watermark "tasks that
    /// existed before this navigation" — see `cancel_owner_before`.
    next_task: Arc<AtomicU64>,
    /// When the current generation reached Ready — a crash after a sustained
    /// ready period is a fresh incident, not a boot loop.
    ready_since: Option<Instant>,
    /// Waiters for a completed shutdown, released when the actor stops.
    shutdown_acks: Vec<oneshot::Sender<ShutdownOutcome>>,
    /// What the most recent shutdown actually achieved (RC.6C).
    shutdown_outcome: Option<ShutdownOutcome>,
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
            let _ = self.state_tx.send(to);
            let _ = self.events.try_send(RuntimeEvent::StateChanged {
                from,
                to,
                detail: detail.into(),
            });
        }
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
                    Some(Command::Shutdown { ack }) => {
                        self.shutdown_acks.push(ack);
                        return true;
                    }
                    Some(c) => fail_command_fast(c, "runtime is restarting"),
                },
            }
        }
    }

    async fn run_loop(&mut self) {
        let mut crashes: VecDeque<Instant> = VecDeque::new();
        let mut backoff = self.config.backoff_base;
        loop {
            self.ready_since = None;
            self.set_state(
                State::Starting,
                format!("spawning (gen {})", self.generation),
            );
            #[cfg(target_os = "linux")]
            let spawned = spawn_child(
                &self.spec,
                &self.config,
                &self.events,
                &mut self.pdeath_anchor,
            )
            .await;
            #[cfg(not(target_os = "linux"))]
            let spawned = spawn_child(&self.spec, &self.config, &self.events).await;
            let cause = match spawned {
                Ok(io) => self.run_child(io).await,
                Err(e) => ExitCause::Crashed(format!("spawn failed: {e}")),
            };

            match cause {
                ExitCause::CleanShutdown => {
                    self.set_state(State::Stopped, "shutdown");
                    return;
                }
                ExitCause::DevRestart => {
                    self.set_state(State::Restarting, "source changed");
                    self.generation += 1;
                }
                ExitCause::Fatal(reason) => {
                    self.set_state(State::Failed, reason);
                    if !self.park_failed().await {
                        return;
                    }
                    // operator retried: fresh counters, spawn again
                    crashes.clear();
                    backoff = self.config.backoff_base;
                    self.generation += 1;
                }
                ExitCause::Crashed(reason) => {
                    // Surviving Ready for a whole breaker window counts as
                    // healthy — the crash that follows is a fresh incident,
                    // not evidence of a boot loop.
                    if self
                        .ready_since
                        .is_some_and(|t| t.elapsed() >= self.config.breaker_window)
                    {
                        crashes.clear();
                        backoff = self.config.backoff_base;
                    }
                    let now = Instant::now();
                    while crashes
                        .front()
                        .is_some_and(|t| now.duration_since(*t) > self.config.breaker_window)
                    {
                        crashes.pop_front();
                    }
                    crashes.push_back(now);
                    if crashes.len() >= self.config.breaker_crashes {
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
                        self.generation += 1;
                        continue;
                    }
                    self.set_state(State::Restarting, reason);
                    if self.sleep_or_shutdown(jitter(backoff)).await {
                        self.set_state(State::Stopped, "shutdown");
                        return;
                    }
                    backoff = (backoff * 2).min(self.config.backoff_cap);
                    self.generation += 1;
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
                Some(Command::Shutdown { ack }) => {
                    self.shutdown_acks.push(ack);
                    self.set_state(State::Stopped, "shutdown");
                    return false;
                }
                Some(Command::Retry) => return true,
                Some(c) => fail_command_fast(c, "runtime is stopped"),
            }
        }
    }

    /// One runtime lifecycle: handshake, then serve until exit/kill.
    async fn run_child(&mut self, mut io: ChildIo) -> ExitCause {
        // Phases. Line numbers are in this file; each phase also starts at
        // a marker comment, quoted here, if the numbers drift.
        //
        // 1. Spawn and hello (line 422, "---- handshake: hello within
        //    deadline ----"). The child was already spawned by spawn_child
        //    from run_loop. Wait for `hello` within `hello_timeout`, check
        //    the protocol version, then go Ready.
        // 2. Steady state (line 493, "---- serving ----"). Task, credit and
        //    liveness bookkeeping, then one `tokio::select!` loop with five
        //    arms: "--- incoming frame ---" (line 520), "--- app command ---"
        //    (line 665), "--- liveness ping ---" (line 948), "--- dev
        //    restart trigger ---" (line 984) and "--- child exit ---"
        //    (line 996).
        // 3. Shutdown and teardown. A requested shutdown is the
        //    `Command::Shutdown { ack }` arm (line 900): shutdown frame on
        //    the control lane after committed data, wait out the grace
        //    budget, kill and reap if needed, deliver late replies, fail
        //    the rest. An unexpected exit is the child exit arm (line 996).
        //    Every other exit (hello failure, wedged loop, missed pongs,
        //    protocol violation, dev restart, all handles dropped) kills
        //    and reaps where it is detected and returns its ExitCause.
        // ---- handshake: hello within deadline ----
        // At half the deadline, emit a repeated `starting → starting`
        // state event so UIs can show "still starting" — informational
        // only, never a state write (set_state dedupes same-state), and
        // at most once per spawn attempt. The timer dies with this
        // select: it cannot fire after hello, EOF, or a shutdown-driven
        // early return.
        let hello = {
            let mut deadline = Box::pin(tokio::time::sleep(self.config.hello_timeout));
            let half_mark = Box::pin(tokio::time::sleep(self.config.hello_timeout / 2));
            tokio::pin!(half_mark);
            let mut slow_noted = false;
            loop {
                tokio::select! {
                    m = io.msg_in.recv() => break Ok(m),
                    _ = &mut half_mark, if !slow_noted => {
                        slow_noted = true;
                        let _ = self.events.try_send(RuntimeEvent::StateChanged {
                            from: State::Starting,
                            to: State::Starting,
                            detail: format!(
                                "still starting ({:?} elapsed; timeout at {:?}, \
                                 see PYLAIR_HELLO_TIMEOUT_MS)",
                                self.config.hello_timeout / 2,
                                self.config.hello_timeout
                            ),
                        });
                    }
                    _ = &mut deadline => break Err(()),
                }
            }
        };
        let hello = match hello {
            Ok(Some(Ok(InMsg::Notification { method, params }))) if method == "hello" => {
                params.unwrap_or(Value::Null)
            }
            Ok(Some(Ok(_))) => {
                kill_and_reap(&mut io).await;
                return ExitCause::Crashed("first frame was not hello".into());
            }
            Ok(Some(Err(e))) => {
                kill_and_reap(&mut io).await;
                return ExitCause::Crashed(format!("protocol violation during hello: {e}"));
            }
            Ok(None) | Err(_) => {
                kill_and_reap(&mut io).await;
                return ExitCause::Crashed(format!(
                    "runtime produced no hello within {:?} \
                     (slow interpreter, first-run venv creation, or a wedged import). \
                     Raise via PYLAIR_HELLO_TIMEOUT_MS or Builder::hello_timeout; \
                     see protocol-v1 §4",
                    self.config.hello_timeout
                ));
            }
        };

        let version_ok = hello["protocol"].as_u64() == Some(proto::PROTOCOL_VERSION as u64);
        if !version_ok {
            kill_and_reap(&mut io).await;
            // Not a crash — a deployment bug. Retrying can't fix it.
            return ExitCause::Fatal(format!(
                "protocol version mismatch: {:?}",
                hello["protocol"]
            ));
        }
        let _ = self
            .events
            .try_send(RuntimeEvent::Exposes(hello["exposes"].clone()));
        self.ready_since = Some(Instant::now());
        self.set_state(State::Ready, "handshake complete");

        // ---- serving ----
        let mut pending: HashMap<u64, PendingKind> = HashMap::new();
        let mut task_of_call: HashMap<u64, u64> = HashMap::new(); // call_id -> task
                                                                  // Task registry {task -> call_id, owner webview} — the ownership
                                                                  // check for cancel/credit happens here, and the map is per-session
                                                                  // so a stale id from a previous generation resolves to NotFound.
        let mut tasks: HashMap<u64, TaskMeta> = HashMap::new();
        // Recently-finished task ids — lets cancel report Completed
        // instead of NotFound. Bounded; cleared on overflow.
        let mut done_tasks: HashSet<u64> = HashSet::new();
        let mut pongs_received: u64 = 0;
        let mut ping_seq: u64 = 0;
        let mut ping_tick = interval(self.config.ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Dev-mode hot reload: the CLI touches the trigger file when source
        // changes; we poll its mtime (no fs-watch dep). Config wins over env.
        let restart_trigger = self
            .config
            .restart_trigger
            .clone()
            .or_else(|| std::env::var_os("PYLAIR_RESTART_TRIGGER").map(PathBuf::from));
        let trigger_mtime = restart_trigger.as_deref().and_then(mtime);
        let mut trigger_tick = interval(Duration::from_millis(250));

        loop {
            tokio::select! {
                // --- incoming frame ---
                msg = io.msg_in.recv() => {
                    match msg {
                        Some(Ok(InMsg::Response { id, result, error })) => {
                            let out = match (result, error) {
                                (_, Some(e)) => Err(e),
                                (r, None) => Ok(r.unwrap_or(Value::Null)),
                            };
                            if let Some(kind) = pending.remove(&id) {
                                match kind {
                                    PendingKind::Call(tx) => { let _ = tx.send(out); }
                                    PendingKind::Stream(sink) => sink.finish(out),
                                    PendingKind::Cancel { respond } => {
                                        let _ = respond.send(cancel_outcome(&out));
                                    }
                                }
                            }
                            if let Some(task) = task_of_call.remove(&id) {
                                tasks.remove(&task);
                                if done_tasks.len() >= 1024 {
                                    done_tasks.clear();
                                }
                                done_tasks.insert(task);
                            }
                        }
                        Some(Ok(InMsg::Notification { method, params })) => {
                            let p = params.unwrap_or(Value::Null);
                            match method.as_str() {
                                "pong" => {
                                    pongs_received += 1;
                                    // Transport is alive. Is the loop SERVING?
                                    let stale = Duration::from_millis(p["loop_stale_ms"].as_u64().unwrap_or(0));
                                    match classify_pong(stale, self.config.loop_stall_kill) {
                                        PongVerdict::StallKill => {
                                            self.set_state(State::Unresponsive,
                                                format!("event loop blocked for {stale:?}"));
                                            fail_all(&mut pending, proto::RUNTIME_WEDGED,
                                                &format!("the runtime's event loop has been blocked for {stale:?} \
                                                          (blocking code inside an async def?)")).await;
                                            kill_and_reap(&mut io).await;
                                            return ExitCause::Crashed(format!("event loop blocked for {stale:?}"));
                                        }
                                        // Diagnostic only (P1-7): the loop
                                        // cannot serve calls right now, but a
                                        // long stall is not proof of a wedge.
                                        PongVerdict::StallWarn => self.set_state(State::Unresponsive,
                                            format!("event loop blocked for {stale:?} (diagnostic)")),
                                        PongVerdict::Ready => self.set_state(State::Ready, "pong received"),
                                    }
                                }
                                "stream.chunk" => {
                                    let task = p["task"].as_u64().unwrap_or(0);
                                    let seq = p["seq"].as_u64().unwrap_or(0);
                                    let value = p["value"].clone();
                                    // A chunk with no outstanding credit is
                                    // a protocol violation (protocol-v1 §3.1):
                                    // a runtime that cannot count credit
                                    // cannot be trusted to honour a per-task
                                    // abort either — same treatment as a
                                    // malformed frame. Chunks for a task the
                                    // generation no longer knows are still
                                    // ignored: they are a plausible race with
                                    // the producer draining after a finish.
                                    if tasks.get(&task).is_some_and(|m| m.credit == 0) {
                                        fail_all(&mut pending,
                                            proto::PROTOCOL_VIOLATION,
                                            "runtime sent stream.chunk with no outstanding credit").await;
                                        kill_and_reap(&mut io).await;
                                        return ExitCause::Crashed(
                                            format!("protocol violation: task {task} sent an uncredited stream.chunk"));
                                    }
                                    let mut abandoned = None;
                                    if let Some(meta) = tasks.get_mut(&task) {
                                        // Each delivered chunk consumed one
                                        // credit (RC.8A accounting).
                                        meta.credit -= 1;
                                        if let Some(PendingKind::Stream(sink)) = pending.get(&meta.call_id) {
                                            if let Err(why) = sink.chunk(seq, value) {
                                                abandoned = Some((meta.call_id, why));
                                            }
                                        }
                                    }
                                    if let Some((call_id, why)) = abandoned {
                                        // The consumer stalled or vanished, or a
                                        // byte cap tripped. End the stream with a
                                        // terminal that cannot be lost, and STOP
                                        // the producer instead of leaving it
                                        // running unobserved.
                                        let (err, note) = match why {
                                            ChunkFail::Stalled => (ErrorObj {
                                                code: proto::INTERNAL,
                                                message: "stream consumer stalled".into(),
                                                data: None,
                                            }, "stalled"),
                                            ChunkFail::Gone => (ErrorObj {
                                                code: proto::INTERNAL,
                                                message: "stream consumer went away".into(),
                                                data: None,
                                            }, "went away"),
                                            ChunkFail::StreamBytes => (limit_err(
                                                "stream_buffer_bytes",
                                                self.config.limits.stream_buffer_bytes as u64,
                                                "stream consumer buffered past the per-stream byte cap"),
                                                "over the byte cap"),
                                            ChunkFail::TotalBytes => (limit_err(
                                                "stream_buffer_bytes_total",
                                                self.config.limits.stream_buffer_bytes_total as u64,
                                                "all streams exceeded the shared byte cap"),
                                                "over the shared byte cap"),
                                        };
                                        let _ = self.events.try_send(RuntimeEvent::Stderr(
                                            format!("task {task}: stream consumer {note}; cancelling producer")));
                                        if let Some(PendingKind::Stream(sink)) = pending.remove(&call_id) {
                                            sink.finish(Err(err));
                                        }
                                        task_of_call.remove(&call_id);
                                        tasks.remove(&task);
                                        let cid = self.next_id.fetch_add(1, Ordering::Relaxed);
                                        let (ack, _) = oneshot::channel();
                                        if send_ctrl(&io, &proto::request(cid, "cancel",
                                            serde_json::json!({"task": task})),
                                            self.config.max_frame) {
                                            pending.insert(cid, PendingKind::Cancel { respond: ack });
                                        }
                                    }
                                }
                                _ => {} // unknown notifications are ignored (spec §8)
                            }
                        }
                        Some(Err(e)) => {
                            fail_all(&mut pending, proto::PROTOCOL_VIOLATION, "runtime sent a malformed frame").await;
                            kill_and_reap(&mut io).await;
                            return ExitCause::Crashed(format!("protocol violation: {e}"));
                        }
                        None => {
                            // reader hit EOF: runtime is gone; monitor should confirm
                            let detail = match timeout(Duration::from_secs(5), &mut io.exited).await {
                                Ok(Ok(Ok(status))) => format!("exited {status}"),
                                _ => "runtime gone".into(),
                            };
                            fail_all(&mut pending, proto::RUNTIME_CRASHED, &detail).await;
                            return ExitCause::Crashed(detail);
                        }
                    }
                }
                // --- app command ---
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else {
                        // all handles dropped — shut down
                        let outcome = if kill_and_reap(&mut io).await {
                            ShutdownOutcome::ForcedKill
                        } else {
                            ShutdownOutcome::Incomplete
                        };
                        self.shutdown_outcome = Some(outcome);
                        return ExitCause::CleanShutdown;
                    };
                    match cmd {
                        Command::Call { fn_name, input, stream, respond, task_out, owner } => {
                            let lim = &self.config.limits;
                            if tasks.len() >= lim.max_concurrent_tasks {
                                let _ = respond.send(Err(limit_err(
                                    "max_concurrent_tasks",
                                    lim.max_concurrent_tasks as u64,
                                    "too many in-flight tasks")));
                                continue;
                            }
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let task = self.next_task.fetch_add(1, Ordering::Relaxed);
                            let mut params = serde_json::json!({"fn": fn_name, "task": task,
                                "input": input});
                            if let Some(s) = stream {
                                params["stream"] = serde_json::json!(s);
                            }
                            let body = proto::request(id, "call", params);
                            match send_frame(&io, &body, self.config.max_frame) {
                                Ok(()) => {
                                    let _ = task_out.send(task);
                                    pending.insert(id, PendingKind::Call(respond));
                                    task_of_call.insert(id, task);
                                    tasks.insert(task, TaskMeta {
                                        call_id: id, owner, is_stream: false, credit: 0,
                                    });
                                }
                                Err(SendFail::Oversize(n)) => {
                                    let _ = respond.send(Err(limit_err(
                                        "max_frame",
                                        self.config.max_frame as u64,
                                        format!("call frame is {n} bytes — the runtime would reject it"))));
                                }
                                Err(SendFail::LaneFull) => {
                                    let _ = respond.send(Err(limit_err(
                                        "frame_out_queue", 64,
                                        "runtime stdin queue full — not draining")));
                                }
                            }
                        }
                        Command::CallStream { fn_name, input, stream, items, task_out, owner } => {
                            // `None` = a pre-flag client: it could be
                            // either API, so the conservative accounting
                            // is "stream" — a slot is taken and the
                            // initial credit is granted exactly as before,
                            // but the wire field stays absent so the
                            // runtime applies no mismatch validation.
                            let is_stream = stream.unwrap_or(true);
                            let lim = &self.config.limits;
                            if tasks.len() >= lim.max_concurrent_tasks {
                                reject_stream(items, task_out, limit_err(
                                    "max_concurrent_tasks",
                                    lim.max_concurrent_tasks as u64,
                                    "too many in-flight tasks"));
                                continue;
                            }
                            if is_stream {
                                let streams = tasks.values().filter(|m| m.is_stream).count();
                                if streams >= lim.max_concurrent_streams {
                                    reject_stream(items, task_out, limit_err(
                                        "max_concurrent_streams",
                                        lim.max_concurrent_streams as u64,
                                        "too many in-flight streams"));
                                    continue;
                                }
                            }
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let task = self.next_task.fetch_add(1, Ordering::Relaxed);
                            let mut params = serde_json::json!({"fn": fn_name, "task": task,
                                "input": input});
                            if let Some(s) = stream {
                                params["stream"] = serde_json::json!(s);
                            }
                            let body = proto::request(id, "call", params);
                            if is_stream {
                                let credit = proto::notification("stream.credit",
                                    serde_json::json!({"task": task, "n": self.config.initial_stream_credit}));
                                // The initial grant is control traffic and goes
                                // FIRST: the runtime buffers credit for a task it
                                // has not registered yet (pending credit), so the
                                // grant cannot be starved or reordered behind
                                // data-lane traffic — and a failed send fails the
                                // stream BEFORE the call is committed instead of
                                // leaving it uncredited forever.
                                if !send_ctrl(&io, &credit, self.config.max_frame) {
                                    reject_stream(items, task_out, limit_err(
                                        "ctrl_queue", 16,
                                        "control queue full — credit not delivered"));
                                    continue;
                                }
                            }
                            match send_frame(&io, &body, self.config.max_frame) {
                                Ok(()) => {
                                    let _ = task_out.send(Ok(task));
                                    pending.insert(id, PendingKind::Stream(items));
                                    task_of_call.insert(id, task);
                                    tasks.insert(task, TaskMeta {
                                        call_id: id, owner, is_stream,
                                        credit: if is_stream {
                                            self.config.initial_stream_credit
                                        } else {
                                            0
                                        },
                                    });
                                }
                                Err(e) => {
                                    // The grant was delivered but the call was
                                    // not — best-effort cancel drops the
                                    // stranded pending credit (bounded anyway).
                                    let cid = self.next_id.fetch_add(1, Ordering::Relaxed);
                                    let _ = send_ctrl(&io, &proto::request(cid, "cancel",
                                        serde_json::json!({"task": task})),
                                        self.config.max_frame);
                                    let max = self.config.max_frame as u64;
                                    reject_stream(items, task_out, match e {
                                        SendFail::Oversize(n) => limit_err("max_frame", max,
                                            format!("call frame is {n} bytes — the runtime would reject it")),
                                        SendFail::LaneFull => limit_err("frame_out_queue", 64,
                                            "runtime stdin queue full — not draining"),
                                    });
                                }
                            }
                        }
                        Command::Cancel { task, respond, caller } => {
                            match tasks.get(&task) {
                                // Unknown to this generation: a completed
                                // tombstone means the cancel raced a finish;
                                // anything else is a stale or foreign id.
                                None => {
                                    let _ = respond.send(if done_tasks.contains(&task) {
                                        CancelOutcome::Completed
                                    } else {
                                        CancelOutcome::NotFound
                                    });
                                }
                                Some(meta) if !owner_allows(&meta.owner, &caller) => {
                                    let _ = respond.send(CancelOutcome::Rejected);
                                }
                                Some(_) => {
                                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                                    let body = proto::request(id, "cancel",
                                        serde_json::json!({"task": task}));
                                    // Control lane: a data flood cannot starve
                                    // this, and a failed try_send is a loud
                                    // outcome — never a silent drop.
                                    if send_ctrl(&io, &body, self.config.max_frame) {
                                        pending.insert(id, PendingKind::Cancel { respond });
                                    } else {
                                        let _ = respond.send(CancelOutcome::DeliveryFailed);
                                    }
                                }
                            }
                        }
                        Command::GrantCredit { task, n, respond, caller } => {
                            match tasks.get_mut(&task) {
                                None => {
                                    let _ = respond.send(Err(ErrorObj {
                                        code: proto::TASK_NOT_FOUND,
                                        message: if done_tasks.contains(&task) {
                                            format!("task {task} already finished")
                                        } else {
                                            format!("no such task in this generation: {task}")
                                        },
                                        data: None,
                                    }));
                                }
                                Some(meta) if !owner_allows(&meta.owner, &caller) => {
                                    let _ = respond.send(Err(ErrorObj {
                                        code: proto::INVALID_REQUEST,
                                        message: format!("task {task} belongs to a different webview"),
                                        data: None,
                                    }));
                                }
                                Some(meta) => {
                                    let lim = &self.config.limits;
                                    let would = meta.credit.checked_add(n);
                                    match would {
                                        Some(total) if total <= lim.max_stream_credit => {
                                            let body = proto::notification("stream.credit",
                                                serde_json::json!({"task": task, "n": n}));
                                            let _ = respond.send(
                                                if send_ctrl(&io, &body, self.config.max_frame) {
                                                    meta.credit = total;
                                                    Ok(())
                                                } else {
                                                    Err(limit_err("ctrl_queue", 16,
                                                        "control queue full — credit not delivered"))
                                                });
                                        }
                                        _ => {
                                            let _ = respond.send(Err(limit_err(
                                                "max_stream_credit",
                                                lim.max_stream_credit,
                                                format!("grant of {n} exceeds the outstanding-credit cap"))));
                                        }
                                    }
                                }
                            }
                        }
                        Command::CancelOwned { owner, before } => {
                            // Window destroyed (before: None) or new
                            // document loading (Some watermark): cancel
                            // every task it owned / owned before the mark.
                            // Supervisor-initiated, so the owner check does
                            // not apply; delivery failures only lose a
                            // cancel the dead context could never observe.
                            for (&task, meta) in tasks.iter() {
                                if meta.owner.as_deref() == Some(owner.as_str())
                                    && before.is_none_or(|b| task < b)
                                {
                                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                                    let body = proto::request(id, "cancel",
                                        serde_json::json!({"task": task}));
                                    let (ack, _) = oneshot::channel();
                                    if send_ctrl(&io, &body, self.config.max_frame) {
                                        pending.insert(id, PendingKind::Cancel { respond: ack });
                                    }
                                }
                            }
                        }
                        Command::Retry => {
                            // Only meaningful from Failed — already serving.
                        }
                        Command::Shutdown { ack } => {
                            self.shutdown_acks.push(ack);
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            // The runtime's drain budget must finish inside
                            // our kill timer: give it the grace minus ~1s
                            // for the ack, exit and reap, floored so a tiny
                            // configured grace still buys real drain time.
                            // grace_ms is ONE budget the runtime shares
                            // across every shutdown phase (protocol §7).
                            let grace_ms = self
                                .config
                                .shutdown_grace
                                .saturating_sub(Duration::from_millis(1000))
                                .max(Duration::from_millis(500))
                                .as_millis() as u64;
                            let body = proto::request(
                                id,
                                "shutdown",
                                serde_json::json!({"grace_ms": grace_ms}),
                            );
                            // Flush ordering: calls committed to the data
                            // lane before this shutdown must be written
                            // first, so the drain can return their real
                            // terminal instead of a generic unavailable.
                            let _ = send_ctrl_flushed(&io, &body, self.config.max_frame);
                            // Graceful exit within the grace window consumes
                            // `exited`; on timeout it is still pollable, so
                            // kill the tree and wait for the reap. Report
                            // what actually happened (RC.6C).
                            let outcome = if timeout(self.config.shutdown_grace, &mut io.exited).await.is_ok() {
                                ShutdownOutcome::Terminated
                            } else if kill_and_reap(&mut io).await {
                                ShutdownOutcome::ForcedKill
                            } else {
                                ShutdownOutcome::Incomplete
                            };
                            self.shutdown_outcome = Some(outcome);
                            // The runtime may have written final responses
                            // (TASK_CANCELLED, results) before exiting —
                            // deliver them so callers get what actually
                            // happened, then sweep the rest.
                            drain_exited_inbox(&mut io, &mut pending, &mut tasks,
                                               &mut task_of_call, &mut done_tasks).await;
                            fail_all(&mut pending, proto::RUNTIME_UNAVAILABLE, "shutdown").await;
                            return ExitCause::CleanShutdown;
                        }
                    }
                }
                // --- liveness ping ---
                _ = ping_tick.tick() => {
                    // Misses count only pings the writer actually put on
                    // the wire: a ping still in the slot behind a full
                    // pipe is backpressure, not evidence the runtime died.
                    let pongs_outstanding = io.pings_sent
                        .load(Ordering::Relaxed)
                        .saturating_sub(pongs_received) as u32;
                    if pongs_outstanding >= self.config.missed_pongs_to_kill {
                        self.set_state(State::Unresponsive,
                            format!("{} missed pongs — runtime wedged", pongs_outstanding));
                        fail_all(&mut pending, proto::RUNTIME_WEDGED,
                            "runtime stopped responding (possible native-code wedge)").await;
                        kill_and_reap(&mut io).await;
                        return ExitCause::Crashed(format!(
                            "wedged: {} missed pongs", pongs_outstanding));
                    }
                    if pongs_outstanding > 0 {
                        // A ping went a full interval unanswered — a real miss.
                        self.set_state(State::Unresponsive,
                            format!("missed {} pong(s)", pongs_outstanding));
                    }
                    ping_seq += 1;
                    let body = proto::request(self.next_id.fetch_add(1, Ordering::Relaxed),
                        "ping", serde_json::json!({"seq": ping_seq}));
                    // Liveness gets its own slot the writer drains ahead
                    // of data, never the shared control lane - a stalled
                    // writer holds at most one pending ping, so liveness
                    // cannot starve cancel/credit either.
                    if let Ok(bytes) = serde_json::to_vec(&body) {
                        if bytes.len() <= self.config.max_frame {
                            *io.ping_slot.lock().unwrap() = Some(bytes);
                            io.ping_notify.notify_one();
                        }
                    }
                }
                // --- dev restart trigger ---
                _ = trigger_tick.tick(), if restart_trigger.is_some() => {
                    let m = restart_trigger.as_deref().and_then(mtime);
                    if m != trigger_mtime {
                        let _ = self.events.try_send(RuntimeEvent::Stderr(
                            "pylair: source changed — restarting python runtime".into()));
                        fail_all(&mut pending, proto::RUNTIME_RESTARTED,
                            "python restarted (source change)").await;
                        kill_and_reap(&mut io).await;
                        return ExitCause::DevRestart;
                    }
                }
                // --- child exit ---
                status = &mut io.exited => {
                    let detail = match status {
                        Ok(Ok(s)) => format!("exited {s}"),
                        Ok(Err(e)) => format!("wait error: {e}"),
                        Err(_) => "monitor dropped".into(),
                    };
                    // Same last-words delivery as the shutdown path: a dying
                    // runtime's queued responses resolve their own calls.
                    drain_exited_inbox(&mut io, &mut pending, &mut tasks,
                                       &mut task_of_call, &mut done_tasks).await;
                    fail_all(&mut pending, proto::RUNTIME_CRASHED, &detail).await;
                    return ExitCause::Crashed(detail);
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
            tasks.remove(&task);
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
