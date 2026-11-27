//! Tauri plugin: supervisor, IPC bridge and permission enforcement for the
//! managed Python runtime.
//!
//! `protocol` / `transport` / `supervisor` are Tauri-free and testable in
//! isolation. The plugin glue owns the supervisor `Handle`, forwards
//! lifecycle events to the frontend, and exposes the `python_*` commands.

pub mod eventbus;
pub mod protocol;
pub mod supervisor;
pub mod transport;

#[doc(hidden)]
pub mod binaries;
#[doc(hidden)]
pub mod blobs;
mod builder;
mod commands;
mod fn_scope;
#[cfg(debug_assertions)]
pub mod inspect;
#[cfg(not(debug_assertions))]
#[path = "inspect_stub.rs"]
pub mod inspect;
mod registry;
mod selftest;
mod spec;
pub mod update;
mod util;
pub mod wrap;

pub use builder::{Builder, StderrDelivery};
#[cfg(debug_assertions)]
pub use commands::python_inspect_snapshot_impl;
pub use commands::{
    forward_stream, python_call_impl, python_call_stream_impl, python_health_impl,
    python_state_impl, StreamAck,
};
pub use protocol::ErrorObj;
pub use registry::{PyRuntime, Registry, WindowRegistry};
pub use selftest::{self_test_gui, self_test_headless, self_test_requested};

/// The app's hidden update-watchdog mode (decision 0011 §2): spawned
/// detached by `python_update_install` as a state-dir copy of the exe
/// with `--pylair-upwatch` on argv. Call at the top of the app's
/// `run()` with the embedded context — `Some(exit_code)` means watchdog
/// mode ran, exit with it; `None` means a normal boot, continue.
/// Without the `updater` feature (or the flag) it is a no-op.
#[cfg(feature = "updater")]
pub fn update_watchdog_entry<R: tauri::Runtime>(ctx: &tauri::Context<R>) -> Option<i32> {
    update::upwatch::entry(ctx)
}
/// See the `updater` variant — the feature-off stub is a plain `None`.
#[cfg(not(feature = "updater"))]
pub fn update_watchdog_entry<R: tauri::Runtime>(_ctx: &tauri::Context<R>) -> Option<i32> {
    None
}
#[doc(hidden)]
pub use spec::set_binaries_debug_env;
pub use spec::{app_spawn_spec, release_spawn_spec, SpawnSpecError};
pub use supervisor::{
    spawn as spawn_supervisor, strip_dev_runtime_status, strip_dev_tracebacks,
    strip_inherited_binaries, Handle, HealthSnapshot, RuntimeEvent, RuntimeKind, SpawnSpec, State,
    StreamItem, SupervisorConfig,
};
pub use util::same_origin;

/// Frontend event names (kept in one place — the client SDK mirrors them).
pub mod events {
    /// `{from, to, detail}` lifecycle transitions.
    pub const STATE: &str = "pylair://state";
    /// One line of runtime stderr.
    pub const STDERR: &str = "pylair://stderr";
    /// The exposes manifest after handshake.
    pub const EXPOSES: &str = "pylair://exposes";
    /// Liveness verdict changes only (docs/decisions/0006 §5):
    /// `{version, generation, liveness}`.
    pub const HEALTH: &str = "pylair://health";
    /// App component phase transitions only (docs/decisions/0007 §5):
    /// `{version, generation, name, phase}` — progress is poll-only.
    pub const APP: &str = "pylair://app";
}

#[cfg(test)]
mod tests;
