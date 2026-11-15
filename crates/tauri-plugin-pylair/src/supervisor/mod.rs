//! The supervisor: owns the runtime process lifecycle (docs/protocol-v1.md
//! §5-6, docs/cancellation.md).
//!
//! Design: a single actor task owns all mutable state (pending calls, streams,
//! liveness). IO is pushed through channels by small spawned tasks — reader,
//! writer, stderr drainer, process monitor — so blocking pipe IO never touches
//! the event loop. Kill semantics are per-platform: unix puts the child in its
//! own process group and killpg's it (catches grandchildren); Windows needs a
//! Job Object (process groups don't exist there).

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

pub use config::{Limits, RuntimeEvent, SpawnSpec, State, SupervisorConfig};
pub use handle::{CallTask, CancelOutcome, Handle, ShutdownOutcome};
#[cfg(target_os = "linux")]
pub use platform_unix::pdeath_anchor_threads;
pub use run::spawn;
pub use stream::{StreamItem, StreamRx};
