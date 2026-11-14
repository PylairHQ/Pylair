//! Tauri plugin: supervisor, IPC bridge and permission enforcement for the
//! managed Python runtime.
//!
//! `protocol` / `transport` / `supervisor` are Tauri-free and testable in
//! isolation. The plugin glue owns the supervisor `Handle`, forwards
//! lifecycle events to the frontend, and exposes the `python_*` commands.

pub mod protocol;
pub mod supervisor;
pub mod transport;

pub use protocol::ErrorObj;
pub use supervisor::{
    spawn as spawn_supervisor, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::{Emitter, Manager, Runtime};

/// The interpreter spec for a packaged app, given the directory holding
/// `python/`, `site-packages/` and the app's source dir. Kept here (not in
/// app code) so path/env fixes reach every app via a plugin update.
pub fn release_spawn_spec(resources: &Path, app_module: &str, source_dir: &str) -> SpawnSpec {
    // Tauri's resource_dir is canonicalized, so on Windows it arrives as a
    // \\?\ verbatim path — Python code reading sys.executable or sys.path
    // can trip on the prefix. Simplify it back when safe (no-op elsewhere).
    let resources = dunce::simplified(resources);
    let python = resources.join("python").join(if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    });
    let sep = if cfg!(windows) { ";" } else { ":" };
    // Import root = PARENT of the source dir: "backend.app" resolves from
    // resources/, not resources/backend/ (mirrors cli `importRoot`).
    let import_root = resources.join(Path::new(source_dir).parent().unwrap_or(Path::new("")));
    let pythonpath = format!(
        "{}{}{}",
        import_root.display(),
        sep,
        resources.join("site-packages").display()
    );
    let mut spec = SpawnSpec::pylair(python, app_module);
    spec.env.push(("PYTHONPATH".into(), pythonpath));
    spec.env.push(("PYTHONUTF8".into(), "1".into()));
    spec.env.push(("PYTHONNOUSERSITE".into(), "1".into()));
    // `python -m` puts the child's cwd first on sys.path — a json.py,
    // asyncio/ or pylair/ in the directory the user launched from would
    // shadow the stdlib or the runtime itself (isolated workers inherit
    // this env, so they are covered too). PYTHONSAFEPATH (3.11+, and our
    // bundled runtime is 3.12) removes the implicit entry; PYTHONPATH
    // above still resolves the app module and bundled site-packages.
    spec.env.push(("PYTHONSAFEPATH".into(), "1".into()));
    // The bundle is precompiled at build time and the install dir is
    // often read-only (per-machine installs, .app bundles): never write
    // bytecode at launch. Isolated workers inherit os.environ, so they
    // are covered too — same for subprocesses the app itself spawns,
    // unless that call passes its own env. Dev mode is untouched:
    // __pycache__ in the source tree is normal and useful there.
    spec.env
        .push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    // Pin cwd to the bundle rather than whatever dir launched the app —
    // deterministic for any code that opens relative paths, and empty
    // of user-writable shadow candidates.
    spec.cwd = Some(resources.to_path_buf());
    // The packaged interpreter must not inherit Python configuration from
    // whatever launched the app: an AppImage's AppRun exports PYTHONHOME
    // (pointing at the AppDir), which breaks the bundled stdlib lookup.
    // PYTHONBREAKPOINT names a module imported by breakpoint() — a launch
    // env var must not get code executed inside the runtime.
    spec.env_remove.extend(
        [
            "PYTHONHOME",
            "PYTHONSTARTUP",
            "PYTHONEXECUTABLE",
            "PYTHONUSERBASE",
            "PYTHONINSPECT",
            "PYTHONPLATLIBDIR",
            "PYTHONWARNINGS",
            "PYTHONBREAKPOINT",
        ]
        .map(String::from),
    );
    scrub_appdir(&mut spec, |k| std::env::var(k).ok());
    spec
}

/// An AppImage's AppRun exports `APPDIR` and prepends its own dirs to
/// `LD_LIBRARY_PATH` and `PATH`. A bundled Python inheriting those can load
/// the AppImage's `libstdc++`/`libssl` instead of the system's and break
/// native wheels (numpy, torch): drop every path entry under `APPDIR`.
/// Removing the var entirely is the fallback when nothing else remains.
fn scrub_appdir(spec: &mut SpawnSpec, env: impl Fn(&str) -> Option<String>) {
    let Some(appdir) = env("APPDIR") else { return };
    let appdir = appdir.trim_end_matches('/');
    if appdir.is_empty() {
        return;
    }
    for name in ["LD_LIBRARY_PATH", "PATH"] {
        let Some(value) = env(name) else { continue };
        let kept: Vec<&str> = value
            .split(':')
            .filter(|e| {
                let e = e.trim_end_matches('/');
                !(e == appdir || e.strip_prefix(appdir).is_some_and(|r| r.starts_with('/')))
            })
            .collect();
        if kept.is_empty() {
            spec.env_remove.push(name.into());
        } else {
            spec.env.push((name.into(), kept.join(":")));
        }
    }
}

/// Why a spawn spec could not be computed. Surfaced through plugin setup
/// as a startup error instead of a panic.
#[derive(Debug)]
#[non_exhaustive]
pub enum SpawnSpecError {
    /// `app.path().resource_dir()` failed AND no bundled resources were
    /// found next to the executable either. Carries Tauri's error — on
    /// macOS it names the symlinked ancestor Tauri refuses to run under.
    ResourceDir(String),
}

impl std::fmt::Display for SpawnSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResourceDir(e) => write!(
                f,
                "resource_dir() failed ({e}) and no bundled resources were \
                 found next to the executable. On macOS this happens when \
                 the app is launched through a symlinked path (e.g. /tmp \
                 -> /private/tmp): launch the app's real path instead."
            ),
        }
    }
}

impl std::error::Error for SpawnSpecError {}

/// Dev-or-release spec: `pylair dev` exports PYLAIR_APP_MODULE / PYLAIR_PYTHON
/// / PYLAIR_PYTHONPATH / PYLAIR_RESTART_TRIGGER → the project venv; a packaged
/// build resolves `resource_dir`. Apps use this as the plugin's spec fn.
pub fn app_spawn_spec<R: Runtime>(
    app: &tauri::AppHandle<R>,
    app_module: &str,
    source_dir: &str,
) -> Result<SpawnSpec, SpawnSpecError> {
    // Development overrides are honored in debug builds only: a packaged app
    // must never let the launching environment redirect which interpreter or
    // module it runs.
    if cfg!(debug_assertions) {
        if let Ok(module) = std::env::var("PYLAIR_APP_MODULE") {
            let program = std::env::var("PYLAIR_PYTHON").unwrap_or_else(|_| "python3".into());
            let mut spec = SpawnSpec::pylair(PathBuf::from(program), &module);
            if let Ok(pp) = std::env::var("PYLAIR_PYTHONPATH") {
                spec.env.push(("PYTHONPATH".into(), pp));
            }
            // NB: PYLAIR_RESTART_TRIGGER is read by the supervisor from its own
            // env (inherited from `pylair dev`) — not forwarded to the child.
            return Ok(spec);
        }
    }
    let res = bundled_resource_dir(app.path().resource_dir())?;
    Ok(release_spawn_spec(&res, app_module, source_dir))
}

/// `resource_dir()` first, exe-relative fallback second. Tauri rejects an
/// exe path containing a symlinked ancestor on macOS (the StartingBinary
/// hardening behind `current_exe`) — a launch through `/tmp` errors out
/// even though the bundle is perfectly readable. The fallback resolves
/// resources relative to the canonicalized executable, so that launch
/// still works; if both fail the error names the cause and the fix.
fn bundled_resource_dir(
    resource_dir: Result<PathBuf, tauri::Error>,
) -> Result<PathBuf, SpawnSpecError> {
    match resource_dir {
        Ok(dir) => Ok(dir),
        Err(e) => resources_from_exe().ok_or_else(|| SpawnSpecError::ResourceDir(e.to_string())),
    }
}

// ── bundled self-test ───────────────────────────────────────────────
// Release-path verification apps can ship: `pylair-self-test` resolves the
// bundled resources exactly as the packaged app sees them, spawns the real
// supervisor, makes one real call, prints, exits. Two entry points:
//   headless — before any AppHandle exists (no window, no Tauri loop);
//   gui      — inside setup(), exercising app.path().resource_dir() itself.

fn resources_from_exe() -> Option<PathBuf> {
    // Canonicalize first: a launch through a symlinked path (e.g. /tmp on
    // macOS → /private/tmp) is exactly the case Tauri's resource_dir()
    // rejects; resolving to the real exe path is what makes this fallback
    // succeed where resource_dir() failed.
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    resources_near_exe(&exe)
}

fn resources_near_exe(exe: &std::path::Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_string_lossy();
    [
        dir.join("..").join("lib").join(stem.as_ref()), // deb/AppImage usr/lib
        dir.join("..").join("Resources"),               // macOS .app
        dir.to_path_buf(),                              // NSIS install root
    ]
    .into_iter()
    .find(|p| p.join("python").exists())
    // Normalize `..` away so callers get the real bundle path.
    .and_then(|p| p.canonicalize().ok())
}

/// A GUI-subsystem exe has no console of its own — `println!`/`eprintln!`
/// (and panic reports) go nowhere. When the process was launched from a
/// terminal — the self-test CI legs — steal the parent's console so the
/// verdict still reaches it. Harmless no-op without a console host.
#[cfg(windows)]
fn attach_parent_console() {
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

async fn self_test_call(spec: SpawnSpec, origin: &str) -> i32 {
    // Print the interpreter actually being spawned — on a clean machine
    // this is the proof the bundled runtime is what's running, not a
    // system or project Python the app happened to find.
    eprintln!(
        "self-test({origin}): interpreter {}",
        spec.program.display()
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (h, actor) = spawn_supervisor(spec, SupervisorConfig::default(), tx);
    tokio::spawn(actor);
    // Surface the runtime's stderr and lifecycle: a self-test that only says
    // "never reached Ready" is undiagnosable in CI.
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                RuntimeEvent::Stderr(line) => eprintln!("[runtime stderr] {line}"),
                RuntimeEvent::StateChanged { from, to, detail } => {
                    eprintln!("[runtime state] {from:?} -> {to:?}: {detail}")
                }
                RuntimeEvent::Exposes(_) => {}
            }
        }
    });
    let mut rx = h.subscribe_state();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while *rx.borrow() != State::Ready {
            if rx.changed().await.is_err() {
                break;
            }
        }
        *rx.borrow() == State::Ready
    })
    .await;
    match ready {
        Ok(true) => match h
            .call("hello", json!({"name": "self-test"}), None, Some(false))
            .await
        {
            Ok(v) => {
                println!("self-test({origin}) ok: {v}");
                h.shutdown().await;
                0
            }
            Err(e) => {
                eprintln!("self-test({origin}): call failed: {e:?}");
                3
            }
        },
        _ => {
            eprintln!(
                "self-test({origin}): never reached Ready (state: {:?})",
                h.state()
            );
            4
        }
    }
}

/// Headless: resources located relative to the executable — runs on a bare
/// machine with no display and no system Python. Returns an exit code.
pub fn self_test_headless(app_module: &str, source_dir: &str) -> i32 {
    attach_parent_console();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("self-test: cannot build tokio runtime: {e}");
            return 2;
        }
    };
    rt.block_on(async {
        match resources_from_exe() {
            Some(res) => {
                self_test_call(release_spawn_spec(&res, app_module, source_dir), "headless").await
            }
            None => {
                eprintln!("self-test: no bundled resources near the executable");
                2
            }
        }
    })
}

/// GUI: runs inside the app's setup() — exercises resource_dir() itself.
/// Spawn on the Tauri runtime; exit the app with the returned code.
pub async fn self_test_gui<R: Runtime>(
    app: &tauri::AppHandle<R>,
    app_module: &str,
    source_dir: &str,
) -> i32 {
    attach_parent_console();
    let spec = match app_spawn_spec(app, app_module, source_dir) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("self-test(gui): spawn spec failed: {e}");
            return 2;
        }
    };
    self_test_call(spec, "gui").await
}

/// True when the process should run the headless self-test instead of the app.
pub fn self_test_requested() -> bool {
    std::env::args().any(|a| a == "--self-test") || std::env::var("PYLAIR_SELF_TEST").is_ok()
}

/// Frontend event names (kept in one place — the client SDK mirrors them).
pub mod events {
    /// `{from, to, detail}` lifecycle transitions.
    pub const STATE: &str = "pylair://state";
    /// One line of runtime stderr.
    pub const STDERR: &str = "pylair://stderr";
    /// The exposes manifest after handshake.
    pub const EXPOSES: &str = "pylair://exposes";
}

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

    /// Delivery targets for runtime-wide events (state, stderr, exposes).
    /// Stream chunks are addressed to their owning window directly.
    fn broadcast_targets(&self) -> Vec<String> {
        self.subscribed.iter().cloned().collect()
    }

    /// Delivery targets for `pylair://stderr`: the app's allow-list
    /// intersected with subscribers. Stderr carries application log lines
    /// that a second window has no business reading — unlike state and
    /// exposes it is opt-in per window, not broadcast.
    fn stderr_targets(&self, delivery: &StderrDelivery) -> Vec<String> {
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
    fn drop_label(&mut self, label: &str) {
        self.subscribed.remove(label);
    }
}

/// F4 (rc.15): an emit_to failure only means "the window is gone" when the
/// window actually no longer exists. A transient failure on a live window —
/// IPC hiccup, delivery racing a reload — must not silently unsubscribe it:
/// the window was never told it lost events, so it would miss state/exposes/
/// stderr until its next plugin command resubscribed it.
fn handle_emit_failure<R: Runtime>(app: &tauri::AppHandle<R>, reg: &Registry, label: &str) {
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

fn subscribed<R: Runtime>(window: &tauri::WebviewWindow<R>, reg: &Registry) {
    reg.lock().unwrap().subscribe(window.label());
}

/// Which webviews may receive `pylair://stderr`. Stderr carries the
/// runtime's log lines — application output a second window has no
/// business reading — so unlike `pylair://state`/`pylair://exposes` it is
/// opt-in per window rather than broadcast to every subscriber.
#[derive(Clone, Debug)]
pub enum StderrDelivery {
    /// Only these window labels receive stderr. Default: `["main"]`.
    Only(Vec<String>),
    /// Every subscribed window — the pre-restriction broadcast. An
    /// explicit opt-out for apps that want all windows to see logs.
    All,
}

impl Default for StderrDelivery {
    fn default() -> Self {
        Self::Only(vec!["main".into()])
    }
}

/// Plugin builder — the app supplies where Python lives, eagerly or lazily.
/// `new` takes a fixed spec; `new_with` defers it to setup time, which is how
/// apps resolve the bundled interpreter under `resource_dir` at release while
/// using a dev venv when `PYLAIR_APP_MODULE` is set.
type SpecFn<R> =
    Box<dyn Fn(&tauri::AppHandle<R>) -> Result<SpawnSpec, SpawnSpecError> + Send + Sync>;

pub struct Builder<R: Runtime = tauri::Wry> {
    spec: SpawnSpec,
    lazy: Option<SpecFn<R>>,
    config: SupervisorConfig,
    stderr: StderrDelivery,
}

impl<R: Runtime> Builder<R> {
    pub fn new(spec: SpawnSpec) -> Self {
        Self {
            spec,
            lazy: None,
            config: SupervisorConfig::default(),
            stderr: StderrDelivery::default(),
        }
    }

    /// Spec computed at plugin setup — receives the AppHandle so it can
    /// resolve `path().resource_dir()`. A spec-fn error aborts startup
    /// with the message as the Tauri plugin error, not a panic.
    pub fn new_with(
        f: impl Fn(&tauri::AppHandle<R>) -> Result<SpawnSpec, SpawnSpecError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            spec: SpawnSpec::default(),
            lazy: Some(Box::new(f)),
            config: SupervisorConfig::default(),
            stderr: StderrDelivery::default(),
        }
    }

    pub fn config(mut self, config: SupervisorConfig) -> Self {
        self.config = config;
        self
    }

    /// Which webviews receive `pylair://stderr` — default
    /// `StderrDelivery::Only(["main"])`. Multi-window apps that show the
    /// log in a non-main window must list that window's label;
    /// `StderrDelivery::All` restores broadcast-to-every-subscriber.
    pub fn stderr_delivery(mut self, delivery: StderrDelivery) -> Self {
        self.stderr = delivery;
        self
    }

    /// How long the supervisor waits for the runtime's `hello` before
    /// declaring the spawn dead (protocol-v1 §4). Default
    /// `PYLAIR_HELLO_TIMEOUT_MS` or 30s; every source — this setter, the
    /// env var, a hand-built `SupervisorConfig` — is clamped to
    /// 1s..=600s at spawn and the clamp is logged on `pylair://stderr`.
    ///
    /// At half the deadline the host emits a repeated
    /// `starting → starting` `pylair://state` event whose `detail` says
    /// how long it has waited — informational only: `detail` wording is
    /// not protocol and clients must not depend on it.
    pub fn hello_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.config.hello_timeout = timeout;
        self
    }

    pub fn build(self) -> tauri::plugin::TauriPlugin<R> {
        let mut plugin = tauri::plugin::Builder::<R>::new("pylair");
        // Installed-webview self-test: the client sees this flag and runs its
        // checks through the real bridge, then calls python_selftest_report.
        if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_some() {
            // The verdict (and any panic) prints to stdout/stderr — a
            // GUI-subsystem exe needs the parent's console for it to land.
            attach_parent_console();
            plugin = plugin.js_init_script("window.__PYLAIR_SELF_TEST__ = true;".to_string());
        }
        plugin
            .setup(move |app, _api| {
                let spec = match self.lazy.as_ref() {
                    Some(f) => f(app)?,
                    None => self.spec.clone(),
                };
                let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<RuntimeEvent>(256);
                // setup runs on the main thread with no reactor entered —
                // spawn the actor via tauri's runtime, not bare tokio::spawn.
                let (handle, actor) = spawn_supervisor(spec, self.config.clone(), ev_tx);
                tauri::async_runtime::spawn(actor);
                app.manage(PyRuntime(handle));
                app.manage(Registry::new(Default::default()));

                let app_handle = app.clone();
                let stderr = self.stderr.clone();
                tauri::async_runtime::spawn(async move {
                    while let Some(ev) = ev_rx.recv().await {
                        let payload = match ev {
                            RuntimeEvent::StateChanged { from, to, detail } => Some((
                                events::STATE,
                                json!({"from": fmt_state(from), "to": fmt_state(to), "detail": detail}),
                            )),
                            RuntimeEvent::Stderr(line) => Some((events::STDERR, json!(line))),
                            RuntimeEvent::Exposes(x) => Some((events::EXPOSES, x)),
                        };
                        let Some((event, payload)) = payload else { continue };
                        // Runtime-wide events go to subscribed webviews only.
                        // Stderr is narrower: the app's allow-list.
                        let reg = app_handle.state::<Registry>();
                        let labels = match event {
                            events::STDERR => reg.lock().unwrap().stderr_targets(&stderr),
                            _ => reg.lock().unwrap().broadcast_targets(),
                        };
                        for label in labels {
                            if app_handle.emit_to(&label, event, payload.clone()).is_err() {
                                handle_emit_failure(&app_handle, &reg, &label);
                            }
                        }
                    }
                });
                Ok(())
            })
            .on_event(|app, event| {
                // Two-window event-scoping self-test: a second webview
                // exists only under the flag. Its capability grants
                // emit/emitTo on purpose — the test proves the delivery
                // fix itself, not the deny-emit stopgap. Window creation
                // must wait for RunEvent::Ready (plugin setup runs before
                // the event loop starts) AND must run off the main thread:
                // create_window dispatches to the loop and blocks on the
                // ack, which deadlocks inside an event callback.
                if let tauri::RunEvent::Ready = event {
                    if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_some() {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = tauri::WebviewWindowBuilder::new(
                                &app,
                                "win-b",
                                tauri::WebviewUrl::App("index.html".into()),
                            )
                            .build()
                            {
                                eprintln!("self-test win-b creation failed: {e}");
                            }
                        });
                    }
                }
                // A destroyed window leaves the subscriber set; a reload does
                // NOT remove it (the label survives — the new document keeps
                // receiving broadcast lifecycle/stderr events, which is what
                // subscribers want). Its tasks are reaped by on_page_load.
                if let tauri::RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::Destroyed,
                    ..
                } = event
                {
                    app.state::<Registry>().lock().unwrap().drop_label(label);
                    // Its tasks die with it — nobody left to see the results.
                    // Window events run on the main thread with no reactor
                    // entered — spawn the send on Tauri's runtime, exactly
                    // like the actor/Exit/on_drop paths below.
                    if let Some(rt) = app.try_state::<PyRuntime>() {
                        let h = rt.0.clone();
                        let owner = label.to_string();
                        tauri::async_runtime::spawn(async move { h.cancel_owner(owner).await });
                    }
                }
                // Block exit until the runtime's process tree is gone: the
                // async drop below may never get to run once the process ends.
                if let tauri::RunEvent::Exit = event {
                    if let Some(rt) = app.try_state::<PyRuntime>() {
                        let handle = rt.0.clone();
                        tauri::async_runtime::block_on(async move {
                            let _ = tokio::time::timeout(
                                std::time::Duration::from_secs(15),
                                handle.shutdown(),
                            )
                            .await;
                        });
                    }
                }
            })
            .on_page_load(|webview, payload| {
                // A new document replaces the page's JS context: every call
                // the old document owned is orphaned — its Channel::send can
                // still return Ok through eval, so send-failure reaping
                // cannot catch this. PageLoadEvent::Started fires at the
                // document commit (WebView2 ContentLoading, WKWebView
                // didCommitNavigation, WebKitGTK LoadCommitted): reloads and
                // dev-server full reloads land here; same-document
                // navigations (anchors, history.pushState, SPA routes) do
                // not. The watermark keeps the race airtight — only tasks
                // allocated BEFORE this navigation are reaped; a call the
                // new document races in registers an id >= the mark and
                // survives. The client-side pagehide hook stays as belt.
                if payload.event() == tauri::webview::PageLoadEvent::Started {
                    if let Some(rt) = webview.try_state::<PyRuntime>() {
                        let h = rt.0.clone();
                        let before = h.task_watermark();
                        let owner = webview.label().to_string();
                        tauri::async_runtime::spawn(async move {
                            h.cancel_owner_before(owner, before).await;
                        });
                    }
                }
            })
            .on_drop(|app| {
                if let Some(rt) = app.try_state::<PyRuntime>() {
                    let handle = rt.0.clone();
                    tauri::async_runtime::spawn(async move { handle.shutdown().await });
                }
            })
            .invoke_handler(tauri::generate_handler![
                python_call,
                python_call_stream,
                python_grant_credit,
                python_cancel,
                python_retry,
                python_state,
                python_shutdown,
                python_selftest_report,
                python_selftest_b_report,
            ])
            .build()
    }
}

/// Exact-origin check for wrap mode (P1-8): two URLs share an origin iff
/// scheme, host and effective port are equal — parse, never prefix-match.
/// `http://127.0.0.1:5000.evil.com` (fails to parse as the expected host)
/// and `http://127.0.0.1@evil.com` (userinfo tricks) are NOT the app origin.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (tauri::Url::parse(a), tauri::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

fn fmt_state(s: State) -> &'static str {
    match s {
        State::Starting => "starting",
        State::Ready => "ready",
        State::Unresponsive => "unresponsive",
        State::Restarting => "restarting",
        State::Failed => "failed",
        State::Stopped => "stopped",
    }
}

#[tauri::command]
async fn python_call<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    fn_name: String,
    input: Value,
) -> Result<Value, ErrorObj> {
    subscribed(&window, &reg);
    rt.0.call(
        &fn_name,
        input,
        Some(window.label().to_string()),
        Some(false),
    )
    .await
}

/// The `python_call_stream` answer. `protocol` is the client↔plugin
/// bridge version (2 = per-call channel delivery), NOT the host↔Python
/// wire protocol (`protocol::PROTOCOL_VERSION` = 1). It exists so a new
/// client can spot an old plugin: the old command returned a bare task
/// id and ignored `onEvent`, which would hang the call forever — a
/// bare-number answer tells the client to fail loudly instead.
const BRIDGE_PROTOCOL: u32 = 2;

#[derive(serde::Serialize)]
pub struct StreamAck {
    pub task: u64,
    pub protocol: u32,
}

/// Start a call with per-call channel delivery. Returns `{task,
/// protocol}`; chunks and the terminal arrive on `on_event` — delivery
/// is bound to the invoking webview, no shared event bus (event-scoping
/// fix).
///
/// `stream` is the client's declared intent: `false` for `call()`
/// (task-slot accounting only — no stream slot, no initial credit), or
/// `true` for `stream()`. Absent (older clients) the task is accounted
/// as a stream exactly as before but NO `stream` field reaches the
/// runtime, so no mismatch validation applies.
#[tauri::command]
async fn python_call_stream<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    on_event: tauri::ipc::Channel<Value>,
    fn_name: String,
    input: Value,
    stream: Option<bool>,
) -> Result<StreamAck, ErrorObj> {
    python_call_stream_impl(&window, &reg, &rt, on_event, fn_name, input, stream).await
}

/// `python_call_stream`'s body, callable in tests — driving it is the
/// real command path minus Tauri's invoke-arg deserialization.
#[doc(hidden)]
pub async fn python_call_stream_impl<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    reg: &Registry,
    rt: &PyRuntime,
    on_event: tauri::ipc::Channel<Value>,
    fn_name: String,
    input: Value,
    stream: Option<bool>,
) -> Result<StreamAck, ErrorObj> {
    subscribed(window, reg);
    let label = window.label().to_string();
    let (task, items) =
        rt.0.call_stream(&fn_name, input, Some(label.clone()), stream)
            .await?;
    forward_stream(on_event, items, rt.0.clone(), task, label);
    Ok(StreamAck {
        task,
        protocol: BRIDGE_PROTOCOL,
    })
}

/// Forward one call's stream items to its per-call channel. A dead
/// consumer — `Channel::send` fails when the webview is gone — reaps
/// that task and stops forwarding, so no stream slot or credit is left
/// tied up. The reap is per-task, not per-label: a webview closed and
/// reopened under the same label can hold newer live calls that a stale
/// forwarder must not kill. Uses `cancel_task`: the awaited send cannot
/// be dropped by a full command queue the way a `try_send` cancel can.
/// Returns the forwarder's join handle (tests await it).
#[doc(hidden)]
pub fn forward_stream(
    on_event: tauri::ipc::Channel<Value>,
    mut items: supervisor::StreamRx,
    rt: Handle,
    task: u64,
    owner: String,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        while let Some(item) = items.recv().await {
            let payload = match item {
                StreamItem::Chunk { seq, value } => {
                    json!({"task": task, "seq": seq, "value": value, "done": false})
                }
                StreamItem::End(Ok(result)) => {
                    json!({"task": task, "done": true, "result": result})
                }
                StreamItem::End(Err(e)) => {
                    json!({"task": task, "done": true, "error": e})
                }
            };
            if on_event.send(payload).is_err() {
                rt.cancel_task(task, owner.clone()).await;
                break;
            }
        }
    })
}

#[tauri::command]
async fn python_grant_credit<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    task: u64,
    n: u64,
) -> Result<(), ErrorObj> {
    subscribed(&window, &reg);
    rt.0.grant_credit(task, n, Some(window.label().to_string()))
        .await
}

/// End of the installed-webview self-test: print the verdict and exit with
/// it. Inert unless PYLAIR_SELF_TEST_WEBVIEW is set, so shipping the
/// permission in a release app grants nothing.
#[tauri::command]
fn python_selftest_report<R: Runtime>(
    app: tauri::AppHandle<R>,
    result: Value,
) -> Result<(), ErrorObj> {
    if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_none() {
        return Err(ErrorObj {
            code: protocol::INVALID_REQUEST,
            message: "webview self-test not enabled".into(),
            data: None,
        });
    }
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    println!("webview-self-test: {result}");
    app.exit(if ok { 0 } else { 5 });
    Ok(())
}

/// Intruder's verdict for the two-window self-test. win-b cannot send it
/// via JS emit/emit_to: a `core:event:deny-emit(-to)` entry (as the default
/// template ships for the main webview) denies those commands origin-wide —
/// Tauri's authority ignores `windows:` scoping for denials — so the report
/// relays through this ACL-checked command and the Rust side emits. Rust
/// emit paths are not subject to webview permissions. Unlike
/// python_selftest_report this does not exit the app, so win-b can hold it
/// without gaining shutdown power. Inert unless PYLAIR_SELF_TEST_WEBVIEW.
#[tauri::command]
fn python_selftest_b_report<R: Runtime>(
    app: tauri::AppHandle<R>,
    report: Value,
) -> Result<(), ErrorObj> {
    if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_none() {
        return Err(ErrorObj {
            code: protocol::INVALID_REQUEST,
            message: "webview self-test not enabled".into(),
            data: None,
        });
    }
    app.emit_to("main", "pylair://selftest-b", report)
        .map_err(|e| ErrorObj {
            code: protocol::INTERNAL,
            message: e.to_string(),
            data: None,
        })
}

#[tauri::command]
async fn python_cancel<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    task: u64,
) -> Result<supervisor::CancelOutcome, ErrorObj> {
    subscribed(&window, &reg);
    rt.0.cancel(task, Some(window.label().to_string())).await
}

/// Operator-initiated recovery: respawn the runtime out of the Failed state.
#[tauri::command]
async fn python_retry<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> Result<(), ErrorObj> {
    subscribed(&window, &reg);
    rt.0.retry().await
}

#[tauri::command]
fn python_state<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> &'static str {
    subscribed(&window, &reg);
    fmt_state(rt.0.state())
}

#[tauri::command]
async fn python_shutdown<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> Result<supervisor::ShutdownOutcome, ErrorObj> {
    subscribed(&window, &reg);
    Ok(rt.0.shutdown().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn env_get(spec: &SpawnSpec, k: &str) -> Option<String> {
        spec.env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn appdir_entries_are_scrubbed_from_child_paths() {
        let mut spec = SpawnSpec::default();
        scrub_appdir(
            &mut spec,
            fake_env(&[
                ("APPDIR", "/tmp/.mount_App123"),
                (
                    "LD_LIBRARY_PATH",
                    "/tmp/.mount_App123/usr/lib:/tmp/.mount_App123/lib:/usr/lib",
                ),
                ("PATH", "/tmp/.mount_App123/usr/bin:/usr/bin:/bin"),
            ]),
        );
        assert_eq!(
            env_get(&spec, "LD_LIBRARY_PATH").as_deref(),
            Some("/usr/lib")
        );
        assert_eq!(env_get(&spec, "PATH").as_deref(), Some("/usr/bin:/bin"));
        assert!(!spec.env_remove.iter().any(|k| k == "PATH"));
    }

    #[test]
    fn appdir_only_entries_remove_the_var() {
        let mut spec = SpawnSpec::default();
        scrub_appdir(
            &mut spec,
            fake_env(&[("APPDIR", "/a"), ("LD_LIBRARY_PATH", "/a/usr/lib:/a/lib")]),
        );
        assert!(spec.env_remove.iter().any(|k| k == "LD_LIBRARY_PATH"));
        assert!(env_get(&spec, "LD_LIBRARY_PATH").is_none());
    }

    #[test]
    fn sibling_prefix_is_not_scrubbed() {
        // /appdir2 is NOT under /appdir — a naive starts_with would eat it.
        let mut spec = SpawnSpec::default();
        scrub_appdir(
            &mut spec,
            fake_env(&[
                ("APPDIR", "/appdir"),
                ("LD_LIBRARY_PATH", "/appdir2/lib:/appdir/lib"),
            ]),
        );
        assert_eq!(
            env_get(&spec, "LD_LIBRARY_PATH").as_deref(),
            Some("/appdir2/lib")
        );
    }

    #[test]
    fn no_appdir_is_a_noop() {
        let mut spec = SpawnSpec::default();
        scrub_appdir(&mut spec, fake_env(&[("LD_LIBRARY_PATH", "/x")]));
        assert!(spec.env.is_empty() && spec.env_remove.is_empty());
    }

    #[test]
    fn origin_match_is_exact() {
        let app = "http://127.0.0.1:5000/";
        assert!(same_origin(app, "http://127.0.0.1:5000/path?q=1"));
        // Bypass attempts: evil subdomains and userinfo tricks.
        assert!(!same_origin(app, "http://127.0.0.1:5000.evil.com/"));
        assert!(!same_origin(app, "http://127.0.0.1@evil.com/"));
        assert!(!same_origin(app, "https://127.0.0.1:5000/")); // scheme differs
        assert!(!same_origin(app, "http://127.0.0.1:5001/")); // port differs
        assert!(!same_origin(app, "http://localhost:5000/")); // host differs
        assert!(!same_origin(app, "not a url"));
        // Opaque origins never match anything.
        assert!(!same_origin("about:blank", "about:blank"));
        // Default-port normalization: explicit :443 == https default.
        assert!(same_origin("https://a.example:443/", "https://a.example/"));
        // Case-insensitive host compare; trailing dot and IPv6 literals are
        // distinct origins (url-crate normalization decides, we don't guess).
        assert!(same_origin("http://EXAMPLE.com/", "http://example.com/"));
        assert!(!same_origin("http://localhost/", "http://localhost./"));
        assert!(same_origin("http://[::1]:8080/", "http://[::1]:8080/x"));
        // localhost and 127.0.0.1 are DIFFERENT origins — pinned so nobody
        // "fixes" the false negative into a false positive later.
        assert!(!same_origin("http://localhost/", "http://127.0.0.1/"));
    }

    #[test]
    fn window_registry_routes_to_owners_and_subscribers() {
        let mut reg = WindowRegistry::default();
        reg.subscribe("main");
        reg.subscribe("panel");
        assert_eq!(reg.broadcast_targets().len(), 2);
        reg.drop_label("panel");
        assert_eq!(reg.broadcast_targets(), vec!["main".to_string()]);
    }

    /// F4 (rc.15): a failed emit_to evicts the label only when the window
    /// is actually gone — a transient failure on a live window must keep
    /// the subscription. get_webview_window is the oracle: a destroyed
    /// window is removed from the manager and hits the same absent-label
    /// path the "ghost" case covers (MockRuntime can't move a live window
    /// to destroyed — its destroy() never runs the event loop's removal).
    /// Same webview-link constraint as plugin_setup.rs — not on Windows.
    #[cfg(not(windows))]
    #[test]
    fn failed_emit_evicts_only_gone_windows() {
        use tauri::test::{mock_context, noop_assets};
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        let app = tauri::test::mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app builds");
        let reg = Registry::default();
        {
            let mut r = reg.lock().unwrap();
            r.subscribe("w1");
            r.subscribe("ghost");
        }
        let w1 = WebviewWindowBuilder::new(&app, "w1", WebviewUrl::default())
            .build()
            .unwrap();

        // A live window survives a transient emit failure.
        handle_emit_failure(app.handle(), &reg, "w1");
        assert!(
            reg.lock()
                .unwrap()
                .broadcast_targets()
                .contains(&"w1".to_string()),
            "live window lost its subscription on a transient emit failure"
        );

        // A label with no window is evicted, exactly as before — this is
        // the path a destroyed window takes (the manager drops it from
        // webview_windows() when WindowEvent::Destroyed is processed).
        handle_emit_failure(app.handle(), &reg, "ghost");
        assert!(
            !reg.lock()
                .unwrap()
                .broadcast_targets()
                .contains(&"ghost".to_string()),
            "emit failure on a missing window must still evict it"
        );
        let _ = w1; // live-window guard above keeps it; nothing else needed
    }

    #[test]
    fn stderr_targets_only_intersects_the_allow_list() {
        let mut reg = WindowRegistry::default();
        reg.subscribe("main");
        reg.subscribe("win-b");
        reg.subscribe("console");
        // Default: main only — win-b and console are subscribed but get nothing.
        assert_eq!(
            reg.stderr_targets(&StderrDelivery::default()),
            vec!["main".to_string()]
        );
        // Explicit list: intersection with subscribers — "console" is
        // allowed and subscribed, "logs" is allowed but never invoked a
        // command, "win-b" is subscribed but not allowed.
        let d = StderrDelivery::Only(vec!["main".into(), "console".into(), "logs".into()]);
        let mut got = reg.stderr_targets(&d);
        got.sort();
        assert_eq!(got, vec!["console".to_string(), "main".to_string()]);
        // All: every subscriber, exactly the old broadcast.
        let mut got = reg.stderr_targets(&StderrDelivery::All);
        got.sort();
        assert_eq!(
            got,
            vec![
                "console".to_string(),
                "main".to_string(),
                "win-b".to_string()
            ]
        );
    }

    #[test]
    fn stream_ack_carries_task_and_bridge_protocol() {
        // A new client reads this object to detect an old plugin (which
        // returned a bare task id) — the exact shape is the contract.
        let v = serde_json::to_value(StreamAck {
            task: 7,
            protocol: BRIDGE_PROTOCOL,
        })
        .unwrap();
        assert_eq!(v, json!({"task": 7, "protocol": 2}));
    }

    #[test]
    fn release_spec_forbids_bytecode_writes() {
        // rc.15 H: the bundle ships precompiled pycs and the install dir
        // is often read-only — the runtime must never write bytecode.
        // Isolated workers and app subprocesses inherit os.environ, so
        // this one var covers the whole tree.
        let spec = release_spawn_spec(Path::new("/res"), "backend.app", "backend");
        assert!(
            spec.env
                .contains(&("PYTHONDONTWRITEBYTECODE".into(), "1".into())),
            "release spawn spec must set PYTHONDONTWRITEBYTECODE=1"
        );
        assert!(
            !spec
                .env_remove
                .iter()
                .any(|k| k == "PYTHONDONTWRITEBYTECODE"),
            "the var must survive into the child env"
        );
    }

    #[test]
    fn bundled_resource_dir_prefers_resource_dir_then_falls_back() {
        // resource_dir() success wins outright.
        let dir = PathBuf::from(if cfg!(windows) { "C:/res" } else { "/res" });
        assert_eq!(bundled_resource_dir(Ok(dir.clone())).unwrap(), dir);
        // resource_dir() failure falls back to exe-relative resources —
        // with no bundle next to the test binary that also fails, and the
        // error must name the cause and the fix, not panic.
        let err = bundled_resource_dir(Err(tauri::Error::UnknownPath)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("resource_dir()"),
            "error lost the cause: {msg}"
        );
        assert!(
            msg.contains("symlinked path"),
            "error not actionable: {msg}"
        );
    }

    #[test]
    fn resources_near_exe_covers_each_bundle_layout() {
        let tmp = std::env::temp_dir().join(format!("pylair-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // macOS .app: <exe>/../Resources/python — expectations are
        // canonicalized too because the resolver returns real paths
        // (on Windows that means a \\?\ prefix).
        let mac = tmp.join("Fixture.app/Contents");
        std::fs::create_dir_all(mac.join("Resources/python")).unwrap();
        std::fs::create_dir_all(mac.join("MacOS")).unwrap();
        let exe = mac.join("MacOS/fixture");
        assert_eq!(
            resources_near_exe(&exe).unwrap(),
            mac.join("Resources").canonicalize().unwrap()
        );
        // deb/AppImage: <exe>/../lib/<stem>/python
        let deb = tmp.join("debroot/usr");
        std::fs::create_dir_all(deb.join("lib/myapp/python")).unwrap();
        std::fs::create_dir_all(deb.join("bin")).unwrap();
        assert_eq!(
            resources_near_exe(&deb.join("bin/myapp")).unwrap(),
            deb.join("lib/myapp").canonicalize().unwrap()
        );
        // NSIS: <exe dir>/python
        let nsis = tmp.join("nsisroot");
        std::fs::create_dir_all(nsis.join("python")).unwrap();
        assert_eq!(
            resources_near_exe(&nsis.join("app.exe")).unwrap(),
            nsis.canonicalize().unwrap()
        );
        // No layout at all → None, never a guessed path.
        let bare = tmp.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(resources_near_exe(&bare.join("app")).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The rc.8 panic: launched through a symlinked path (`/tmp` →
    /// `/private/tmp` on macOS, or any symlinked ancestor dir), Tauri
    /// rejects the exe path — the plugin must still find the bundle.
    /// Spawns the test binary through a symlinked directory and asserts
    /// `resources_from_exe` resolves the CANONICAL bundle, proving the
    /// fallback works in the exact condition that broke resource_dir().
    #[cfg(unix)]
    #[test]
    fn launched_via_symlinked_dir_still_resolves_resources() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        const CHILD_ENV: &str = "PYLAIR_LINKED_EXE_CHILD";
        if std::env::var(CHILD_ENV).is_ok() {
            // Child: report what the exe-relative resolver sees here.
            let exe = std::env::current_exe().unwrap();
            let res = resources_from_exe();
            println!(
                "exe={}\nres={}",
                exe.display(),
                res.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "NONE".into())
            );
            return;
        }

        let tmp = std::env::temp_dir().join(format!("pylair-linked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let real = tmp.join("realdir/Fixture.app/Contents");
        std::fs::create_dir_all(real.join("Resources/python")).unwrap();
        std::fs::create_dir_all(real.join("MacOS")).unwrap();
        let real_exe = real.join("MacOS/pylair-test-bin");
        std::fs::copy(std::env::current_exe().unwrap(), &real_exe).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(real_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The launch path's ancestor is a symlink — the macOS /tmp case.
        let linkdir = tmp.join("linkdir");
        symlink(tmp.join("realdir"), &linkdir).unwrap();
        let linked_exe = linkdir.join("Fixture.app/Contents/MacOS/pylair-test-bin");

        // --exact needs the module-qualified name: this is a unit test
        // inside `mod tests`, so its full name carries the `tests::`
        // prefix (unlike the tests/*.rs files' top-level names).
        let out = Command::new(linked_exe)
            .args([
                "tests::launched_via_symlinked_dir_still_resolves_resources",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("spawn test binary through symlinked dir");
        assert!(
            out.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let res_line = stdout
            .lines()
            .find_map(|l| l.strip_prefix("res="))
            .unwrap_or_else(|| panic!("child reported no res= line; stdout: {stdout}"));
        let want = real.join("Resources").canonicalize().unwrap();
        assert_eq!(
            PathBuf::from(res_line).canonicalize().unwrap(),
            want,
            "resources resolved to a non-canonical or wrong path (exe line: {})",
            stdout.lines().next().unwrap_or("?")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
