//! The supervisor: owns the runtime process lifecycle (docs/protocol-v1.md
//! §5-6, docs/cancellation.md).
//!
//! Design: a single actor task owns all mutable state (pending calls, streams,
//! liveness). IO is pushed through channels by small spawned tasks — reader,
//! writer, stderr drainer, process monitor — so blocking pipe IO never touches
//! the event loop. Kill semantics are per-platform: unix puts the child in its
//! own process group and killpg's it (catches grandchildren); Windows needs a
//! Job Object (process groups don't exist there).
//!
//! # Where things live
//!
//! | File | Owns |
//! |---|---|
//! | `config.rs` | `State`, `RuntimeEvent`, `SpawnSpec`, `Limits`, `SupervisorConfig`, the `PYLAIR_*` env knobs, hello-timeout parse and clamp |
//! | `handle.rs` | `Handle`, the public API (`call`, `call_stream`, `cancel`, `grant_credit`, `shutdown`, ...), the `Command`s it sends, `CallTask`, `CancelOutcome`, `ShutdownOutcome` |
//! | `run.rs` | `spawn()`, the `Supervisor` actor (`run`, `run_loop`, restart backoff, `park_failed`), `run_child` (one runtime generation: the `select!` loop; its phases live in `run/`), `fail_all`, `drain_exited_inbox`, `kill_and_reap` |
//! | `run/handshake.rs` | `Supervisor::handshake`: spawn to Ready (hello within the deadline, protocol version check; for the `Web` kind also `ready {url}` within `ready_timeout`, url validation) |
//! | `run/session.rs` | `Session`, the per-generation bookkeeping, and the `select!` arm handlers `on_frame`, `on_command`, `on_ping_tick`, `on_trigger_tick`, `on_child_exit` |
//! | `run/session_commands.rs` | the `Command` handlers: `call`, `call_stream`, `cancel`, `grant_credit`, `cancel_owned`, `shutdown` |
//! | `spawn.rs` | `spawn_child`: the platform spawn path, the stdin writer (control lane first, single ping slot), the stdout reader, the stderr drain |
//! | `wire.rs` | data-lane and control-lane send helpers (`send_frame`, `send_ctrl`, `send_ctrl_flushed`, `Ctrl`), `owner_allows`, `cancel_outcome`, `jitter`, `mtime` |
//! | `stream.rs` | `StreamItem`, `StreamRx`, the byte-leased `StreamSink` and `stream_pair` |
//! | `stderr.rs` | bounded lossy stderr lines (`StderrLines`) and the drop-oldest `StderrSink` |
//! | `platform_unix.rs` | every non-Windows target: the Linux parent-death anchor thread, process-group kill and reap, the non-Windows `monitor_child` |
//! | `platform_windows.rs` | Windows: the Job Object, the suspended-spawn resume, console creation flags, the Windows `monitor_child` |
//! | `tests.rs` | unit tests for the internals above |
//!
//! # Where the behaviour is specified
//!
//! - Credit lane (pull-based stream flow control, the initial grant sent
//!   ahead of the call): docs/protocol-v1.md section 3.1. Code: `handle.rs`
//!   (`grant_credit`), `run/session.rs` and `run/session_commands.rs`
//!   (credit accounting in `Session::on_frame` and `Session::grant_credit`),
//!   `stream.rs`.
//! - Control lane (cancel, credit and shutdown frames overtake data; the
//!   single-slot ping): docs/protocol-v1.md section 5.1 ("Control frames
//!   have their own lane") and section 5 (liveness). Code: `wire.rs`, the
//!   writer task in `spawn.rs`.
//! - Cancel outcomes: docs/protocol-v1.md section 3.2 and
//!   docs/cancellation.md. Code: `handle.rs`, `wire.rs` (`cancel_outcome`).
//! - Hello timeout: docs/protocol-v1.md section 4, item 3. Code: `config.rs`,
//!   `run/handshake.rs`.
//! - Web runtime kind (wrap mode: ready on `ready {url}`, ready timeout,
//!   url validation, the current url as state): docs/protocol-v1.md
//!   section 4.1. Code: `config.rs` (`RuntimeKind`), `run/handshake.rs`,
//!   `handle.rs` (`ready_url`), `util.rs` (`validate_ready_url`).
//! - Shutdown (completes before it is acknowledged, one shared `grace_ms`
//!   budget, committed data flushed ahead of `shutdown`): docs/protocol-v1.md
//!   sections 6 and 7. Code: `Session::shutdown` (`run/session_commands.rs`),
//!   `wire.rs` (`send_ctrl_flushed`).
//! - Job Object and suspended spawn: docs/protocol-v1.md section 6,
//!   docs/security.md section 7, docs/design/windows-job-object-race.md.
//!   Code: `platform_windows.rs`, `spawn.rs`.
//! - Linux parent-death signal and process groups: docs/protocol-v1.md
//!   section 6. Code: `platform_unix.rs`, `spawn.rs`.
//! - Limits and `LIMIT_EXCEEDED`: docs/security.md section 6,
//!   docs/protocol-v1.md section 8. Code: `config.rs` (`Limits`),
//!   `handle.rs` (`limit_err`), `run.rs`.
//! - Stderr is always drained: docs/protocol-v1.md section 1. Code:
//!   `stderr.rs`, `spawn.rs`.

mod config;
mod handle;
#[cfg(not(windows))]
mod platform_unix;
#[cfg(windows)]
mod platform_windows;
mod run;
mod spawn;
mod stderr;
mod stream;
#[cfg(test)]
mod tests;
mod wire;

pub use config::{Limits, RuntimeEvent, RuntimeKind, SpawnSpec, State, SupervisorConfig};
pub use handle::{CallTask, CancelOutcome, Handle, ShutdownOutcome};
#[cfg(target_os = "linux")]
pub use platform_unix::pdeath_anchor_threads;
pub use run::spawn;
pub use stream::{StreamItem, StreamRx};
