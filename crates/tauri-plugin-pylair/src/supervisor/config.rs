//! Runtime state and events, the spawn spec, limits and the supervisor
//! configuration (hello timeout: docs/protocol-v1.md section 4; restart
//! policy: section 7; limits: docs/security.md section 6).

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::protocol as proto;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

/// What the supervised child is (docs/protocol-v1.md section 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeKind {
    /// The bridge runtime: `python -m pylair <app>`. Ready on `hello`.
    #[default]
    Bridge,
    /// Wrap mode: `python -m pylair.wrap`, which supervises a web server.
    /// Ready on `ready {url}`, not on `hello`.
    Web,
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
    /// A liveness verdict (transport or loop) changed
    /// (docs/decisions/0006 §5): `{version, generation, liveness}` —
    /// no per-window data. One emit per flip, never per tick.
    HealthChanged(Value),
    /// An app-reported component's phase changed (docs/decisions/0007
    /// §5): `{version, generation, name, phase}` — one emit per
    /// transition, never per progress tick.
    AppChanged(Value),
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
/// nothing is allowed to grow without bound. Limits are per runtime;
/// `per_window_fairness` adds a guaranteed minimum per live window on
/// top (docs/decisions/0005-per-window-fairness.md).
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
    /// Guaranteed-minimum fairness across webview owners
    /// (docs/decisions/0005): every live window keeps a small reserved
    /// minimum of the shared caps and may borrow the rest. A
    /// single-window app behaves exactly as before. Default
    /// `PYLAIR_PER_WINDOW_FAIRNESS` or true; `0`/`false`/`off` disables.
    pub per_window_fairness: bool,
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

/// Bool env knob: unset -> `default`; `0`/`false`/`off` (any case) ->
/// false; anything else -> true.
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
    }
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
pub(super) fn parse_hello_timeout_ms(v: Option<String>) -> (u64, Option<String>) {
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
pub(super) fn clamp_hello_timeout_ms(ms: u64) -> (u64, Option<String>) {
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

/// `ready_timeout` default and supported window. A web server that loads a
/// model at import time can take minutes before it answers a request.
const READY_TIMEOUT_DEFAULT: Duration = Duration::from_secs(120);
const READY_TIMEOUT_MIN_MS: u64 = 1_000;
const READY_TIMEOUT_MAX_MS: u64 = 3_600_000;

/// Clamp a ready timeout into the supported window; the warning names both
/// the bad value and the range it was clamped into.
pub(super) fn clamp_ready_timeout_ms(ms: u64) -> (u64, Option<String>) {
    if (READY_TIMEOUT_MIN_MS..=READY_TIMEOUT_MAX_MS).contains(&ms) {
        (ms, None)
    } else {
        (
            ms.clamp(READY_TIMEOUT_MIN_MS, READY_TIMEOUT_MAX_MS),
            Some(format!(
                "ready_timeout={ms}ms outside {READY_TIMEOUT_MIN_MS}..={READY_TIMEOUT_MAX_MS} - clamped"
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
            per_window_fairness: env_bool("PYLAIR_PER_WINDOW_FAIRNESS", true),
        }
    }
}

impl Limits {
    /// Clamp zero and absurd values into supported ranges; returns the
    /// warnings for anything clamped. Called once at supervisor start —
    /// the effective limits are logged with them.
    pub(super) fn validated(mut self) -> (Self, Vec<String>) {
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
    pub fn with_per_window_fairness(mut self, v: bool) -> Self {
        self.per_window_fairness = v;
        self
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SupervisorConfig {
    /// What the child is. `Bridge` (default) is ready on `hello`; `Web` is
    /// ready on `ready {url}` (protocol-v1 section 4.1).
    pub kind: RuntimeKind,
    /// Deadline for the runtime's `hello` after spawn.
    pub hello_timeout: Duration,
    /// `Web` only: deadline for `ready {url}` after `hello`. Default 120s,
    /// clamped to 1s..=1h at spawn. Separate from `hello_timeout` because a
    /// server that loads a model at import time legitimately takes minutes.
    pub ready_timeout: Duration,
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
    /// a diagnostic only — and that default is deliberate: legitimate
    /// blocking work (a multi-GB model load inside `async def`) can stall
    /// the loop for minutes, so killing on a stall turns a slow-but-healthy
    /// app into a restart loop, and a restart mid-update would look like a
    /// bad update to the watchdog and get it rolled back. `Some(limit)`
    /// treats a stall at or beyond it as wedged: in-flight calls fail with
    /// `RUNTIME_WEDGED` and the runtime is killed and restarted — opt in
    /// only when a blocked loop really is unrecoverable for your app. For
    /// visibility without the kill, see `loop_stall_warn`.
    pub loop_stall_kill: Option<Duration>,
    /// Warn on a stalled loop without killing: when a pong reports
    /// `loop_stale_ms` at or past this, one `pylair` stderr line per stall
    /// episode names the likely cause and fix, and a matching "recovered"
    /// line lands when the loop frees. `Some(5s)` is the default; `None`
    /// silences the warning entirely. Independent of `loop_stall_kill` —
    /// a configured kill still wins (the stall is fatal, not a warning).
    /// Suppressed while `health().app` reports a component `loading` —
    /// `on_startup` blocking is legitimate — but a stall that began during
    /// load and continues once the app is ready warns then, and says so.
    pub loop_stall_warn: Option<Duration>,
    /// How long `cancel()` waits for the runtime's acknowledgement.
    pub cancel_timeout: Duration,
    pub max_frame: usize,
    /// Dev-mode hot reload: when this file's mtime changes, respawn the
    /// runtime (no crash accounting). Falls back to the
    /// PYLAIR_RESTART_TRIGGER env var when unset.
    pub restart_trigger: Option<PathBuf>,
    /// Root of the per-generation blob directories (decision 0010):
    /// `<blob_root>/gen-<n>` is created per spawn, exported to the child
    /// as `PYLAIR_BLOB_DIR` and deleted at generation end. `None` (the
    /// default, or when the cache dir could not be resolved) disables
    /// blobs — the runtime then falls back to `$b64`. The plugin builder
    /// fills this from `app.path().app_cache_dir()`; tests set it to a
    /// temp dir.
    pub blob_root: Option<PathBuf>,
    /// Blob ceilings (decision 0010 §6). `None` reads the
    /// `PYLAIR_MAX_BLOB_*` env knobs; `Some` overrides them — embedders
    /// and tests get a deterministic registry without env races.
    pub blob_limits: Option<crate::blobs::BlobLimits>,
    /// Concurrency/queue/buffer ceilings (RC.8A). Validated and logged at
    /// supervisor start.
    pub limits: Limits,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            kind: RuntimeKind::Bridge,
            ready_timeout: READY_TIMEOUT_DEFAULT,
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
            loop_stall_warn: Some(Duration::from_secs(5)),
            cancel_timeout: Duration::from_secs(5),
            max_frame: proto::max_frame(),
            restart_trigger: None,
            blob_root: None,
            blob_limits: None,
            limits: Limits::default(),
        }
    }
}
