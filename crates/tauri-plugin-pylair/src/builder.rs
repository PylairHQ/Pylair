use crate::commands;
use crate::registry::handle_emit_failure;
use crate::selftest::attach_parent_console;
use crate::util::state_payload;
use crate::wrap;
use crate::{
    events, spawn_supervisor, PyRuntime, Registry, RuntimeEvent, RuntimeKind, SpawnSpec,
    SpawnSpecError, SupervisorConfig,
};
use serde_json::json;
use tauri::{Emitter, Manager, Runtime};

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
    /// Wrap mode: the spec, kind, ready timeout and the whole window come
    /// from `pylair-wrap.json` at setup (see `wrap`).
    wrap: bool,
}

/// `plugins.pylair` in tauri.conf.json. Tauri deserializes that section
/// into the plugin's config type at `initialize` — registered as `()`,
/// ANY map there panics the app at startup ("invalid type: map,
/// expected unit"). Today it only carries the update watchdog knobs
/// (decision 0011 §2); every field is optional and unknown keys are
/// ignored. The plugin's config type is `Option<PluginConfig>`: the
/// section deserializes as `null` when absent, which a bare struct
/// rejects — `Option` accepts both shapes (None when absent, Some for
/// a real map).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginConfig {
    pub update: UpdateKnobsConf,
}

/// `plugins.pylair.update` — the watchdog knobs the gate copies into
/// pending.json (they're read from the raw conf because the watchdog
/// path runs before plugin init; this type exists so Tauri accepts the
/// map at all).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UpdateKnobsConf {
    pub heartbeat_ms: Option<u64>,
    pub max_boots: Option<u64>,
    pub hard_cap_ms: Option<u64>,
}

impl<R: Runtime> Builder<R> {
    pub fn new(spec: SpawnSpec) -> Self {
        Self {
            spec,
            lazy: None,
            config: SupervisorConfig::default(),
            stderr: StderrDelivery::default(),
            wrap: false,
        }
    }

    /// Wrap mode: show an existing Python web UI in the window. Everything
    /// else comes from the config `pylair dev` / `pylair build` wrote
    /// (`pylair-wrap.json`): the launcher command, the ready timeout and the
    /// download behaviour. The app's own window must be declared in
    /// `tauri.conf.json` with `"create": false`, so the plugin can build it
    /// with the navigation, download and new-window hooks. See `wrap`.
    pub fn wrap() -> Self {
        let mut b = Self::new(SpawnSpec::default());
        b.wrap = true;
        b.config.kind = RuntimeKind::Web;
        b
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
            wrap: false,
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

    /// What the supervised child is. The default is the bridge runtime
    /// (ready on `hello`); [`RuntimeKind::Web`] is wrap mode, where the
    /// child is `python -m pylair.wrap`, ready only on its `ready {url}`
    /// notification (protocol-v1 section 4.1). A web runtime's state events
    /// and `python_state` carry the current `url`.
    pub fn runtime_kind(mut self, kind: RuntimeKind) -> Self {
        self.config.kind = kind;
        self
    }

    /// `Web` kind only: how long to wait for `ready {url}` after `hello`.
    /// Default 120s, clamped to 1s..=1h at spawn (the clamp is logged on
    /// `pylair://stderr`). Independent of [`Builder::hello_timeout`].
    pub fn ready_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.config.ready_timeout = timeout;
        self
    }

    pub fn build(self) -> tauri::plugin::TauriPlugin<R, Option<PluginConfig>> {
        let mut plugin = tauri::plugin::Builder::<R, Option<PluginConfig>>::new("pylair");
        // Installed-webview self-test: the client sees this flag and runs its
        // checks through the real bridge, then calls python_selftest_report.
        if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_some() {
            // The verdict (and any panic) prints to stdout/stderr — a
            // GUI-subsystem exe needs the parent's console for it to land.
            attach_parent_console();
            plugin = plugin.js_init_script("window.__PYLAIR_SELF_TEST__ = true;".to_string());
        }
        // Latency bench (docs/performance.md): same mechanism, opt-in
        // only. The client runs the timing harness instead of the checks
        // and reports through python_bench_report. No second window —
        // the bench measures a single-window steady state.
        if std::env::var_os("PYLAIR_SELF_TEST_BENCH").is_some() {
            attach_parent_console();
            plugin = plugin.js_init_script(
                "window.__PYLAIR_SELF_TEST__ = true; window.__PYLAIR_SELF_TEST_BENCH__ = true;"
                    .to_string(),
            );
        }
        // Wrap mode only: the IPC probe the installed self-test injects into
        // every page, and the navigation lock for the app's window.
        if self.wrap {
            if let Some(js) = wrap::ipc_probe_script() {
                plugin = plugin.js_init_script(js);
            }
            plugin = plugin.on_navigation(|webview, url| {
                webview
                    .try_state::<wrap::WrapState<R>>()
                    .map(|s| s.0.navigation_allowed(webview.label(), url))
                    .unwrap_or(true)
            });
        }
        // Binary blobs (docs/decisions/0010): registered blobs are served
        // to their owning webview only — the handler resolves the
        // requesting label and the app origins it may claim per request.
        plugin = plugin.register_asynchronous_uri_scheme_protocol(
            crate::blobs::SCHEME,
            |ctx, req, responder| {
                let app = ctx.app_handle().clone();
                let label = ctx.webview_label().to_string();
                std::thread::spawn(move || {
                    responder.respond(crate::blobs::serve_request(&app, &label, &req));
                });
            },
        );
        // Dev-only inspector (docs/inspector.md): the scheme serves the
        // bundled page and the snapshot, both gated to the inspector
        // window's label. Compiled out of release builds entirely.
        #[cfg(debug_assertions)]
        if std::env::var_os("PYLAIR_INSPECT").is_some_and(|v| v == "1") {
            plugin = plugin.register_asynchronous_uri_scheme_protocol(
                crate::inspect::SCHEME,
                |ctx, req, responder| {
                    let app = ctx.app_handle().clone();
                    let label = ctx.webview_label().to_string();
                    std::thread::spawn(move || {
                        let path = req.uri().path().to_string();
                        let (status, mime, body) = crate::inspect::serve(&app, &label, &path);
                        responder.respond(
                            tauri::http::Response::builder()
                                .status(status)
                                .header("content-type", mime)
                                .body(body)
                                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new())),
                        );
                    });
                },
            );
        }
        plugin
            .setup(move |app, _api| {
                let wrap_rt = if self.wrap {
                    Some(wrap::WrapRuntime::load(app)?)
                } else {
                    None
                };
                let mut config = self.config.clone();
                // Blob root (decision 0010): `<cache>/pylair-blobs/gen-N`
                // per generation, exported as PYLAIR_BLOB_DIR. Same
                // resolve/mkdir rule as PYLAIR_APP_DATA_DIR — a failure
                // leaves the capability off and logs, never crashes.
                if config.blob_root.is_none() {
                    config.blob_root =
                        crate::blobs::BlobRegistry::resolve_root(app.path().app_cache_dir());
                }
                let spec = match (&wrap_rt, self.lazy.as_ref()) {
                    (Some(rt), _) => {
                        config.ready_timeout =
                            std::time::Duration::from_secs(rt.ready_timeout_secs);
                        rt.spawn_spec(app)?
                    }
                    (None, Some(f)) => f(app)?,
                    (None, None) => self.spec.clone(),
                };
                let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<RuntimeEvent>(256);
                // setup runs on the main thread with no reactor entered —
                // spawn the actor via tauri's runtime, not bare tokio::spawn.
                let (handle, actor) = spawn_supervisor(spec, config, ev_tx);
                tauri::async_runtime::spawn(actor);
                let url_source = handle.clone();
                // The fairness map lives in the supervisor; the window
                // registry maintains the live set it consults
                // (decisions 0005).
                let reg = Registry::new(Default::default());
                reg.lock().unwrap().set_fairness(handle.fairness());
                app.manage(PyRuntime(handle));
                app.manage(reg);
                // Update gate (decision 0011): only exists for apps
                // carrying plugins.updater — writes the file contract
                // the watchdog reads (boot marker happens inside).
                if let Some(gate) = crate::update::UpdateGate::new(app) {
                    app.manage(gate);
                }
                let wrap_ui = match &wrap_rt {
                    Some(rt) => {
                        let ui = wrap::Ui::new(app, url_source.clone(), rt);
                        app.manage(wrap::WrapState(ui.clone()));
                        wrap::install_setup(app);
                        Some(ui)
                    }
                    None => None,
                };

                let app_handle = app.clone();
                let stderr = self.stderr.clone();
                let update_gate = app
                    .try_state::<std::sync::Arc<crate::update::UpdateGate>>()
                    .map(|s| (*s).clone());
                tauri::async_runtime::spawn(async move {
                    while let Some(ev) = ev_rx.recv().await {
                        // Wrap mode: record the event and drive the window
                        // BEFORE delivery, so a local page that loads next
                        // can be given what it missed.
                        if let Some(ui) = &wrap_ui {
                            ui.on_event(&ev);
                        }
                        // Update gate: readiness/boot evidence writes —
                        // before fan-out, on every generation event.
                        if let Some(g) = &update_gate {
                            g.on_event(&ev);
                        }
                        let payload = match ev {
                            // A web runtime's url is read at emit time, so a lagging
                            // consumer attaches the CURRENT url, never a stale one.
                            RuntimeEvent::StateChanged { from, to, detail } => Some((
                                events::STATE,
                                state_payload(from, to, &detail, url_source.ready_url().as_deref()),
                            )),
                            RuntimeEvent::Stderr(line) => Some((events::STDERR, json!(line))),
                            RuntimeEvent::Exposes(x) => Some((events::EXPOSES, x)),
                            RuntimeEvent::HealthChanged(x) => Some((events::HEALTH, x)),
                            RuntimeEvent::AppChanged(x) => Some((events::APP, x)),
                        };
                        let Some((event, payload)) = payload else {
                            continue;
                        };
                        // Runtime-wide events go to subscribed webviews only.
                        // Stderr is narrower: the app's allow-list.
                        let reg = app_handle.state::<Registry>();
                        let labels = match event {
                            events::STDERR => reg.lock().unwrap().stderr_targets(&stderr),
                            _ => reg.lock().unwrap().broadcast_targets(),
                        };
                        for label in labels {
                            // A wrapped page is remote content: it gets no
                            // lifecycle or log events, only local pages do.
                            if wrap_ui.as_ref().is_some_and(|ui| !ui.may_emit_to(&label)) {
                                continue;
                            }
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
                // Wrap mode: the window is built here, once the loop is running
                // (see wrap::install_window), never in `setup`.
                if let tauri::RunEvent::Ready = event {
                    // Dev-only inspector window (docs/inspector.md) —
                    // debug builds + PYLAIR_INSPECT=1 only. Off the main
                    // thread, same as win-b above: create_window
                    // dispatches to the loop and blocks on the ack.
                    #[cfg(debug_assertions)]
                    if std::env::var_os("PYLAIR_INSPECT").is_some_and(|v| v == "1") {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            // Same platform split as the blob scheme
                            // (decision 0010): Windows/Android serve
                            // custom schemes at http://<scheme>.localhost.
                            let url = if cfg!(windows) || cfg!(target_os = "android") {
                                format!("http://{}.localhost/", crate::inspect::SCHEME)
                            } else {
                                format!("{}://localhost/", crate::inspect::SCHEME)
                            };
                            if let Err(e) = tauri::WebviewWindowBuilder::new(
                                &app,
                                crate::inspect::WINDOW_LABEL,
                                tauri::WebviewUrl::External(
                                    url.parse().expect("inspector url parses"),
                                ),
                            )
                            .title("pylair inspector")
                            .inner_size(1100.0, 700.0)
                            .build()
                            {
                                eprintln!("pylair: inspector window failed: {e}");
                            }
                        });
                    }
                    if let Some(st) = app.try_state::<wrap::WrapState<R>>() {
                        let (app, ui) = (app.clone(), st.0.clone());
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = wrap::install_window(&app, &ui) {
                                eprintln!("pylair: wrap mode could not build the window: {e}");
                                app.exit(1);
                            }
                        });
                    }
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
                    // Its blob handles die too (decision 0010 §6: window
                    // close frees the window's live blobs).
                    // Window events run on the main thread with no reactor
                    // entered — spawn the send on Tauri's runtime, exactly
                    // like the actor/Exit/on_drop paths below.
                    if let Some(rt) = app.try_state::<PyRuntime>() {
                        let h = rt.0.clone();
                        let owner = label.to_string();
                        crate::blobs::lock_blobs(&h.blobs()).drop_owner(&owner);
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
                if let Some(st) = webview.try_state::<wrap::WrapState<R>>() {
                    if webview.label() == wrap::WINDOW {
                        match payload.event() {
                            tauri::webview::PageLoadEvent::Started => {
                                st.0.page_started(payload.url())
                            }
                            tauri::webview::PageLoadEvent::Finished
                                if st.0.is_local(payload.url()) =>
                            {
                                st.0.local_page_loaded()
                            }
                            tauri::webview::PageLoadEvent::Finished => {
                                st.0.app_page_finished(payload.url())
                            }
                        }
                    }
                }
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
    use super::*;

    // plugins.pylair deserializes as `null` when the section is absent
    // and as a map when present -- the plugin's config type is
    // Option<PluginConfig> for exactly that reason (a bare struct
    // panicked every non-updater app at init).
    #[test]
    fn plugin_config_accepts_absent_and_present_sections() {
        assert!(
            serde_json::from_value::<Option<PluginConfig>>(serde_json::Value::Null)
                .unwrap()
                .is_none()
        );
        let conf = serde_json::from_value::<Option<PluginConfig>>(serde_json::json!({}))
            .unwrap()
            .expect("an empty map is a present section");
        assert_eq!(conf.update.heartbeat_ms, None);
        let conf = serde_json::from_value::<Option<PluginConfig>>(serde_json::json!({
            "update": {"heartbeatMs": 4000, "maxBoots": 5, "unknownKey": true}
        }))
        .unwrap()
        .expect("a knobs map deserializes");
        assert_eq!(conf.update.heartbeat_ms, Some(4000));
        assert_eq!(conf.update.max_boots, Some(5));
        assert_eq!(conf.update.hard_cap_ms, None);
    }
}
