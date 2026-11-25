//! Wrap mode, plugin side (docs/wrap-mode-brief.md section 3.3, W2).
//!
//! A wrap app shows an existing Python web UI in its window. This module is
//! everything that is the same for every wrap app, so the app's own `lib.rs`
//! is one line (`Builder::wrap()`):
//!
//! - reading the runtime config the CLI wrote (`pylair-wrap.json`) and
//!   turning it into the launcher's `SpawnSpec`;
//! - the window: the local *starting* page until the server is ready, then
//!   the server's url, then the local *restarting* page whenever the server
//!   is gone, so the user never sees a blank or dead page;
//! - the navigation policy (the app origin only; everything else goes to
//!   the system browser), downloads (Save As by default), and a Back /
//!   Reload / Home menu;
//! - an env-gated installed self-test that proves all of it on the real
//!   webview, including that the wrapped page has no Tauri IPC.
//!
//! The decisions are pure functions in [`policy`] and unit-tested on every
//! OS; the glue here needs a webview and is proven by the installed
//! self-test (`PYLAIR_SELF_TEST_WRAP`).

mod downloads;
pub mod policy;
mod selftest;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tauri::{Emitter, Manager, Runtime};

pub use policy::{DownloadMode, LocalOrigin, NavDecision, Showing, UiAction};

use crate::supervisor::{Handle, RuntimeEvent};
use crate::util::state_payload;
use crate::{events, SpawnSpec, SpawnSpecError};

/// The window a wrap app's UI lives in.
pub const WINDOW: &str = "main";
/// File name of the runtime config in the resources dir.
pub const RUNTIME_FILE: &str = "pylair-wrap.json";
/// Lines of server output kept for a late-loading local page.
const LOG_KEEP: usize = 200;

/// What the CLI wrote for the plugin (`packages/cli/src/wrapconfig.ts`,
/// `WrapRuntime`). `launcher` is the launcher command line minus the
/// interpreter; `source` is the staged source dir the server runs in, a
/// path relative to the resources dir, or absolute under `pylair dev`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WrapRuntime {
    pub version: u32,
    pub source: String,
    pub launcher: Vec<String>,
    #[serde(rename = "readyTimeoutSecs")]
    pub ready_timeout_secs: u64,
    #[serde(default)]
    pub downloads: DownloadMode,
}

impl WrapRuntime {
    /// Dev (`pylair dev`, debug builds only): the file `PYLAIR_WRAP_CONFIG`
    /// names. Packaged: `<resources>/pylair-wrap.json`.
    pub fn load<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<Self, SpawnSpecError> {
        let path = if cfg!(debug_assertions) && std::env::var_os("PYLAIR_WRAP_CONFIG").is_some() {
            PathBuf::from(std::env::var_os("PYLAIR_WRAP_CONFIG").expect("checked"))
        } else {
            crate::spec::bundled_resource_dir(app.path().resource_dir())?.join(RUNTIME_FILE)
        };
        Self::from_file(&path)
    }

    pub(crate) fn from_file(path: &Path) -> Result<Self, SpawnSpecError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            SpawnSpecError::WrapConfig(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::parse(&text)
            .map_err(|e| SpawnSpecError::WrapConfig(format!("{}: {e}", path.display())))
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let rt: WrapRuntime = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if rt.version != 1 {
            return Err(format!(
                "unsupported pylair-wrap.json version {} (this plugin reads 1): rebuild with a matching `pylair`",
                rt.version
            ));
        }
        if rt.launcher.is_empty() {
            return Err("launcher is empty".into());
        }
        Ok(rt)
    }

    /// The launcher's spawn spec. Release: the bundled interpreter with the
    /// same hygiene env as the bridge runtime, and the server running in
    /// the staged source dir. Dev: the project venv (`PYLAIR_PYTHON`).
    pub fn spawn_spec<R: Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
    ) -> Result<SpawnSpec, SpawnSpecError> {
        let sep = if cfg!(windows) { ";" } else { ":" };
        if Path::new(&self.source).is_absolute() {
            // `pylair dev`: debug builds only, like every dev override.
            if !cfg!(debug_assertions) {
                return Err(SpawnSpecError::WrapConfig(
                    "an absolute source dir is only valid under `pylair dev`".into(),
                ));
            }
            let program = std::env::var("PYLAIR_PYTHON").unwrap_or_else(|_| "python3".into());
            let dir = PathBuf::from(&self.source);
            let mut spec = SpawnSpec {
                program: program.into(),
                args: self.launcher.clone(),
                env: vec![
                    ("PYTHONPATH".into(), dir.to_string_lossy().into_owned()),
                    ("PYTHONUTF8".into(), "1".into()),
                ],
                cwd: Some(dir),
                ..Default::default()
            };
            crate::spec::set_app_data_dir_env(&mut spec, app.path().app_data_dir(), |k| {
                std::env::var(k).ok()
            });
            return Ok(spec);
        }
        let res = crate::spec::bundled_resource_dir(app.path().resource_dir())?;
        let mut spec = crate::release_spawn_spec(&res, "", &self.source);
        spec.args = self.launcher.clone();
        let dir = res.join(&self.source);
        // The server's own modules live in the source dir, which PYTHONSAFEPATH
        // keeps off sys.path, so name it explicitly next to site-packages.
        spec.env.retain(|(k, _)| k != "PYTHONPATH");
        spec.env.push((
            "PYTHONPATH".into(),
            format!(
                "{}{sep}{}",
                dir.display(),
                res.join("site-packages").display()
            ),
        ));
        spec.cwd = Some(dir);
        crate::spec::set_app_data_dir_env(&mut spec, app.path().app_data_dir(), |k| {
            std::env::var(k).ok()
        });
        Ok(spec)
    }
}

struct Inner {
    showing: Showing,
    shown_app_url: Option<String>,
    last_state: Option<Value>,
    log: VecDeque<String>,
}

/// The window half of wrap mode. One per app, managed as [`WrapState`].
pub struct Ui<R: Runtime> {
    app: tauri::AppHandle<R>,
    handle: Handle,
    local: LocalOrigin,
    downloads: DownloadMode,
    inner: Mutex<Inner>,
    /// How many times the restarting page was shown (the self-test counts it).
    restarting_shown: std::sync::atomic::AtomicU32,
    /// Navigations and new windows refused outright (the self-test counts them).
    denied: std::sync::atomic::AtomicU32,
    /// The last finished download: where it went and whether it worked.
    last_download: Mutex<Option<(PathBuf, bool)>>,
    /// Why the server last went down (the Restarting detail, e.g. "exited
    /// exit status: 4"): the self-test checks a killed server is a crash.
    restart_reason: Mutex<Option<String>>,
    /// Origin of the wrapped page whose load last finished (the self-test
    /// waits for it: script run on a page that is still being replaced is lost).
    app_loaded: Mutex<Option<String>>,
    /// When the Ui came up — nav-event timestamps are relative to it.
    created: std::time::Instant,
    /// Bounded ring of navigation decisions the host made, newest last —
    /// cold-path diagnostic (the wrap self-test dumps it when the
    /// navigation-lock probe fails, and it answers "did the webview even
    /// ask us?" for real apps too). 64 entries is minutes of user-rate
    /// navigation.
    nav_events: Mutex<std::collections::VecDeque<NavEvent>>,
}

/// One navigation decision the host made, for the diagnostic ring.
pub struct NavEvent {
    /// Milliseconds since the Ui was created.
    pub at_ms: u64,
    /// `navigation` (in-place) or `new-window`.
    pub kind: &'static str,
    pub url: String,
    /// `allow` | `external` | `deny` — and for `navigation` events on
    /// windows other than the wrap window, `allow (other window)`.
    pub decision: &'static str,
}

/// Managed state: lets plugin hooks (navigation, page load) find the [`Ui`].
pub struct WrapState<R: Runtime>(pub Arc<Ui<R>>);

impl<R: Runtime> Ui<R> {
    pub(crate) fn new(app: &tauri::AppHandle<R>, handle: Handle, rt: &WrapRuntime) -> Arc<Self> {
        let dev = if cfg!(debug_assertions) {
            app.config().build.dev_url.clone()
        } else {
            None
        };
        Arc::new(Self {
            app: app.clone(),
            handle,
            local: LocalOrigin::new(dev.as_ref(), false),
            downloads: rt.downloads,
            restarting_shown: std::sync::atomic::AtomicU32::new(0),
            denied: std::sync::atomic::AtomicU32::new(0),
            last_download: Mutex::new(None),
            app_loaded: Mutex::new(None),
            restart_reason: Mutex::new(None),
            created: std::time::Instant::now(),
            nav_events: Mutex::new(std::collections::VecDeque::new()),
            inner: Mutex::new(Inner {
                showing: Showing::Local,
                shown_app_url: None,
                last_state: None,
                log: VecDeque::new(),
            }),
        })
    }

    fn app_url(&self) -> Option<tauri::Url> {
        self.handle
            .ready_url()
            .and_then(|u| tauri::Url::parse(&u).ok())
    }

    /// Plugin navigation hook: true lets the webview load `url`.
    pub(crate) fn navigation_allowed(&self, label: &str, url: &tauri::Url) -> bool {
        if label != WINDOW {
            self.note_nav("navigation", url, "allow (other window)");
            return true;
        }
        match policy::decide_navigation(url, self.app_url().as_ref(), &self.local) {
            NavDecision::Allow => {
                self.note_nav("navigation", url, "allow");
                true
            }
            NavDecision::External => {
                self.note_nav("navigation", url, "external");
                open_external(url);
                false
            }
            NavDecision::Deny => {
                self.note_nav("navigation", url, "deny");
                self.denied
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                false
            }
        }
    }

    fn note_nav(&self, kind: &'static str, url: &tauri::Url, decision: &'static str) {
        let mut g = self.nav_events.lock().unwrap();
        if g.len() >= 64 {
            g.pop_front();
        }
        g.push_back(NavEvent {
            at_ms: self.created.elapsed().as_millis() as u64,
            kind,
            url: url.to_string(),
            decision,
        });
    }

    /// The diagnostic dump the wrap self-test appends when the
    /// navigation-lock probe fails: every navigation decision the host
    /// saw, the denial count, whether the page ever loaded. Answers the
    /// key question directly: did the webview even ASK, or is the lock
    /// itself broken.
    pub(crate) fn nav_diagnostics(&self, denied_seen: u32) -> String {
        let mut out = format!(
            "denied total {} (the probe needed +2), page loaded: {:?}
  nav events:",
            self.denied.load(std::sync::atomic::Ordering::SeqCst),
            self.app_loaded.lock().unwrap(),
        );
        let _ = denied_seen;
        let g = self.nav_events.lock().unwrap();
        if g.is_empty() {
            out.push_str(" NONE — the webview never asked the host");
        }
        for e in g.iter() {
            out.push_str(&format!(
                "
    +{}ms {} {} -> {}",
                e.at_ms, e.kind, e.url, e.decision
            ));
        }
        out
    }

    /// `target=_blank` / `window.open`: never a second window.
    pub(crate) fn new_window(&self, url: &tauri::Url) {
        match policy::decide_new_window(url, self.app_url().as_ref(), &self.local) {
            NavDecision::Allow => {
                self.note_nav("new-window", url, "allow");
                self.navigate(url.clone());
            }
            NavDecision::External => {
                self.note_nav("new-window", url, "external");
                open_external(url);
            }
            NavDecision::Deny => {
                self.note_nav("new-window", url, "deny");
                self.denied
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    fn navigate(&self, url: tauri::Url) {
        if let Some(w) = self.app.get_webview_window(WINDOW) {
            let _ = w.navigate(url);
        }
    }

    /// The pid of the server the launcher started most recently, from its
    /// own log line (`started pid N on 127.0.0.1:PORT: ...`).
    pub(crate) fn server_pid(&self) -> Option<u32> {
        let g = self.inner.lock().unwrap();
        g.log.iter().rev().find_map(|l| {
            let rest = l.split("started pid ").nth(1)?;
            rest.split_whitespace().next()?.parse().ok()
        })
    }

    /// A wrapped page finished loading.
    pub(crate) fn app_page_finished(&self, url: &tauri::Url) {
        *self.app_loaded.lock().unwrap() = Some(url.origin().ascii_serialization());
    }

    pub(crate) fn is_local(&self, url: &tauri::Url) -> bool {
        self.local.contains(url)
    }

    /// Page loads are the source of truth for what the window shows (the
    /// user can also reach a page with the menu).
    pub(crate) fn page_started(&self, url: &tauri::Url) {
        let local = self.local.contains(url);
        let mut g = self.inner.lock().unwrap();
        g.showing = if local { Showing::Local } else { Showing::App };
        if !local {
            g.shown_app_url = Some(url.origin().ascii_serialization());
        }
    }

    /// A local page finished loading: give it what it missed (the state and
    /// the server's log so far), and if the server became ready while it was
    /// loading (or the user pressed Back to it), go on to the app.
    pub(crate) fn local_page_loaded(&self) {
        let (state, log) = {
            let g = self.inner.lock().unwrap();
            (
                g.last_state.clone(),
                g.log.iter().cloned().collect::<Vec<_>>(),
            )
        };
        if let Some(s) = state {
            let _ = self.app.emit_to(WINDOW, events::STATE, s);
        }
        for line in log {
            let _ = self.app.emit_to(WINDOW, events::STDERR, line);
        }
        if let Some(url) = self.handle.ready_url() {
            self.apply(UiAction::ShowApp(url));
        }
    }

    /// Whether runtime events may be delivered to `label` right now. A
    /// wrapped page is remote content: it is never sent the server's log or
    /// the lifecycle, only the local pages are.
    pub(crate) fn may_emit_to(&self, label: &str) -> bool {
        label != WINDOW || self.inner.lock().unwrap().showing == Showing::Local
    }

    /// Called for every runtime event, in order, before delivery.
    pub(crate) fn on_event(&self, ev: &RuntimeEvent) {
        match ev {
            RuntimeEvent::Stderr(line) => {
                let mut g = self.inner.lock().unwrap();
                if g.log.len() == LOG_KEEP {
                    g.log.pop_front();
                }
                g.log.push_back(line.clone());
            }
            RuntimeEvent::StateChanged { from, to, detail } => {
                let url = self.handle.ready_url();
                let payload = state_payload(*from, *to, detail, url.as_deref());
                if *to == crate::State::Restarting {
                    *self.restart_reason.lock().unwrap() = Some(detail.clone());
                }
                let action = {
                    let mut g = self.inner.lock().unwrap();
                    g.last_state = Some(payload);
                    policy::ui_action(*to, url.as_deref(), g.showing, g.shown_app_url.as_deref())
                };
                self.apply(action);
            }
            RuntimeEvent::Exposes(_) => {}
            RuntimeEvent::HealthChanged(_) => {}
            RuntimeEvent::AppChanged(_) => {}
        }
    }

    fn apply(&self, action: UiAction) {
        match action {
            UiAction::ShowApp(url) => {
                if let Ok(u) = tauri::Url::parse(&url) {
                    {
                        let mut g = self.inner.lock().unwrap();
                        g.showing = Showing::App;
                        g.shown_app_url = Some(u.origin().ascii_serialization());
                    }
                    self.navigate(u);
                }
            }
            UiAction::ShowRestarting => {
                self.restarting_shown
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.lock().unwrap().showing = Showing::Local;
                self.navigate(self.local.page("restarting.html"));
            }
            UiAction::Nothing => {}
        }
    }

    fn menu_action(&self, id: &str) {
        let Some(w) = self.app.get_webview_window(WINDOW) else {
            return;
        };
        match id {
            "pylair:back" => {
                let _ = w.eval("history.back()");
            }
            "pylair:reload" => {
                let _ = w.eval("location.reload()");
            }
            "pylair:home" => match self.handle.ready_url() {
                Some(url) => self.apply(UiAction::ShowApp(url)),
                None => self.navigate(self.local.page("index.html")),
            },
            _ => {}
        }
    }
}

/// Open `url` in the system browser. Only `http`, `https` and `mailto` ever
/// get here (see [`policy::decide_navigation`]); the url is passed as one
/// argument, never through a shell.
pub(crate) fn open_external(url: &tauri::Url) {
    use std::process::Command;
    let u = url.as_str();
    #[cfg(windows)]
    let res = {
        use std::os::windows::process::CommandExt;
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", u])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
    };
    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(u).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let res = Command::new("xdg-open").arg(u).spawn();
    if let Err(e) = res {
        eprintln!("pylair: could not open {u} in the system browser: {e}");
    }
}

/// Plugin `setup` half of the install: the local pages get lifecycle and log
/// events without having to invoke a command first.
pub(crate) fn install_setup<R: Runtime>(app: &tauri::AppHandle<R>) {
    app.state::<crate::Registry>()
        .lock()
        .unwrap()
        .subscribe(WINDOW);
}

/// Build the window (with the download and new-window hooks only a window
/// builder can carry) and the Back / Reload / Home menu.
///
/// Must run from `RunEvent::Ready`, off the main thread: plugin `setup` runs
/// before the event loop starts, and creating a window there blocks on an
/// acknowledgement from the loop that is not running yet (the app hangs
/// before showing anything). The plugin's `on_event` spawns this.
pub(crate) fn install_window<R: Runtime>(
    app: &tauri::AppHandle<R>,
    ui: &Arc<Ui<R>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == WINDOW)
        .cloned();
    match config {
        Some(cfg) if !cfg.create => {
            let on_dl = ui.clone();
            let on_win = ui.clone();
            tauri::WebviewWindowBuilder::from_config(app, &wrap_window_config(cfg))?
                .on_download(move |webview, event| downloads::handle(&on_dl, webview, event))
                .on_new_window(move |url, _features| {
                    on_win.new_window(&url);
                    tauri::webview::NewWindowResponse::Deny
                })
                .build()?;
        }
        _ => eprintln!(
            "pylair: window \"{WINDOW}\" is created by Tauri, so the download and \
             new-window hooks are not installed. Set \"create\": false on it in \
             tauri.conf.json (the wrap template does)."
        ),
    }

    let menu = tauri::menu::Menu::default(app)?;
    let accel = |mac: &'static str, other: &'static str| {
        if cfg!(target_os = "macos") {
            mac
        } else {
            other
        }
    };
    let nav = tauri::menu::SubmenuBuilder::new(app, "Navigate")
        .item(
            &tauri::menu::MenuItemBuilder::with_id("pylair:back", "Back")
                .accelerator(accel("Cmd+[", "Alt+Left"))
                .build(app)?,
        )
        .item(
            &tauri::menu::MenuItemBuilder::with_id("pylair:reload", "Reload")
                .accelerator("CmdOrCtrl+R")
                .build(app)?,
        )
        .item(
            &tauri::menu::MenuItemBuilder::with_id("pylair:home", "Home")
                .accelerator(accel("Cmd+Shift+H", "Alt+Home"))
                .build(app)?,
        )
        .build()?;
    menu.append(&nav)?;
    app.set_menu(menu)?;
    let on_menu = ui.clone();
    app.on_menu_event(move |_app, event| on_menu.menu_action(event.id().as_ref()));

    if let Ok(marker) = std::env::var("PYLAIR_SELF_TEST_WRAP") {
        selftest::start(ui.clone(), marker);
    }
    Ok(())
}

/// The IPC probe the self-test injects into every page (see
/// [`selftest`]). Empty unless the self-test is requested, so a normal app
/// carries no extra script. It only acts on the app's own loopback origin.
pub(crate) fn ipc_probe_script() -> Option<String> {
    std::env::var_os("PYLAIR_SELF_TEST_WRAP").map(|_| selftest::PROBE_JS.to_string())
}

/// The `WindowConfig` a wrap window is built from: the app's own
/// `windows[*]` entry with the native drag-drop handler off. Tauri
/// installs a handler that turns file drops into `DragDropEvent`s emitted
/// over IPC; a wrapped page cannot use IPC, and on Windows the handler
/// replaces WebView2's so an HTML5 drop never reaches the page.
fn wrap_window_config(
    mut cfg: tauri::utils::config::WindowConfig,
) -> tauri::utils::config::WindowConfig {
    cfg.drag_drop_enabled = false;
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"version":1,"source":"app","launcher":["-m","pylair.wrap","--framework","command","--cmd","python","-m","x"],"readyTimeoutSecs":120}"#;

    #[test]
    fn the_runtime_file_the_cli_writes_parses() {
        let rt = WrapRuntime::parse(GOOD).unwrap();
        assert_eq!(rt.source, "app");
        assert_eq!(rt.ready_timeout_secs, 120);
        assert_eq!(
            rt.downloads,
            DownloadMode::Ask,
            "downloads defaults to a Save As dialog"
        );
        assert_eq!(rt.launcher[0], "-m");
        let rt =
            WrapRuntime::parse(&GOOD.replace("120}", "120,\"downloads\":\"downloads\"}")).unwrap();
        assert_eq!(rt.downloads, DownloadMode::Downloads);
    }

    #[test]
    fn the_wrap_window_disables_tauris_native_drag_drop_handler() {
        // Enabled by default; the handler only exists to emit
        // DragDropEvents over IPC a wrapped page cannot use, and on
        // Windows it replaces WebView2's so an HTML5 drop never reaches
        // the page.
        assert!(tauri::utils::config::WindowConfig::default().drag_drop_enabled);
        let cfg = wrap_window_config(tauri::utils::config::WindowConfig::default());
        assert!(
            !cfg.drag_drop_enabled,
            "wrap windows disable the native handler so HTML5 drag-and-drop reaches the page"
        );
    }

    #[test]
    fn a_runtime_file_the_plugin_cannot_run_is_refused_with_a_reason() {
        let v2 = WrapRuntime::parse(&GOOD.replace("\"version\":1", "\"version\":2")).unwrap_err();
        assert!(v2.contains("version 2") && v2.contains("rebuild"), "{v2}");
        let empty = WrapRuntime::parse(&GOOD.replace(
            "[\"-m\",\"pylair.wrap\",\"--framework\",\"command\",\"--cmd\",\"python\",\"-m\",\"x\"]",
            "[]",
        ))
        .unwrap_err();
        assert!(empty.contains("launcher is empty"), "{empty}");
        assert!(WrapRuntime::parse("{").is_err());
        assert!(WrapRuntime::parse("{\"version\":1}").is_err());
        assert!(WrapRuntime::parse(
            &GOOD
                .replace("\"downloads\"", "\"x\"")
                .replace("120}", "120,\"downloads\":\"never\"}")
        )
        .is_err());
    }
}
