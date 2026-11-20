use crate::{Handle, StderrDelivery};
use tauri::{Manager, Runtime};

use crate::supervisor::fairness::FairnessRef;

/// Managed state holding the supervisor handle.
pub struct PyRuntime(pub Handle);

/// Which webviews may receive runtime events, and which webview owns each
/// streaming task. Nothing is broadcast to all windows: P1-8 — an app with
/// two webviews must not leak one window's streams into the other.
/// The fairness live set is a second, narrower membership (decisions 0005,
/// amended by 0006): `subscribe` alone does not join it — only
/// `subscribe_live` does, so a window that never calls Python reserves no
/// share.
#[derive(Default)]
pub struct WindowRegistry {
    subscribed: std::collections::HashSet<String>,
    /// The supervisor's shared fairness map, wired by the builder.
    /// Absent in tests that build a bare registry.
    fairness: Option<FairnessRef>,
}

impl WindowRegistry {
    /// Attach the supervisor's fairness map so `subscribe_live`/`drop_label`
    /// maintain the live set (builder wiring only).
    pub(crate) fn set_fairness(&mut self, fairness: FairnessRef) {
        self.fairness = Some(fairness);
    }

    /// Mark a webview as an event subscriber (every bridge command does this
    /// for the invoking window; exposed for tests and explicit opt-in).
    /// Event delivery only — the window does NOT join the fairness live
    /// set; `subscribe_live` does both.
    pub fn subscribe(&mut self, label: &str) {
        self.subscribed.insert(label.to_string());
    }

    /// Subscribe AND join the fairness live set — `python_call` and
    /// `python_call_stream` only, at the subscription site before dispatch,
    /// so a window's first call is live before its first admission check
    /// (decisions 0006 §3, amending 0005 §3). A window that only polls
    /// state/health, cancels or shuts down stays a subscriber and reserves
    /// nothing.
    pub(crate) fn subscribe_live(&mut self, label: &str) {
        self.subscribed.insert(label.to_string());
        if let Some(fair) = &self.fairness {
            fair.lock().unwrap().join_window(label);
        }
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
    /// explicit WindowEvent::Destroyed). Leaving subscribers also
    /// leave the fairness live set.
    pub(super) fn drop_label(&mut self, label: &str) {
        self.subscribed.remove(label);
        if let Some(fair) = &self.fairness {
            fair.lock().unwrap().leave_window(label);
        }
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

/// `subscribed` for the two call commands: event subscriber AND fairness
/// live-set member — the join lands before dispatch, so the window's first
/// call is live before its first admission check. Every other bridge
/// command uses `subscribed` and reserves nothing.
pub(super) fn subscribed_live<R: Runtime>(window: &tauri::WebviewWindow<R>, reg: &Registry) {
    reg.lock().unwrap().subscribe_live(window.label());
}
