use crate::{Handle, StderrDelivery};
use tauri::{Manager, Runtime};

/// Managed state holding the supervisor handle.
pub struct PyRuntime(pub Handle);

/// Which webviews may receive runtime events, and which webview owns each
/// streaming task. Nothing is broadcast to all windows: P1-8 — an app with
/// two webviews must not leak one window's streams into the other.
#[derive(Default)]
pub struct WindowRegistry {
    subscribed: std::collections::HashSet<String>,
}

impl WindowRegistry {
    /// Mark a webview as an event subscriber (every bridge command does this
    /// for the invoking window; exposed for tests and explicit opt-in).
    pub fn subscribe(&mut self, label: &str) {
        self.subscribed.insert(label.to_string());
    }

    /// SPIKE ONLY: whether a webview is in the subscriber set. Lets the
    /// fn-scope tests prove a denied call does not subscribe its window.
    #[doc(hidden)]
    pub fn is_subscribed(&self, label: &str) -> bool {
        self.subscribed.contains(label)
    }

    /// Delivery targets for runtime-wide events (state, stderr, exposes).
    /// Stream chunks are addressed to their owning window directly.
    pub(super) fn broadcast_targets(&self) -> Vec<String> {
        self.subscribed.iter().cloned().collect()
    }

    /// Delivery targets for `pylair://stderr`: the app's allow-list
    /// intersected with subscribers. Stderr carries application log lines
    /// that a second window has no business reading — unlike state and
    /// exposes it is opt-in per window, not broadcast.
    pub(super) fn stderr_targets(&self, delivery: &StderrDelivery) -> Vec<String> {
        match delivery {
            StderrDelivery::All => self.broadcast_targets(),
            StderrDelivery::Only(labels) => self
                .subscribed
                .iter()
                .filter(|l| labels.contains(l))
                .cloned()
                .collect(),
        }
    }

    /// Forget dead windows (an emit_to that failed to deliver, or an
    /// explicit WindowEvent::Destroyed).
    pub(super) fn drop_label(&mut self, label: &str) {
        self.subscribed.remove(label);
    }
}

/// F4 (rc.15): an emit_to failure only means "the window is gone" when the
/// window actually no longer exists. A transient failure on a live window —
/// IPC hiccup, delivery racing a reload — must not silently unsubscribe it:
/// the window was never told it lost events, so it would miss state/exposes/
/// stderr until its next plugin command resubscribed it.
pub(super) fn handle_emit_failure<R: Runtime>(
    app: &tauri::AppHandle<R>,
    reg: &Registry,
    label: &str,
) {
    if app.get_webview_window(label).is_some() {
        if cfg!(debug_assertions) {
            eprintln!("pylair: emit_to({label}) failed on a live window; keeping subscription");
        }
    } else {
        reg.lock().unwrap().drop_label(label);
    }
}

/// Managed as `Arc` so spawned stream tasks can own a handle.
pub type Registry = std::sync::Arc<std::sync::Mutex<WindowRegistry>>;

pub(super) fn subscribed<R: Runtime>(window: &tauri::WebviewWindow<R>, reg: &Registry) {
    reg.lock().unwrap().subscribe(window.label());
}
