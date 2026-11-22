//! Tauri plugin: supervisor, IPC bridge and permission enforcement for the
//! managed Python runtime.
//!
//! `protocol` / `transport` / `supervisor` are Tauri-free and testable in
//! isolation. The plugin glue owns the supervisor `Handle`, forwards
//! lifecycle events to the frontend, and exposes the `python_*` commands.

pub mod protocol;
pub mod supervisor;
pub mod transport;

mod builder;
mod commands;
mod fn_scope;
mod registry;
mod selftest;
mod spec;
mod util;
pub mod wrap;

pub use builder::{Builder, StderrDelivery};
pub use commands::{
    forward_stream, python_call_impl, python_call_stream_impl, python_health_impl,
    python_state_impl, StreamAck,
};
pub use protocol::ErrorObj;
pub use registry::{PyRuntime, Registry, WindowRegistry};
pub use selftest::{self_test_gui, self_test_headless, self_test_requested};
pub use spec::{app_spawn_spec, release_spawn_spec, SpawnSpecError};
pub use supervisor::{
    spawn as spawn_supervisor, Handle, HealthSnapshot, RuntimeEvent, RuntimeKind, SpawnSpec, State,
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
