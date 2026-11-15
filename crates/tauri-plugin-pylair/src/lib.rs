//! Tauri plugin: supervisor, IPC bridge and permission enforcement for the
//! managed Python runtime.
//!
//! `protocol` / `transport` / `supervisor` are Tauri-free and testable in
//! isolation. The plugin glue owns the supervisor `Handle`, forwards
//! lifecycle events to the frontend, and exposes the `python_*` commands.

pub mod protocol;
pub mod supervisor;
pub mod transport;

mod commands;
mod registry;
mod selftest;
mod spec;
mod util;

pub use commands::{forward_stream, python_call_stream_impl, StreamAck};
pub use protocol::ErrorObj;
pub use registry::{PyRuntime, Registry, WindowRegistry};
pub use selftest::{self_test_gui, self_test_headless, self_test_requested};
pub use spec::{app_spawn_spec, release_spawn_spec, SpawnSpecError};
pub use supervisor::{
    spawn as spawn_supervisor, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
pub use util::same_origin;

use registry::handle_emit_failure;
use selftest::attach_parent_console;
use serde_json::json;
use tauri::{Emitter, Manager, Runtime};
use util::fmt_state;

/// Frontend event names (kept in one place — the client SDK mirrors them).
pub mod events {
    /// `{from, to, detail}` lifecycle transitions.
    pub const STATE: &str = "pylair://state";
    /// One line of runtime stderr.
    pub const STDERR: &str = "pylair://stderr";
    /// The exposes manifest after handshake.
    pub const EXPOSES: &str = "pylair://exposes";
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
            .invoke_handler(commands::invoke_handler())
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::commands::*;
    use super::spec::*;
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

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
