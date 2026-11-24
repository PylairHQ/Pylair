//! Live per-app update state (decision 0011 §2). One `UpdateGate`
//! exists per app that carries a `plugins.updater` config block; it is
//! the host-side writer/reader of the `updater-state/` file contract
//! ([`super`]) — everything the watchdog will act on passes through
//! it.

use super::{
    health_update, new_session_id, phase, read_pending, state_dir, write_boot_marker,
    write_failed_marker, write_ready_marker, Knobs, UpdatePhase, NO_COMPONENTS_GRACE_MS,
};
use crate::supervisor::{RuntimeEvent, State};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, Runtime};

pub struct UpdateGate {
    /// `<app_data_dir>/updater-state` — shared with the watchdog.
    pub dir: PathBuf,
    /// The running bundle version (`version` in tauri.conf.json).
    pub version: String,
    /// knobs resolved from `plugins.pylair.update` + §2 defaults.
    pub knobs: Knobs,
    /// `bundle.windows.nsis.installMode` normalized:
    /// "current-user" | "per-machine" | "both". Windows-only concept;
    /// the install command consults it on Windows only.
    pub install_mode: String,
    /// `plugins.updater.endpoints` — the rollback manifest is fetched
    /// from the first endpoint's directory.
    pub endpoints: Vec<String>,
    installing: AtomicBool,
    /// The `Update` stashed by `python_update_check`, consumed by
    /// download/install — held as the live object since `Update` is
    /// not `Clone`.
    #[cfg(feature = "updater")]
    pub(crate) offer: Mutex<Option<tauri_plugin_updater::Update>>,
    /// Verified artifact bytes stashed by `python_update_download`.
    #[cfg(feature = "updater")]
    pub(crate) downloaded: Mutex<Option<Vec<u8>>>,
    /// This generation's `app.component` phases — rebuilt on each
    /// generation via `seen_component`/`ready_seq` bookkeeping.
    components: Mutex<BTreeMap<String, String>>,
    seen_component: AtomicBool,
    /// Bumped at every runtime-Ready — invalidates outstanding grace
    /// timers from older generations.
    ready_seq: AtomicU64,
    /// False while the generation is not in Ready (dead, starting,
    /// restarting) — a stale grace timer must not confirm it.
    runtime_ready: AtomicBool,
    ready_since: Mutex<Option<Instant>>,
    /// Session nonce for this process's pending window — written into
    /// pending.json by install; the watchdog keys ready markers on it.
    pub session_id: String,
    /// Grace window for a first component report (tests shrink it).
    grace_ms: u64,
}

impl UpdateGate {
    /// Build the gate for an updater-configured app; `None` when the
    /// conf carries no `plugins.updater` block (a feature-off or
    /// never-opted-in app gets no update surface at all).
    ///
    /// Boot marker: a launch of the pending version writes one
    /// `boots/` file — the watchdog's crash-loop evidence (§2).
    pub fn new<R: Runtime>(app: &AppHandle<R>) -> Option<Arc<Self>> {
        let conf = serde_json::to_value(app.config()).ok()?;
        let updater = conf.pointer("/plugins/updater")?;
        let data = app.path().app_data_dir().ok()?;
        let dir = state_dir(&data);
        let mode = conf
            .pointer("/bundle/windows/nsis/installMode")
            .and_then(Value::as_str)
            .unwrap_or("currentUser");
        let install_mode = match mode {
            "perMachine" => "per-machine",
            "both" => "both",
            _ => "current-user",
        };
        let gate = Self {
            dir: dir.clone(),
            version: conf
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            knobs: Knobs::from_conf(conf.pointer("/plugins/pylair")),
            install_mode: install_mode.to_string(),
            endpoints: updater
                .get("endpoints")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            installing: AtomicBool::new(false),
            #[cfg(feature = "updater")]
            offer: Mutex::new(None),
            #[cfg(feature = "updater")]
            downloaded: Mutex::new(None),
            components: Mutex::new(BTreeMap::new()),
            seen_component: AtomicBool::new(false),
            ready_seq: AtomicU64::new(0),
            runtime_ready: AtomicBool::new(false),
            ready_since: Mutex::new(None),
            session_id: new_session_id(),
            grace_ms: NO_COMPONENTS_GRACE_MS,
        };
        gate.note_boot();
        Some(Arc::new(gate))
    }

    /// Constructor for tests and non-app contexts: same gate, explicit
    /// inputs, no conf/AppHandle needed.
    #[doc(hidden)]
    pub fn with_dir(opts: GateOpts) -> Arc<Self> {
        let gate = Self {
            dir: opts.dir,
            version: opts.version,
            knobs: opts.knobs,
            install_mode: opts.install_mode,
            endpoints: opts.endpoints,
            installing: AtomicBool::new(false),
            #[cfg(feature = "updater")]
            offer: Mutex::new(None),
            #[cfg(feature = "updater")]
            downloaded: Mutex::new(None),
            components: Mutex::new(BTreeMap::new()),
            seen_component: AtomicBool::new(false),
            ready_seq: AtomicU64::new(0),
            runtime_ready: AtomicBool::new(false),
            ready_since: Mutex::new(None),
            session_id: opts.session_id.unwrap_or_else(new_session_id),
            grace_ms: opts.grace_ms,
        };
        gate.note_boot();
        Arc::new(gate)
    }

    /// A launch of the pending version writes one `boots/` file — the
    /// watchdog's crash-loop evidence (§2 `max_boots`).
    fn note_boot(&self) {
        if let Some(p) = read_pending(&self.dir) {
            if p.to == self.version {
                if let Err(e) = write_boot_marker(&self.dir, &p, std::process::id()) {
                    eprintln!("[pylair] update: boot marker write failed: {e}");
                }
            }
        }
    }

    /// Tap on the supervisor event stream — called from the builder's
    /// event fan-out before webview delivery.
    pub fn on_event(self: &Arc<Self>, ev: &RuntimeEvent) {
        match ev {
            RuntimeEvent::StateChanged { to, .. } => {
                if *to == State::Ready {
                    self.note_ready();
                } else {
                    self.runtime_ready.store(false, Ordering::SeqCst);
                }
            }
            RuntimeEvent::AppChanged(v) => {
                let name = v.get("name").and_then(Value::as_str);
                let phase = v.get("phase").and_then(Value::as_str);
                if let (Some(n), Some(p)) = (name, phase) {
                    self.note_component(n, p);
                }
            }
            _ => {}
        }
    }

    /// Inputs for [`UpdateGate::with_dir`] — the gate minus the
    /// tauri.conf.json plumbing.
    #[doc(hidden)]
    pub fn note_ready_pub(self: &Arc<Self>) {
        self.note_ready();
    }

    /// Test access to the component-transition arm of [`Self::on_event`].
    #[doc(hidden)]
    pub fn note_component_pub(&self, name: &str, phase: &str) {
        self.note_component(name, phase);
    }

    /// Runtime reached Ready. A component-reporting app confirms via
    /// its components; a non-instrumented app confirms after a grace
    /// window in which no component report arrived
    /// (`NO_COMPONENTS_GRACE_MS` — the only wall clock on the confirm
    /// path, so a slow first component report is never misread as
    /// "no instrumentation").
    fn note_ready(self: &Arc<Self>) {
        self.runtime_ready.store(true, Ordering::SeqCst);
        *self.ready_since.lock().unwrap() = Some(Instant::now());
        let seq = self.ready_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if self.seen_component.load(Ordering::SeqCst) {
            self.try_ready_marker();
            return;
        }
        let gate = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(gate.grace_ms));
            if gate.ready_seq.load(Ordering::SeqCst) != seq
                || !gate.runtime_ready.load(Ordering::SeqCst)
                || gate.seen_component.load(Ordering::SeqCst)
            {
                return;
            }
            gate.try_ready_marker();
        });
    }

    /// An `app.component` transition landed (decision 0007). A `failed`
    /// phase during the pending window writes the failed marker the
    /// watchdog treats as evidence of death; all-ready writes the
    /// ready marker.
    fn note_component(&self, name: &str, phase: &str) {
        self.seen_component.store(true, Ordering::SeqCst);
        self.components
            .lock()
            .unwrap()
            .insert(name.to_string(), phase.to_string());
        if phase == "failed" {
            if let Some(p) = read_pending(&self.dir) {
                if p.to == self.version {
                    if let Err(e) = write_failed_marker(&self.dir, &p, name) {
                        eprintln!("[pylair] update: failed marker write failed: {e}");
                    }
                }
            }
            return;
        }
        if phase == "ready" {
            let all = self
                .components
                .lock()
                .unwrap()
                .values()
                .all(|p| p == "ready");
            if all {
                self.try_ready_marker();
            }
        }
    }

    /// Write `ready-<version>` if a pending update targets the running
    /// version. Re-reads pending.json at write time — the marker
    /// carries the file's CURRENT session_id, so a pending written by
    /// a newer install supersedes instead of satisfying stale state.
    fn try_ready_marker(&self) {
        let Some(p) = read_pending(&self.dir) else {
            return;
        };
        if p.to != self.version {
            return;
        }
        if let Err(e) = write_ready_marker(&self.dir, &p) {
            eprintln!("[pylair] update: ready marker write failed: {e}");
        }
    }

    /// `health().update` for this app.
    pub fn health(&self) -> Value {
        health_update(&self.dir, self.installing.load(Ordering::SeqCst))
    }

    /// The `updating` readiness component's phase, or `None` when no
    /// update activity exists to report.
    pub fn component_state(&self) -> Option<&'static str> {
        match phase(&self.dir, self.installing.load(Ordering::SeqCst)) {
            UpdatePhase::None => None,
            p => Some(p.as_str()),
        }
    }

    /// `installing` is process-local — the post-install process either
    /// exits (Windows) or continues until relaunch, so a flag
    /// suffices.
    pub fn set_installing(&self, v: bool) {
        self.installing.store(v, Ordering::SeqCst);
    }
}

/// Inputs for [`UpdateGate::with_dir`] — the gate minus the
/// tauri.conf.json plumbing.
pub struct GateOpts {
    pub dir: PathBuf,
    pub version: String,
    pub knobs: Knobs,
    pub install_mode: String,
    pub endpoints: Vec<String>,
    pub session_id: Option<String>,
    pub grace_ms: u64,
}
