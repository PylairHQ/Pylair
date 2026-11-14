//! The supervisor: owns the runtime process lifecycle (docs/protocol-v1.md
//! §5-6, docs/cancellation.md).
//!
//! Design: a single actor task owns all mutable state (pending calls, streams,
//! liveness). IO is pushed through channels by small spawned tasks — reader,
//! writer, stderr drainer, process monitor — so blocking pipe IO never touches
//! the event loop. Kill semantics are per-platform: unix puts the child in its
//! own process group and killpg's it (catches grandchildren); Windows needs a
//! Job Object (process groups don't exist there).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, timeout};

use crate::protocol::{self as proto, ErrorObj, InMsg};
use crate::transport::Transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Unresponsive,
    Restarting,
    Failed,
    /// Supervisor received `Shutdown` and the runtime exited cleanly.
    /// (Host-side terminal state; not a runtime-reported state.)
    Stopped,
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    StateChanged {
        from: State,
        to: State,
        detail: String,
    },
    /// One line of runtime stderr (always drained — protocol §9).
    Stderr(String),
    /// Parsed `hello` exposes manifest.
    Exposes(Value),
}

#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    /// Path to the Python interpreter (system python in dev, bundled in prod).
    pub program: PathBuf,
    /// e.g. `["-m", "pylair", "my_app"]`
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Variables stripped from the INHERITED environment before `env` is
    /// applied (e.g. a launcher's PYTHONHOME must not reach a bundled Python).
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl SpawnSpec {
    pub fn pylair(python: PathBuf, app_module: &str) -> Self {
        Self {
            program: python,
            args: vec!["-m".into(), "pylair".into(), app_module.into()],
            env: vec![],
            env_remove: vec![],
            cwd: None,
        }
    }
}

/// Enforceable ceilings on input, output and concurrency (RC.8A). Every
/// overload returns `LIMIT_EXCEEDED` naming the limit in `data.limit` —
/// nothing is allowed to grow without bound. Limits are per RUNTIME, not
/// per window: one busy webview can exhaust a shared cap and starve the
/// others (no fairness — Known limitations).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Limits {
    /// In-flight tasks (plain calls, streams and isolated workers — all
    /// are tasks). Default `PYLAIR_MAX_TASKS` or 64.
    pub max_concurrent_tasks: usize,
    /// In-flight streaming tasks (a subset of `max_concurrent_tasks`).
    /// Default `PYLAIR_MAX_STREAMS` or 32.
    pub max_concurrent_streams: usize,
    /// Bytes buffered for ONE stream's consumer (serialized-size estimate).
    /// The separate `stream_buffer` chunk-count cap still applies —
    /// whichever trips first ends the stream. Default
    /// `PYLAIR_STREAM_BUFFER_BYTES` or 16 MiB.
    pub stream_buffer_bytes: usize,
    /// Total buffered stream bytes across all streams. Default
    /// `PYLAIR_STREAM_BUFFER_BYTES_TOTAL` or 64 MiB.
    pub stream_buffer_bytes_total: usize,
    /// Outstanding pull-credit per stream task (initial grant + later
    /// grants - chunks delivered). Default `PYLAIR_MAX_STREAM_CREDIT` or
    /// 4096.
    pub max_stream_credit: u64,
    /// Commands queued to the supervisor. Default
    /// `PYLAIR_MAX_QUEUED_COMMANDS` or 64.
    pub max_queued_commands: usize,
    /// Isolated worker processes in flight at once (enforced Python-side).
    /// Default `PYLAIR_MAX_ISOLATED_WORKERS` or 8.
    pub max_isolated_workers: usize,
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `PYLAIR_HELLO_TIMEOUT_MS` default and supported window. A cold
/// interpreter boot is ~0.05–1.3s and the worst observed CI boot
/// exceeded 10s under starvation; 30s is ~3x that worst case. Below 1s
/// even a trivial hello can lose to a cold machine; above 10min a silent
/// runtime is a hang, not a slow start.
const HELLO_TIMEOUT_DEFAULT_MS: u64 = 30_000;
const HELLO_TIMEOUT_MIN_MS: u64 = 1_000;
const HELLO_TIMEOUT_MAX_MS: u64 = 600_000;

/// Parse the knob's raw value: a whole-millis count. Garbage falls back
/// to the default with a warning — a malformed env var must not kill the
/// supervisor's configuration.
fn parse_hello_timeout_ms(v: Option<String>) -> (u64, Option<String>) {
    match v {
        None => (HELLO_TIMEOUT_DEFAULT_MS, None),
        Some(s) => match s.trim().parse::<u64>() {
            Ok(ms) => (ms, None),
            Err(_) => (
                HELLO_TIMEOUT_DEFAULT_MS,
                Some(format!(
                    "PYLAIR_HELLO_TIMEOUT_MS={s:?} is not a whole number of \
                     milliseconds — ignored"
                )),
            ),
        },
    }
}

/// Clamp a millis value into the supported window; the warning names
/// both the bad value and the range it was clamped into.
fn clamp_hello_timeout_ms(ms: u64) -> (u64, Option<String>) {
    if (HELLO_TIMEOUT_MIN_MS..=HELLO_TIMEOUT_MAX_MS).contains(&ms) {
        (ms, None)
    } else {
        (
            ms.clamp(HELLO_TIMEOUT_MIN_MS, HELLO_TIMEOUT_MAX_MS),
            Some(format!(
                "hello_timeout={ms}ms outside {HELLO_TIMEOUT_MIN_MS}..={HELLO_TIMEOUT_MAX_MS} — clamped"
            )),
        )
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: env_usize("PYLAIR_MAX_TASKS", 64),
            max_concurrent_streams: env_usize("PYLAIR_MAX_STREAMS", 32),
            stream_buffer_bytes: env_usize("PYLAIR_STREAM_BUFFER_BYTES", 16 * 1024 * 1024),
            stream_buffer_bytes_total: env_usize(
                "PYLAIR_STREAM_BUFFER_BYTES_TOTAL",
                64 * 1024 * 1024,
            ),
            max_stream_credit: env_u64("PYLAIR_MAX_STREAM_CREDIT", 4096),
            max_queued_commands: env_usize("PYLAIR_MAX_QUEUED_COMMANDS", 64),
            max_isolated_workers: env_usize("PYLAIR_MAX_ISOLATED_WORKERS", 8),
        }
    }
}

impl Limits {
    /// Clamp zero and absurd values into supported ranges; returns the
    /// warnings for anything clamped. Called once at supervisor start —
    /// the effective limits are logged with them.
    fn validated(mut self) -> (Self, Vec<String>) {
        let mut warns = Vec::new();
        let mut clamp = |v: &mut usize, name: &str, lo: usize, hi: usize| {
            if *v < lo || *v > hi {
                warns.push(format!("{name}={v} outside {lo}..={hi} — clamped"));
                *v = (*v).clamp(lo, hi);
            }
        };
        clamp(
            &mut self.max_concurrent_tasks,
            "max_concurrent_tasks",
            1,
            100_000,
        );
        clamp(
            &mut self.max_concurrent_streams,
            "max_concurrent_streams",
            1,
            100_000,
        );
        clamp(
            &mut self.stream_buffer_bytes,
            "stream_buffer_bytes",
            1024,
            1 << 30,
        );
        clamp(
            &mut self.stream_buffer_bytes_total,
            "stream_buffer_bytes_total",
            1024,
            8 << 30,
        );
        clamp(
            &mut self.max_queued_commands,
            "max_queued_commands",
            1,
            10_000,
        );
        clamp(
            &mut self.max_isolated_workers,
            "max_isolated_workers",
            0,
            1024,
        );
        if self.max_stream_credit < 1 || self.max_stream_credit > 1_000_000 {
            warns.push(format!(
                "max_stream_credit={} outside 1..=1000000 — clamped",
                self.max_stream_credit
            ));
            self.max_stream_credit = self.max_stream_credit.clamp(1, 1_000_000);
        }
        // A per-stream cap above the total can never hold a full buffer.
        if self.stream_buffer_bytes_total < self.stream_buffer_bytes {
            warns.push(format!(
                "stream_buffer_bytes_total={} below per-stream cap {} — raised",
                self.stream_buffer_bytes_total, self.stream_buffer_bytes
            ));
            self.stream_buffer_bytes_total = self.stream_buffer_bytes;
        }
        (self, warns)
    }

    pub fn with_max_concurrent_tasks(mut self, v: usize) -> Self {
        self.max_concurrent_tasks = v;
        self
    }
    pub fn with_max_concurrent_streams(mut self, v: usize) -> Self {
        self.max_concurrent_streams = v;
        self
    }
    pub fn with_stream_buffer_bytes(mut self, v: usize) -> Self {
        self.stream_buffer_bytes = v;
        self
    }
    pub fn with_stream_buffer_bytes_total(mut self, v: usize) -> Self {
        self.stream_buffer_bytes_total = v;
        self
    }
    pub fn with_max_stream_credit(mut self, v: u64) -> Self {
        self.max_stream_credit = v;
        self
    }
    pub fn with_max_queued_commands(mut self, v: usize) -> Self {
        self.max_queued_commands = v;
        self
    }
    pub fn with_max_isolated_workers(mut self, v: usize) -> Self {
        self.max_isolated_workers = v;
        self
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SupervisorConfig {
    /// Deadline for the runtime's `hello` after spawn.
    pub hello_timeout: Duration,
    /// How often we ping while Ready.
    pub ping_interval: Duration,
    /// Pongs outstanding before we declare the runtime wedged and kill it.
    /// A C extension holding the GIL can starve pongs — see cancellation.md.
    pub missed_pongs_to_kill: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Crash-loop breaker: this many crashes within `breaker_window` -> Failed.
    pub breaker_crashes: usize,
    pub breaker_window: Duration,
    /// Grace after `shutdown` request before we kill.
    pub shutdown_grace: Duration,
    /// Stream pull-credit granted to a new streaming call up front.
    pub initial_stream_credit: u64,
    /// Chunks buffered per stream between the supervisor and its consumer.
    /// A consumer that falls this far behind is cut off (and its producer
    /// cancelled) rather than allowed to stall the supervisor.
    pub stream_buffer: usize,
    /// Pongs come from a reader thread and only prove the process and pipe
    /// are alive. The runtime also reports how long its asyncio loop has made
    /// no progress (`loop_stale_ms`). `None` (the default) reports stalls as
    /// a diagnostic only — legitimate blocking work (a multi-GB model load
    /// inside `async def`) can stall the loop for minutes. `Some(limit)`
    /// treats a stall at or beyond it as wedged: in-flight calls fail with
    /// `RUNTIME_WEDGED` and the runtime is killed and restarted.
    pub loop_stall_kill: Option<Duration>,
    /// How long `cancel()` waits for the runtime's acknowledgement.
    pub cancel_timeout: Duration,
    pub max_frame: usize,
    /// Dev-mode hot reload: when this file's mtime changes, respawn the
    /// runtime (no crash accounting). Falls back to the
    /// PYLAIR_RESTART_TRIGGER env var when unset.
    pub restart_trigger: Option<PathBuf>,
    /// Concurrency/queue/buffer ceilings (RC.8A). Validated and logged at
    /// supervisor start.
    pub limits: Limits,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            hello_timeout: Duration::from_millis(
                parse_hello_timeout_ms(std::env::var("PYLAIR_HELLO_TIMEOUT_MS").ok()).0,
            ),
            ping_interval: Duration::from_secs(5),
            missed_pongs_to_kill: 3,
            backoff_base: Duration::from_millis(250),
            backoff_cap: Duration::from_secs(8),
            breaker_crashes: 5,
            breaker_window: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            initial_stream_credit: 64,
            stream_buffer: 256,
            loop_stall_kill: None,
            cancel_timeout: Duration::from_secs(5),
            max_frame: proto::max_frame(),
            restart_trigger: None,
            limits: Limits::default(),
        }
    }
}

/// A streamed item delivered to the caller of `call_stream`.
#[derive(Debug)]
pub enum StreamItem {
    Chunk {
        seq: u64,
        value: Value,
    },
    /// Terminal message for the stream: Ok(result) or Err(error).
    End(Result<Value, ErrorObj>),
}

/// Byte-budget lease for one buffered stream chunk (RC.8A): lives in the
/// channel item, so the counters it meters are released exactly when the
/// chunk leaves the buffer — recv'd by the consumer or dropped with the
/// channel. A consumer holding received items is draining, not stalling,
/// so its memory is not counted here.
struct StreamLease {
    per_stream: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    bytes: u64,
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.per_stream.fetch_sub(self.bytes, Ordering::Relaxed);
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Channel payload: the public `StreamItem::Chunk` plus its byte lease.
struct WeightedChunk {
    seq: u64,
    value: Value,
    _lease: StreamLease,
}

/// Supervisor-side end of a stream. Chunks are bounded by count AND
/// bytes (`try_send`); the terminal result travels on its own one-shot,
/// so it can never be lost to a full chunk buffer and never awaits.
struct StreamSink {
    chunks: mpsc::Sender<WeightedChunk>,
    end: Option<oneshot::Sender<Result<Value, ErrorObj>>>,
    per_stream: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    per_cap: u64,
    total_cap: u64,
}

/// Why a chunk could not be delivered.
#[derive(Debug)]
enum ChunkFail {
    /// Chunk-count buffer full: the consumer is not draining.
    Stalled,
    /// Per-stream byte cap exceeded (RC.8A).
    StreamBytes,
    /// All-streams byte cap exceeded (RC.8A).
    TotalBytes,
    /// Receiver dropped: the consumer is gone.
    Gone,
}

impl StreamSink {
    fn chunk(&self, seq: u64, value: Value) -> Result<(), ChunkFail> {
        // Serialized-size estimate — Value::to_string is already a
        // compact JSON encoding, close to the on-wire frame weight.
        let bytes = value.to_string().len() as u64;
        let per = self.per_stream.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if per > self.per_cap {
            self.per_stream.fetch_sub(bytes, Ordering::Relaxed);
            return Err(ChunkFail::StreamBytes);
        }
        let total = self.total.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if total > self.total_cap {
            self.total.fetch_sub(bytes, Ordering::Relaxed);
            self.per_stream.fetch_sub(bytes, Ordering::Relaxed);
            return Err(ChunkFail::TotalBytes);
        }
        self.chunks
            .try_send(WeightedChunk {
                seq,
                value,
                _lease: StreamLease {
                    per_stream: self.per_stream.clone(),
                    total: self.total.clone(),
                    bytes,
                },
            })
            .map_err(|e| match e {
                // On failure the rejected WeightedChunk (with its lease)
                // is handed back and dropped — counters release.
                mpsc::error::TrySendError::Full(_) => ChunkFail::Stalled,
                mpsc::error::TrySendError::Closed(_) => ChunkFail::Gone,
            })
    }

    /// Deliver the terminal. Never blocks; dropping the sink afterwards closes
    /// the chunk channel, which is how the receiver learns to read the end.
    fn finish(mut self, result: Result<Value, ErrorObj>) {
        if let Some(end) = self.end.take() {
            let _ = end.send(result);
        }
    }
}

/// Consumer-side end of a stream: queued chunks first, then exactly one
/// `StreamItem::End`, then `None`.
pub struct StreamRx {
    chunks: mpsc::Receiver<WeightedChunk>,
    end: Option<oneshot::Receiver<Result<Value, ErrorObj>>>,
}

impl StreamRx {
    pub async fn recv(&mut self) -> Option<StreamItem> {
        if self.end.is_some() {
            if let Some(c) = self.chunks.recv().await {
                return Some(StreamItem::Chunk {
                    seq: c.seq,
                    value: c.value,
                });
            }
            // Chunk channel closed => the sink is gone and the terminal is set
            // (or the supervisor itself dropped).
            let end = self.end.take()?;
            return Some(StreamItem::End(
                end.await.unwrap_or_else(|_| Err(stopped_err())),
            ));
        }
        None
    }
}

fn stream_pair(
    buffer: usize,
    per_stream_cap: u64,
    total: Arc<AtomicU64>,
    total_cap: u64,
) -> (StreamSink, StreamRx) {
    let (ctx, crx) = mpsc::channel(buffer.max(1));
    let (etx, erx) = oneshot::channel();
    (
        StreamSink {
            chunks: ctx,
            end: Some(etx),
            per_stream: Arc::new(AtomicU64::new(0)),
            total,
            per_cap: per_stream_cap,
            total_cap,
        },
        StreamRx {
            chunks: crx,
            end: Some(erx),
        },
    )
}

enum Command {
    Call {
        fn_name: String,
        input: Value,
        /// Client intent to declare on the wire (`"stream": <bool>`).
        /// `None` sends no field — the legacy shape a pre-flag host used,
        /// which the runtime answers without mismatch validation.
        stream: Option<bool>,
        respond: oneshot::Sender<Result<Value, ErrorObj>>,
        task_out: oneshot::Sender<u64>,
        owner: Option<String>,
    },
    CallStream {
        fn_name: String,
        input: Value,
        /// Declared client intent: `Some(false)` is a plain `call()` —
        /// no stream slot, no initial credit, `"stream": false` on the
        /// wire. `Some(true)` is `stream()` — slot + credit, `"stream":
        /// true`. `None` is a pre-flag client: accounted as a stream
        /// (slot + credit) but the field is OMITTED from the request so
        /// the runtime applies no mismatch validation.
        stream: Option<bool>,
        items: StreamSink,
        /// Ok(task id) or the typed rejection — `call_stream` must surface
        /// the REAL error (e.g. LIMIT_EXCEEDED), not a bare disconnect.
        task_out: oneshot::Sender<Result<u64, ErrorObj>>,
        owner: Option<String>,
    },
    Cancel {
        task: u64,
        respond: oneshot::Sender<CancelOutcome>,
        caller: Option<String>,
    },
    GrantCredit {
        task: u64,
        n: u64,
        respond: oneshot::Sender<Result<(), ErrorObj>>,
        caller: Option<String>,
    },
    /// Cancel every task owned by this webview label — sent when the
    /// window is destroyed or its document reloads (supervisor-initiated,
    /// not a JS caller). `before` bounds the reap to tasks allocated
    /// before a watermark: on a page-load the new document's calls may
    /// register before this command lands, and they must survive.
    CancelOwned { owner: String, before: Option<u64> },
    /// Operator-initiated restart out of the Failed state.
    Retry,
    /// Stop the runtime. `ack` fires only after the process tree is reaped,
    /// carrying what cleanup actually achieved.
    Shutdown {
        ack: oneshot::Sender<ShutdownOutcome>,
    },
}

/// What happened to a cancel request — distinct outcomes so a caller never
/// has to guess whether its frame landed or its task stopped (RC.6B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// Runtime confirmed the task stopped.
    Cancelled,
    /// Delivered, but the work can't be interrupted (inline sync): the
    /// caller sees TASK_CANCELLED while the Python may run to completion.
    RequestedMayContinue,
    /// Isolated worker was terminated and its process reaped.
    Terminated,
    /// Termination was requested but couldn't be confirmed — the worker
    /// may still be running.
    TerminationUncertain,
    /// The task already finished before the cancel landed.
    Completed,
    /// No such task in this runtime generation (stale or unknown id).
    NotFound,
    /// The caller does not own this task.
    Rejected,
    /// The control frame could not be queued — nothing was delivered.
    DeliveryFailed,
}

/// What shutdown actually achieved — reported, never assumed (RC.6C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownOutcome {
    /// Graceful exit inside the grace window.
    Terminated,
    /// Had to SIGKILL/taskkill — the tree is confirmed dead.
    ForcedKill,
    /// Kill sent but the exit wasn't confirmed in the wait window.
    Incomplete,
    /// The runtime was already gone or its state unknown.
    Unknown,
}

/// Stale-loop duration at which pongs surface an `Unresponsive` diagnostic.
/// Below the Python lag monitor's 1s threshold this would flap on jitter.
const LOOP_STALL_WARN: Duration = Duration::from_secs(1);

/// What a pong's `loop_stale_ms` means for the runtime. Pure so the
/// verdict boundaries are unit-testable without a clock or a process:
/// a configured kill wins over the warning, and with no kill configured
/// a stalled loop is only ever a diagnostic (P1-7: model loads can block
/// the loop for minutes on a healthy app).
#[derive(Debug, PartialEq, Eq)]
enum PongVerdict {
    Ready,
    StallWarn,
    StallKill,
}

fn classify_pong(stale: Duration, kill: Option<Duration>) -> PongVerdict {
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

/// An eagerly-dispatched call: the request is already on its way when this
/// returns (unlike `call`, an async fn which is lazy until polled). `task`
/// resolves to the runtime task id — needed for `cancel`.
pub struct CallTask {
    pub task: oneshot::Receiver<u64>,
    pub result: oneshot::Receiver<Result<Value, ErrorObj>>,
}

/// Handle the app holds; clones freely.
#[derive(Clone)]
pub struct Handle {
    cmd: mpsc::Sender<Command>,
    state: watch::Receiver<State>,
    stream_buffer: usize,
    cancel_timeout: Duration,
    /// Shared with the actor: total stream bytes buffered for ALL
    /// consumers, metered by each chunk's `StreamLease` (RC.8A).
    stream_bytes_total: Arc<AtomicU64>,
    /// Shared with the actor: the next task id it will allocate. A page
    /// load reads this as a watermark — tasks with id < watermark existed
    /// before the navigation committed and belong to the dead document.
    next_task: Arc<AtomicU64>,
    limits: Arc<Limits>,
}

impl Handle {
    fn queue_err(&self) -> ErrorObj {
        limit_err(
            "max_queued_commands",
            self.limits.max_queued_commands as u64,
            "command queue full — the supervisor is saturated",
        )
    }

    pub fn state(&self) -> State {
        *self.state.borrow()
    }

    /// Subscribe to state changes (UI lifecycle events).
    pub fn subscribe_state(&self) -> watch::Receiver<State> {
        self.state.clone()
    }

    /// Dispatch a call immediately (non-async, try_send) and get its task id.
    /// `owner` is the controlling caller's identity (a webview label from
    /// Tauri — never a JS-supplied value); `None` marks the task unowned
    /// (test/dev paths only — plugin commands always pass the label).
    /// `stream` is the wire `stream` field: `Some(false)` for `call()`
    /// (the only sane value here — `Some(true)` asks the runtime to
    /// stream into a call that cannot receive chunks), `None` omits the
    /// field entirely (no runtime mismatch validation).
    pub fn call_task(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
        stream: Option<bool>,
    ) -> Result<CallTask, ErrorObj> {
        let (rtx, rrx) = oneshot::channel();
        let (ttx, trx) = oneshot::channel();
        self.cmd
            .try_send(Command::Call {
                fn_name: fn_name.into(),
                input,
                stream,
                respond: rtx,
                task_out: ttx,
                owner,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => self.queue_err(),
                mpsc::error::TrySendError::Closed(_) => stopped_err(),
            })?;
        Ok(CallTask {
            task: trx,
            result: rrx,
        })
    }

    /// Call a function and await its result. For cancellation, prefer
    /// `call_task` — a pending `call` future carries no task id.
    pub async fn call(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
        stream: Option<bool>,
    ) -> Result<Value, ErrorObj> {
        let ct = self.call_task(fn_name, input, owner, stream)?;
        ct.result.await.map_err(|_| stopped_err())?
    }

    /// Start a call delivered over a per-call stream channel — chunks
    /// until `StreamItem::End`. `stream` is the declared client intent
    /// (`Some(false)` = plain `call()`, `Some(true)` = `stream()`,
    /// `None` = pre-flag client, see `Command::CallStream`).
    pub async fn call_stream(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
        stream: Option<bool>,
    ) -> Result<(u64, StreamRx), ErrorObj> {
        let (items_tx, items_rx) = stream_pair(
            self.stream_buffer,
            self.limits.stream_buffer_bytes as u64,
            self.stream_bytes_total.clone(),
            self.limits.stream_buffer_bytes_total as u64,
        );
        let (task_tx, task_rx) = oneshot::channel();
        self.cmd
            .try_send(Command::CallStream {
                fn_name: fn_name.into(),
                input,
                stream,
                items: items_tx,
                task_out: task_tx,
                owner,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => self.queue_err(),
                mpsc::error::TrySendError::Closed(_) => stopped_err(),
            })?;
        let task = task_rx.await.map_err(|_| stopped_err())??;
        Ok((task, items_rx))
    }

    /// Retry after a Failed state — respawns the runtime fresh.
    pub async fn retry(&self) -> Result<(), ErrorObj> {
        self.cmd.try_send(Command::Retry).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => self.queue_err(),
            mpsc::error::TrySendError::Closed(_) => stopped_err(),
        })
    }

    /// Ask the supervisor to cancel `task` as `caller`. Resolves to a
    /// `CancelOutcome` — never an ambiguous bool (RC.6B). A full command
    /// queue is a `LIMIT_EXCEEDED` error, not a wait.
    pub async fn cancel(
        &self,
        task: u64,
        caller: Option<String>,
    ) -> Result<CancelOutcome, ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .try_send(Command::Cancel {
                task,
                respond: tx,
                caller,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => self.queue_err(),
                mpsc::error::TrySendError::Closed(_) => stopped_err(),
            })?;
        match timeout(self.cancel_timeout, rx).await {
            Ok(r) => r.map_err(|_| stopped_err()),
            Err(_) => Err(ErrorObj {
                code: proto::RUNTIME_WEDGED,
                message: format!(
                    "cancel not acknowledged within {:?}: the runtime's event loop may be blocked",
                    self.cancel_timeout
                ),
                data: None,
            }),
        }
    }

    /// Cancel every task owned by `owner` — used when a window is destroyed.
    /// Async so the CALLER picks the executor — the same rule as `spawn`
    /// below. Window events run on the main thread where no reactor is
    /// entered; a `tokio::spawn` inside this method panicked there. The
    /// awaited send keeps the non-dropping guarantee: a momentarily-full
    /// command queue cannot silently lose it (the window's tasks would
    /// run on orphaned) — and nobody is left to report a loss to anyway.
    pub async fn cancel_owner(&self, owner: String) {
        let _ = self
            .cmd
            .send(Command::CancelOwned {
                owner,
                before: None,
            })
            .await;
    }

    /// The next task id the actor will allocate — a watermark for
    /// `cancel_owner_before`. Read cheap: a shared atomic.
    pub fn task_watermark(&self) -> u64 {
        self.next_task.load(Ordering::Relaxed)
    }

    /// Cancel every task `owner` allocated BEFORE `before` — used when a
    /// webview starts loading a new document. The label survives reloads,
    /// so unlike `cancel_owner` this must not sweep tasks the NEW
    /// document managed to register before this command lands: their ids
    /// are >= the watermark and they stay. Same awaited-send,
    /// never-dropped guarantee as `cancel_owner`.
    pub async fn cancel_owner_before(&self, owner: String, before: u64) {
        let _ = self
            .cmd
            .send(Command::CancelOwned {
                owner,
                before: Some(before),
            })
            .await;
    }

    /// Cancel exactly `task` as its owner — used by the stream forwarder
    /// when the call's channel dies. Unlike `cancel` this awaits the
    /// command send, so a momentarily-full queue cannot silently drop the
    /// reap the way a `try_send` could; the outcome is discarded — the
    /// dead consumer could never observe it. Cancelling one task leaves
    /// same-owner siblings alone (a window closed and reopened under the
    /// same label may hold newer live calls).
    pub async fn cancel_task(&self, task: u64, owner: String) {
        let (tx, _rx) = oneshot::channel();
        let _ = self
            .cmd
            .send(Command::Cancel {
                task,
                respond: tx,
                caller: Some(owner),
            })
            .await;
    }

    /// Grant more stream pull-credit to `task` as `caller`.
    pub async fn grant_credit(
        &self,
        task: u64,
        n: u64,
        caller: Option<String>,
    ) -> Result<(), ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .try_send(Command::GrantCredit {
                task,
                n,
                respond: tx,
                caller,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => self.queue_err(),
                mpsc::error::TrySendError::Closed(_) => stopped_err(),
            })?;
        rx.await.map_err(|_| stopped_err())?
    }

    /// Stop the runtime and wait until its process tree is gone (or the
    /// supervisor has already stopped). Returns what cleanup achieved.
    /// The awaited send is deliberate: a shutdown must never be dropped.
    pub async fn shutdown(&self) -> ShutdownOutcome {
        let (ack, done) = oneshot::channel();
        if self.cmd.send(Command::Shutdown { ack }).await.is_ok() {
            done.await.unwrap_or(ShutdownOutcome::Unknown)
        } else {
            ShutdownOutcome::Unknown
        }
    }
}

fn stopped_err() -> ErrorObj {
    ErrorObj {
        code: proto::RUNTIME_UNAVAILABLE,
        message: "runtime is stopped".into(),
        data: None,
    }
}

/// Overload always answers with an explicit, typed error naming the limit
/// (RC.8A) — never a silent wait, a drop, or a mislabeled "stopped".
fn limit_err(limit: &str, max: u64, msg: impl Into<String>) -> ErrorObj {
    ErrorObj {
        code: proto::LIMIT_EXCEEDED,
        message: msg.into(),
        data: Some(serde_json::json!({"limit": limit, "max": max})),
    }
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
struct ChildIo {
    /// Push raw frame bodies to write to child stdin.
    frame_out: mpsc::Sender<Vec<u8>>,
    /// Control frames (cancel / credit / shutdown) — a separate bounded
    /// lane the writer task always drains first, so a data flood cannot
    /// starve or silently drop control (RC.6B).
    ctrl_out: mpsc::Sender<Ctrl>,
    /// Received decoded incoming messages / transport errors.
    msg_in: mpsc::Receiver<Result<InMsg, ChildReadErr>>,
    /// Exit status of the child process.
    exited: oneshot::Receiver<io::Result<std::process::ExitStatus>>,
    /// Signal the monitor to kill the child process tree.
    kill: mpsc::Sender<()>,
    // stderr is drained by a detached task -> events.
}

enum ChildReadErr {
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
        let mut pongs_outstanding: u32 = 0;
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
                                    pongs_outstanding = 0;
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
                            let body = proto::request(id, "shutdown", Value::Null);
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
                    pongs_outstanding += 1;
                    ping_seq += 1;
                    let body = proto::request(self.next_id.fetch_add(1, Ordering::Relaxed),
                        "ping", serde_json::json!({"seq": ping_seq}));
                    let _ = send_frame(&io, &body, self.config.max_frame);
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

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Why an outgoing frame could not be queued (RC.8A): the data lane is
/// saturated, or the frame exceeds `max_frame` — which would otherwise
/// trip the PEER's read cap and kill the whole runtime.
enum SendFail {
    LaneFull,
    Oversize(usize),
}

fn send_frame(io: &ChildIo, body: &Value, max_frame: usize) -> Result<(), SendFail> {
    match serde_json::to_vec(body) {
        Ok(bytes) if bytes.len() > max_frame => Err(SendFail::Oversize(bytes.len())),
        Ok(bytes) => io.frame_out.try_send(bytes).map_err(|_| SendFail::LaneFull),
        Err(_) => Err(SendFail::LaneFull),
    }
}

/// Control lane: cancel / credit / shutdown frames the writer drains before
/// data frames. Bounded — a failed try_send is a visible failure, not a drop.
/// Control frames are always small, but the size check is kept anyway so
/// no path can ever write a frame the peer would reject.
fn send_ctrl(io: &ChildIo, body: &Value, max_frame: usize) -> bool {
    match serde_json::to_vec(body) {
        Ok(bytes) if bytes.len() <= max_frame => io.ctrl_out.try_send(Ctrl::Now(bytes)).is_ok(),
        _ => false,
    }
}

/// Control lane with data-lane flush ordering: the writer first writes
/// every data frame committed before this one. Used for `shutdown` — a
/// call the host already accepted must reach the runtime ahead of the
/// shutdown, or the task never registers and the caller hears a generic
/// 'unavailable' instead of the drain's real terminal (rc.15: the
/// ctrl-lane initial grant made this reordering deterministic on
/// Windows).
fn send_ctrl_flushed(io: &ChildIo, body: &Value, max_frame: usize) -> bool {
    match serde_json::to_vec(body) {
        Ok(bytes) if bytes.len() <= max_frame => {
            io.ctrl_out.try_send(Ctrl::FlushDataThen(bytes)).is_ok()
        }
        _ => false,
    }
}

/// One control-lane frame. Most control traffic overtakes queued data;
/// `FlushDataThen` additionally flushes committed data frames first.
enum Ctrl {
    Now(Vec<u8>),
    FlushDataThen(Vec<u8>),
}

/// May `caller` control a task recorded with `owner`? Unowned tasks
/// (supervisor-direct callers, tests) are open to anyone; owned tasks only
/// to the exact webview label Tauri reported for the invoking window.
fn owner_allows(owner: &Option<String>, caller: &Option<String>) -> bool {
    match owner {
        None => true,
        Some(o) => caller.as_deref() == Some(o.as_str()),
    }
}

/// Map the runtime's cancel ack — `{"cancelled": bool, "status"?, "reason"?}`
/// — to a distinct outcome. An error reply means the cancel itself failed.
fn cancel_outcome(out: &Result<Value, ErrorObj>) -> CancelOutcome {
    match out {
        Ok(v) if v["cancelled"].as_bool().unwrap_or(false) => match v["status"].as_str() {
            Some("terminated") => CancelOutcome::Terminated,
            Some("may_continue") => CancelOutcome::RequestedMayContinue,
            Some("termination_in_progress") => CancelOutcome::TerminationUncertain,
            _ => CancelOutcome::Cancelled,
        },
        // cancelled:false — the task already finished, or the pending
        // buffer refused it. A cancel that merely outraced registration
        // comes back cancelled:true + status:"pending" instead.
        Ok(_) => CancelOutcome::Completed,
        Err(_) => CancelOutcome::TerminationUncertain,
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

fn jitter(d: Duration) -> Duration {
    // +/-25% jitter, deterministic-enough without an RNG dep
    let nanos = d.as_nanos() as u64;
    let j = nanos / 4;
    let r = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0))
        % (2 * j + 1);
    Duration::from_nanos(nanos - j + r)
}

// ---- process spawning & kill plumbing ----

/// Linux: spawn the child FROM a dedicated long-lived thread that parks
/// for the host process's lifetime. PR_SET_PDEATHSIG is bound to the
/// *thread* that forked the child — not the process — so spawning on a
/// transient executor thread (e.g. a spawn_blocking worker at its ~10s
/// keepalive) would SIGKILL a perfectly healthy runtime. The anchor can
/// only die with the whole process, so the signal means exactly "the
/// host is gone".
///
/// One anchor serves every spawn a supervisor ever makes: the pre-rc.15
/// shape parked a fresh thread per spawn, so crash/retry respawns stacked
/// anchor threads (~8 MiB of reserved stack each) that never unwound.
/// The fork itself still happens on the anchor thread — the runtime
/// handle and the fully-configured `Command` are carried over a job
/// channel and executed there.
#[cfg(target_os = "linux")]
type AnchorJob = (
    tokio::runtime::Handle,
    tokio::process::Command,
    std::sync::mpsc::Sender<io::Result<Child>>,
);

#[cfg(target_os = "linux")]
struct PdeathAnchor(std::sync::mpsc::Sender<AnchorJob>);

/// Live anchor threads — test-only visibility that respawns reuse one
/// anchor instead of stacking them.
#[cfg(target_os = "linux")]
static ANCHOR_THREADS: AtomicU64 = AtomicU64::new(0);

/// Live `pylair-pdeath-anchor` threads in this process (test hook).
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn pdeath_anchor_threads() -> usize {
    ANCHOR_THREADS.load(Ordering::SeqCst) as usize
}

#[cfg(target_os = "linux")]
impl PdeathAnchor {
    fn new() -> io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<AnchorJob>();
        std::thread::Builder::new()
            .name("pylair-pdeath-anchor".into())
            .spawn(move || {
                ANCHOR_THREADS.fetch_add(1, Ordering::SeqCst);
                // Jobs until every sender is gone — a sender lives in the
                // supervisor actor, so the anchor outlives every spawn it
                // served and exits when the supervisor does.
                while let Ok((rt, mut cmd, reply)) = rx.recv() {
                    let res = {
                        let _guard = rt.enter();
                        cmd.spawn()
                    };
                    let _ = reply.send(res);
                }
                ANCHOR_THREADS.fetch_sub(1, Ordering::SeqCst);
            })
            .map(|_| Self(tx))
    }
}

#[cfg(target_os = "linux")]
fn spawn_via_pdeathsig_anchor(
    spec: &SpawnSpec,
    anchor: &mut Option<PdeathAnchor>,
) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|e| io::Error::other(format!("no runtime for spawn: {e}")))?;
    let mut cmd = TokioCommand::new(dunce::simplified(&spec.program));
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    cmd.envs(spec.env.iter().cloned());
    // The classic fork/prctl race: if the whole process dies between the
    // fork and the child's PR_SET_PDEATHSIG, the child is reparented before
    // the flag exists and the signal would never fire — a permanent orphan.
    // Mirror the isolated worker's check: verify getppid() still names the
    // process we forked from; if it changed, abort the exec and fail spawn.
    let expected_ppid = std::process::id() as libc::pid_t;
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != expected_ppid {
                return Err(io::Error::other("reparented before pdeathsig — aborting"));
            }
            Ok(())
        });
    }
    let anchor = match anchor {
        Some(a) => a,
        None => anchor.insert(PdeathAnchor::new()?),
    };
    let (tx, rx) = std::sync::mpsc::channel::<io::Result<Child>>();
    anchor
        .0
        .send((rt, cmd, tx))
        .map_err(|_| io::Error::other("pdeathsig anchor is gone"))?;
    rx.recv()
        .map_err(|_| io::Error::other("pdeathsig anchor died before spawn"))?
}

async fn spawn_child(
    spec: &SpawnSpec,
    config: &SupervisorConfig,
    events: &mpsc::Sender<RuntimeEvent>,
    #[cfg(target_os = "linux")] pdeath_anchor: &mut Option<PdeathAnchor>,
) -> io::Result<ChildIo> {
    // Simplify a canonicalized (Windows \\?\-prefixed) program path so the
    // child sees a plain sys.executable — dev-mode callers may pass one too.
    // On Linux the spawn happens on a dedicated anchor thread instead —
    // see spawn_via_pdeathsig_anchor.
    #[cfg(not(target_os = "linux"))]
    let mut child = {
        let mut cmd = TokioCommand::new(dunce::simplified(&spec.program));
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        cmd.envs(spec.env.iter().cloned());
        #[cfg(windows)]
        {
            // The bundled python.exe is console-subsystem: launched from our
            // GUI-subsystem app it would pop an empty console window unless
            // CREATE_NO_WINDOW suppresses it. Orthogonal to containment —
            // the Job Object attaches by handle after spawn, and stdio
            // stays on the pipes either way.
            cmd.creation_flags(child_creation_flags(
                std::env::var_os("PYLAIR_CONSOLE").as_deref(),
            ));
        }
        #[cfg(unix)]
        {
            // Own process group so killpg can reach grandchildren (§10).
            cmd.process_group(0);
        }
        cmd.spawn()?
    };
    #[cfg(target_os = "linux")]
    let mut child = spawn_via_pdeathsig_anchor(spec, pdeath_anchor)?;
    #[cfg(windows)]
    let pid = child.id();
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Job Object handle must outlive the child — held by the monitor task.
    #[cfg(windows)]
    let job = assign_to_job_object(pid);
    #[cfg(windows)]
    if job.is_none() {
        let _ = events.try_send(RuntimeEvent::Stderr(
            "pylair: could not place the runtime in a Job Object; falling back to \
             taskkill /T for cleanup (descendants may survive a hard host exit)"
                .into(),
        ));
    }
    #[cfg(windows)]
    let tree_pid = pid;

    // monitor: owns the child, awaits exit or kill signal
    let (exit_tx, exit_rx) = oneshot::channel();
    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
    #[cfg(windows)]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx, job, tree_pid));
    #[cfg(not(windows))]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx));

    // writer: frames -> child stdin. Two lanes: control (cancel/credit/
    // shutdown, small and always drained first) and data (calls, initial
    // credit). A stream flood can fill the data lane; it cannot starve or
    // silently drop a control frame (RC.6B).
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(64);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(16);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut w = stdin;
        loop {
            let body: Vec<u8> = tokio::select! {
                biased;
                // Ctrl always wins when both lanes have a frame; a closed
                // ctrl channel falls through to the data lane.
                c = ctrl_rx.recv() => match c {
                    Some(Ctrl::Now(b)) => b,
                    Some(Ctrl::FlushDataThen(b)) => {
                        // Shutdown ordering: write every data frame that
                        // was committed before this control frame — a call
                        // accepted before shutdown must reach the runtime
                        // or its caller hears 'unavailable' instead of the
                        // drain's real terminal.
                        while let Ok(d) = frame_rx.try_recv() {
                            if w.write_all(&(d.len() as u32).to_be_bytes())
                                .await
                                .is_err()
                                || w.write_all(&d).await.is_err()
                                || w.flush().await.is_err()
                            {
                                return; // child stdin gone
                            }
                        }
                        b
                    }
                    None => match frame_rx.recv().await {
                        Some(b) => b,
                        None => return,
                    },
                },
                d = frame_rx.recv() => match d {
                    Some(b) => b,
                    None => match ctrl_rx.recv().await {
                        Some(Ctrl::Now(b)) => b,
                        Some(Ctrl::FlushDataThen(b)) => b, // data lane already empty
                        None => return,
                    },
                },
            };
            if w.write_all(&(body.len() as u32).to_be_bytes())
                .await
                .is_err()
                || w.write_all(&body).await.is_err()
                || w.flush().await.is_err()
            {
                return; // child stdin gone
            }
        }
    });

    // reader: child stdout -> parsed messages
    let (msg_tx, msg_rx) = mpsc::channel::<Result<InMsg, ChildReadErr>>(64);
    let max_frame = config.max_frame;
    tokio::spawn(async move {
        let mut t = ReaderOnly {
            inner: stdout,
            max_frame,
        };
        loop {
            match t.read_frame().await {
                Ok(Some(body)) => {
                    let parsed = InMsg::parse(&body).map_err(ChildReadErr::Malformed);
                    if msg_tx.send(parsed).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return, // EOF
                Err(e) => {
                    let _ = msg_tx.send(Err(ChildReadErr::Io(e))).await;
                    return;
                }
            }
        }
    });

    // stderr: always drained, forwarded as events (protocol §9). Raw
    // bytes + lossy UTF-8: an invalid byte must never end the drain —
    // a full pipe deadlocks the child. The sink between drain and the
    // bounded event channel owns the documented drop-oldest policy.
    let ev = events.clone();
    let sink = Arc::new(StderrSink::new());
    {
        let sink = sink.clone();
        tokio::spawn(async move {
            let mut rdr = BufReader::with_capacity(16 * 1024, stderr);
            let mut lines = StderrLines::new();
            loop {
                match rdr.fill_buf().await {
                    Ok([]) => break, // EOF — child stderr closed
                    Ok(b) => {
                        let n = b.len();
                        for line in lines.feed(b) {
                            sink.push(line);
                        }
                        rdr.consume(n);
                    }
                    // A real I/O error (not bad UTF-8, which can't get
                    // here — we never decode through lines()).
                    Err(_) => break,
                }
            }
            if let Some(last) = lines.finish() {
                sink.push(last);
            }
            sink.finish();
        });
    }
    tokio::spawn(async move { sink.forward(ev).await });

    Ok(ChildIo {
        frame_out: frame_tx,
        ctrl_out: ctrl_tx,
        msg_in: msg_rx,
        exited: exit_rx,
        kill: kill_tx,
    })
}

/// stderr line + queue plumbing (protocol §9: drain unconditionally,
/// drop OLDEST with a "[dropped N lines]" marker under backpressure).
///
/// One delivered line caps at this many raw bytes; the rest of the line
/// is swallowed and the delivered text ends with the trunc marker.
const STDERR_LINE_CAP: usize = 8 * 1024;
/// Lines buffered between the always-draining reader and the bounded
/// event channel. Full → oldest entries are evicted and counted.
const STDERR_QUEUE_CAP: usize = 512;
const STDERR_TRUNC_MARK: &str = "…[truncated]";

/// Bounded, lossy stderr line assembler: splits raw bytes on `\n`,
/// converts each line with `from_utf8_lossy` (bad bytes become `U+FFFD`,
/// never an error), and caps a single line at `STDERR_LINE_CAP` input
/// bytes — an over-long line is delivered truncated with a visible
/// marker instead of growing the buffer without bound.
struct StderrLines {
    buf: Vec<u8>,
    /// Swallowing the tail of an over-cap line until its `\n`.
    over: bool,
}

impl StderrLines {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            over: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                out.push(self.take());
            } else if !self.over {
                if self.buf.len() < STDERR_LINE_CAP {
                    self.buf.push(b);
                } else {
                    // A byte is actually being dropped — only now is the
                    // line truncated. A line of exactly STDERR_LINE_CAP
                    // bytes is delivered unmarked.
                    self.over = true;
                }
            } // else: swallow — cap already reached for this line
        }
        out
    }

    /// Flush a partial line at EOF (no trailing newline).
    fn finish(&mut self) -> Option<String> {
        (!self.buf.is_empty() || self.over).then(|| self.take())
    }

    fn take(&mut self) -> String {
        let mut s = String::from_utf8_lossy(&self.buf).into_owned();
        if self.over {
            s.push_str(STDERR_TRUNC_MARK);
        }
        self.buf.clear();
        self.over = false;
        s
    }
}

enum SinkItem {
    Line(String),
    /// Drain ended — forward exits after the queue empties.
    Eof,
}

#[derive(Default)]
struct SinkState {
    q: VecDeque<SinkItem>,
    /// Lines evicted but not yet reported. The forwarder emits the
    /// cumulative "[dropped N stderr lines]" marker at delivery time —
    /// markers can't be evicted and the count can never be lost or
    /// double-counted.
    dropped: u64,
}

/// The drop-oldest bound the event channel can't express: a bounded
/// queue between the always-draining pipe reader (never blocks) and the
/// `mpsc` forwarder (blocks on backpressure). Under overflow the oldest
/// undelivered line is evicted and counted; the next pushed line is
/// preceded by a `[dropped N stderr lines]` marker — the documented
/// §9/§5 contract `try_send` (drop-newest, silent) could not provide.
struct StderrSink {
    state: std::sync::Mutex<SinkState>,
    notify: tokio::sync::Notify,
}

impl StderrSink {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SinkState::default()),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn push(&self, line: String) {
        {
            let mut st = self.state.lock().unwrap();
            while st.q.len() >= STDERR_QUEUE_CAP {
                match st.q.pop_front() {
                    Some(SinkItem::Line(_)) => st.dropped += 1,
                    // Eof only ever sits at the tail of a draining queue;
                    // if we ever see it at the front the invariant broke —
                    // keep it rather than swallow it.
                    Some(SinkItem::Eof) => {
                        st.q.push_back(SinkItem::Eof);
                        break;
                    }
                    None => break,
                }
            }
            st.q.push_back(SinkItem::Line(line));
        }
        self.notify.notify_one();
    }

    fn finish(&self) {
        self.state.lock().unwrap().q.push_back(SinkItem::Eof);
        self.notify.notify_one();
    }

    /// Forward queued lines onto the event channel; exits on Eof or a
    /// closed receiver. Blocks only on `send` — never on the reader.
    async fn forward(self: Arc<Self>, ev: mpsc::Sender<RuntimeEvent>) {
        loop {
            // Register interest before polling so a push between the
            // check and the wait can't be missed (Notify keeps a permit,
            // but this order is the canonical no-race form).
            let notified = self.notify.notified();
            let (item, dropped) = {
                let mut st = self.state.lock().unwrap();
                (st.q.pop_front(), std::mem::take(&mut st.dropped))
            };
            if dropped > 0 {
                let marker = format!("[dropped {dropped} stderr lines]");
                if ev.send(RuntimeEvent::Stderr(marker)).await.is_err() {
                    return;
                }
            }
            match item {
                Some(SinkItem::Line(line)) => {
                    if ev.send(RuntimeEvent::Stderr(line)).await.is_err() {
                        return;
                    }
                }
                Some(SinkItem::Eof) => return,
                None => notified.await,
            }
        }
    }
}

/// stdout reader without a writer — the reader task only consumes.
struct ReaderOnly<R> {
    inner: R,
    max_frame: usize,
}

#[async_trait::async_trait]
impl<R: tokio::io::AsyncRead + Unpin + Send> Transport for ReaderOnly<R> {
    async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        use tokio::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_be_bytes(len_buf) as usize;
        if n > self.max_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                crate::transport::FrameTooLarge(n),
            ));
        }
        let mut body = vec![0u8; n];
        self.inner.read_exact(&mut body).await?;
        Ok(Some(body))
    }
    async fn write_frame(&mut self, _body: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "reader only"))
    }
}

#[cfg(not(windows))]
async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
) {
    #[cfg(unix)]
    let pgid = child.id();
    tokio::select! {
        status = child.wait() => {
            // The leader exiting on its own must not leave group members
            // (workers, user subprocesses) running with no supervisor.
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                reap_group(pgid);
            }
            let _ = exit_tx.send(status);
        }
        _ = kill_rx.recv() => {
            kill_tree(&mut child).await;
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// SIGKILL every remaining member of the process group `pgid` we created.
/// ESRCH (already empty) and EPERM (group id recycled by an unrelated
/// process) are both "nothing of ours left to kill".
#[cfg(unix)]
fn reap_group(pgid: u32) {
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
    // Holding this keeps the Job Object alive; it closes when the monitor
    // task ends (after the child exits), which also kills any survivors.
    job: Option<JobHandle>,
    tree_pid: Option<u32>,
) {
    tokio::select! {
        status = child.wait() => { let _ = exit_tx.send(status); }
        _ = kill_rx.recv() => {
            if job.is_none() {
                // No Job Object: taskkill /T is the only way to reach the tree.
                if let Some(pid) = tree_pid {
                    // taskkill is console-subsystem too — same suppression.
                    let _ = tokio::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .creation_flags(child_creation_flags(
                            std::env::var_os("PYLAIR_CONSOLE").as_deref(),
                        ))
                        .output()
                        .await;
                }
            }
            let _ = child.start_kill(); // Job Object also reaps on handle close
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// Kill the child process tree: killpg on unix (own process group, protocol
/// §10); other targets fall back to plain child kill. On Windows the Job
/// Object reaps the tree when its handle closes (see monitor_child).
#[cfg(not(windows))]
async fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            // negative pid = process group we created via process_group(0)
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

/// Windows creation flags for console-subsystem children (the bundled
/// python.exe, taskkill). Without CREATE_NO_WINDOW a GUI-subsystem app
/// gets an empty console window per child. PYLAIR_CONSOLE (any value)
/// keeps the window for debugging; the bridge uses pipes, never the
/// console, so suppression is safe.
#[cfg(windows)]
fn child_creation_flags(console_env: Option<&std::ffi::OsStr>) -> u32 {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if console_env.is_some() {
        0
    } else {
        CREATE_NO_WINDOW
    }
}

/// windows-sys HANDLE is *mut c_void, which is !Send — wrap it so it can be
/// held by the monitor task (keeps the Job Object alive for the child's life).
#[cfg(windows)]
struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn assign_to_job_object(pid: Option<u32>) -> Option<JobHandle> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            CloseHandle(job);
            return None;
        }
        let Some(pid) = pid else {
            CloseHandle(job);
            return None;
        };
        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() || AssignProcessToJobObject(job, proc) == 0 {
            if !proc.is_null() {
                CloseHandle(proc);
            }
            CloseHandle(job);
            return None;
        }
        CloseHandle(proc);
        Some(JobHandle(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F5: PYLAIR_HELLO_TIMEOUT_MS parsing — real values pass through,
    /// garbage falls back to the default with a warning.
    #[test]
    fn hello_timeout_env_parses_or_defaults() {
        assert_eq!(
            parse_hello_timeout_ms(None),
            (30_000, None),
            "unset env must default to 30s"
        );
        assert_eq!(parse_hello_timeout_ms(Some("5000".into())).0, 5_000);
        assert_eq!(parse_hello_timeout_ms(Some(" 45000 ".into())).0, 45_000);
        // Garbage must not kill config: default + a warning to surface.
        for bad in ["abc", "", "-5", "1.5", "0x10"] {
            let (ms, warn) = parse_hello_timeout_ms(Some(bad.into()));
            assert_eq!(ms, 30_000, "{bad:?} must fall back to default");
            assert!(warn.is_some(), "{bad:?} must produce a warning");
        }
    }

    /// F5: the supported window is 1s..=600s, clamped with a warning.
    #[test]
    fn hello_timeout_clamps_into_supported_range() {
        assert_eq!(clamp_hello_timeout_ms(1_000).0, 1_000);
        assert_eq!(clamp_hello_timeout_ms(600_000).0, 600_000);
        assert_eq!(clamp_hello_timeout_ms(30_000).0, 30_000);
        let (lo, w1) = clamp_hello_timeout_ms(0);
        assert_eq!(lo, 1_000);
        assert!(w1.is_some());
        let (lo2, w2) = clamp_hello_timeout_ms(500);
        assert_eq!(lo2, 1_000);
        assert!(w2.is_some());
        let (hi, w3) = clamp_hello_timeout_ms(999_999);
        assert_eq!(hi, 600_000);
        assert!(w3.is_some());
        // In-range produces no warning.
        assert!(clamp_hello_timeout_ms(30_000).1.is_none());
    }

    #[test]
    fn stderr_lines_lossy_and_capped() {
        let mut l = StderrLines::new();
        // Invalid UTF-8 mid-stream: arrives as U+FFFD, does not stop parsing.
        let out = l.feed(b"\xff\xfe bad \x80\nsecond line\n");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains('\u{FFFD}'));
        assert_eq!(out[1], "second line");
        // A line over the cap is truncated with the marker, then parsing
        // resumes on the next line.
        let mut big = vec![b'x'; STDERR_LINE_CAP + 500];
        big.extend_from_slice(b"\nok\n");
        let out = l.feed(&big);
        assert_eq!(out.len(), 2);
        assert!(out[0].len() <= STDERR_LINE_CAP + STDERR_TRUNC_MARK.len());
        assert!(out[0].ends_with(STDERR_TRUNC_MARK));
        assert_eq!(out[1], "ok");
        // Exactly at the cap nothing was dropped: no marker. One byte
        // over and the marker returns.
        let mut exact = vec![b'e'; STDERR_LINE_CAP];
        exact.extend_from_slice(b"\n");
        let out = l.feed(&exact);
        assert_eq!(out.len(), 1);
        assert!(!out[0].ends_with(STDERR_TRUNC_MARK));
        assert_eq!(out[0].len(), STDERR_LINE_CAP);
        let mut one_over = vec![b'o'; STDERR_LINE_CAP + 1];
        one_over.extend_from_slice(b"\n");
        let out = l.feed(&one_over);
        assert!(out[0].ends_with(STDERR_TRUNC_MARK));
        // Partial line held until newline or finish.
        assert!(l.feed(b"no newline yet").is_empty());
        assert_eq!(l.finish().as_deref(), Some("no newline yet"));
    }

    #[tokio::test]
    async fn stderr_sink_drops_oldest_with_marker() {
        let sink = Arc::new(StderrSink::new());
        for i in 0..(STDERR_QUEUE_CAP + 10) {
            sink.push(format!("line-{i}"));
        }
        sink.finish();
        let (tx, mut rx) = mpsc::channel(STDERR_QUEUE_CAP * 2);
        let fwd = tokio::spawn({
            let s = sink.clone();
            async move { s.forward(tx).await }
        });
        let mut got = Vec::new();
        while let Some(RuntimeEvent::Stderr(l)) = rx.recv().await {
            got.push(l);
        }
        fwd.await.unwrap();
        // 522 pushes into a 512 queue: the last 10 pushes each evict one
        // head line — oldest first, newest survives, and ONE marker
        // reports the cumulative count at delivery time.
        assert_eq!(got.last().unwrap(), "line-521");
        assert_eq!(got.first().unwrap(), "[dropped 10 stderr lines]");
        assert!(!got.contains(&"line-0".to_string()));
        assert!(!got.contains(&"line-9".to_string()));
        assert!(got.contains(&"line-10".to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn console_flag_suppressed_unless_pylair_console() {
        use std::ffi::OsStr;
        // Default: CREATE_NO_WINDOW so the console-subsystem runtime stays
        // invisible under a GUI app.
        assert_eq!(child_creation_flags(None), 0x0800_0000);
        // PYLAIR_CONSOLE present (any value) → no flag, window allowed.
        for v in ["1", "0", ""] {
            assert_eq!(child_creation_flags(Some(OsStr::new(v))), 0);
        }
    }

    #[test]
    fn pong_classification_boundaries() {
        use PongVerdict::*;
        let kill = Some(Duration::from_secs(300));
        // Below the warn threshold: Ready even with a kill configured.
        assert_eq!(classify_pong(Duration::ZERO, kill), Ready);
        assert_eq!(
            classify_pong(LOOP_STALL_WARN - Duration::from_millis(1), kill),
            Ready
        );
        // Warn band: from threshold up to (but not including) kill.
        assert_eq!(classify_pong(LOOP_STALL_WARN, kill), StallWarn);
        assert_eq!(classify_pong(Duration::from_secs(299), kill), StallWarn);
        // At/over the configured kill: StallKill.
        assert_eq!(classify_pong(Duration::from_secs(300), kill), StallKill);
        // No kill configured: any stall is only a diagnostic, forever.
        assert_eq!(classify_pong(Duration::from_secs(3600), None), StallWarn);
        assert_eq!(classify_pong(Duration::MAX, None), StallWarn);
        // A kill configured below the warn threshold still wins (config is king).
        assert_eq!(
            classify_pong(Duration::from_millis(500), Some(Duration::from_millis(500))),
            StallKill
        );
    }

    #[test]
    fn stream_byte_caps_distinguish_per_stream_and_total() {
        use serde_json::json;
        // Deterministic counterpart to the e2e limits test: two streams
        // sharing one total budget — the chunk that crosses the SHARED cap
        // must be reported as TotalBytes even though its own per-stream
        // usage is under the per cap.
        let total = Arc::new(AtomicU64::new(0));
        let (s1, mut r1) = stream_pair(64, 1024, total.clone(), 1024);
        let (s2, _r2) = stream_pair(64, 1024, total.clone(), 1024);

        for i in 0..5u64 {
            assert!(s1.chunk(i, json!(0)).is_ok());
            assert!(s2.chunk(i, json!(0)).is_ok());
        }
        // Push the shared counter near the cap via a third view of the same
        // Arc — equivalent to bytes buffered by a consumer that isn't
        // draining.
        total.store(1024 - 8, Ordering::Relaxed);
        // s2's next chunk (18 bytes serialized) crosses the shared budget:
        // TotalBytes even though s2's own usage (~23 bytes) is under per.
        let err = json!("x".repeat(16));
        let res = s2.chunk(99, err.clone());
        assert!(
            matches!(res, Err(ChunkFail::TotalBytes)),
            "expected TotalBytes, got {res:?}"
        );
        // Draining s1's buffered chunks releases its leases, but the total
        // is still over — s1's next chunk trips the shared cap too.
        while r1.chunks.try_recv().is_ok() {}
        assert!(matches!(s1.chunk(99, err), Err(ChunkFail::TotalBytes)));
    }
}
