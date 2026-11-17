//! Wrap-mode window policy, as pure functions (docs/security.md section 9).
//!
//! Everything here decides; nothing here touches a window. The window glue
//! in `mod.rs` and `downloads.rs` calls these, so the rules are unit-tested
//! on every OS without a webview.
//!
//! The rule for navigation is the one from `docs/decisions/0003-wrap-mode.md`:
//! the window may only navigate within the app origin (the exact loopback
//! origin of the current `ready` url, compared with [`same_origin`]) or to
//! the app's own local pages. Every other `http`, `https` or `mailto` link
//! opens in the system browser; every other scheme goes nowhere.

use std::path::{Path, PathBuf};

use crate::util::same_origin;

/// What to do with a navigation or new-window request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavDecision {
    /// Load it in the window.
    Allow,
    /// Do not load it here; open it in the system browser.
    External,
    /// Do not load it anywhere (a scheme we never hand to the OS).
    Deny,
}

/// The origin the app's own local pages (starting, restarting) are served
/// from: `tauri://localhost` (macOS, Linux), `http(s)://tauri.localhost`
/// (Windows, Android), or the dev server's origin under `tauri dev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOrigin(tauri::Url);

impl LocalOrigin {
    pub fn new(dev_url: Option<&tauri::Url>, https: bool) -> Self {
        let base = match dev_url {
            Some(u) => u.clone(),
            None if cfg!(any(windows, target_os = "android")) => {
                let scheme = if https { "https" } else { "http" };
                tauri::Url::parse(&format!("{scheme}://tauri.localhost/")).expect("static url")
            }
            None => tauri::Url::parse("tauri://localhost/").expect("static url"),
        };
        Self(base)
    }

    /// `page` is the file name of a local page ("restarting.html").
    pub fn page(&self, page: &str) -> tauri::Url {
        self.0.join(page).expect("a relative page name joins")
    }

    /// The local origin rooted at `base` (tests; `new` is the real one).
    pub fn from_base(base: tauri::Url) -> Self {
        Self(base)
    }

    /// Is `url` one of the app's own local pages?
    ///
    /// Compared as scheme, host and port rather than with [`same_origin`]:
    /// `tauri://localhost` is a custom scheme, whose origin is opaque, and
    /// two opaque origins are never equal, so `same_origin` would call the
    /// app's own page foreign on macOS and Linux (and send it to the
    /// browser). `http://tauri.localhost` and a dev server are ordinary
    /// origins, for which this is the same rule.
    pub fn contains(&self, url: &tauri::Url) -> bool {
        url.scheme() == self.0.scheme()
            && url.host_str() == self.0.host_str()
            && url.port_or_known_default() == self.0.port_or_known_default()
            && url.username().is_empty()
    }
}

/// Decide a top-level navigation to `target`.
///
/// `app_url` is the current validated `ready` url (`None` while no server
/// is up). The local pages are always allowed; the app origin only once
/// there is one.
pub fn decide_navigation(
    target: &tauri::Url,
    app_url: Option<&tauri::Url>,
    local: &LocalOrigin,
) -> NavDecision {
    if local.contains(target) {
        return NavDecision::Allow;
    }
    // Same-document and in-page things the page produces itself.
    if target.as_str() == "about:blank" {
        return NavDecision::Allow;
    }
    if let Some(app) = app_url {
        // `blob:` urls carry their creator's origin, which `Url::origin`
        // resolves; a blob made by the app page is the app's own.
        if same_origin(target.as_str(), app.as_str()) {
            return NavDecision::Allow;
        }
    }
    match target.scheme() {
        "http" | "https" | "mailto" => NavDecision::External,
        _ => NavDecision::Deny,
    }
}

/// Decide a `target=_blank` / `window.open` request. The page never gets a
/// second window: the app's own origin is shown in place, anything else
/// goes to the system browser or nowhere.
pub fn decide_new_window(
    target: &tauri::Url,
    app_url: Option<&tauri::Url>,
    local: &LocalOrigin,
) -> NavDecision {
    decide_navigation(target, app_url, local)
}

/// How a download from the wrapped page is saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DownloadMode {
    /// A native Save As dialog (default): a silent save with no feedback is
    /// the worst option (Streamlit's `download_button` showed it).
    #[default]
    Ask,
    /// Straight into the user's Downloads folder, with feedback after.
    Downloads,
}

/// A file name that is safe to join onto a directory: the last path
/// component of what the server suggested, with anything that would escape
/// the directory or confuse the OS removed. Never empty.
pub fn safe_file_name(suggested: &str) -> String {
    let last = suggested
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('.');
    let cleaned: String = last
        .chars()
        .map(|c| match c {
            c if c.is_control() => '_',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

/// `name`, `name (1)`, `name (2)`... the first one `exists` says is free,
/// so saving to Downloads never overwrites an earlier file.
pub fn unique_in(dir: &Path, name: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !exists(&first) {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !exists(p))
        .expect("an unbounded range always ends")
}

/// The file name a download's destination suggests (`destination` holds the
/// webview's proposed path; only its last component is trusted).
pub fn suggested_name(destination: &Path) -> String {
    safe_file_name(
        &destination
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

/// The text shown after a download finished.
pub fn download_feedback(path: &Path, success: bool) -> String {
    if success {
        format!("Saved to {}", path.display())
    } else {
        "Download failed".to_string()
    }
}

/// Which webview URL is showing, as far as the window glue needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Showing {
    /// One of the app's own local pages.
    Local,
    /// The wrapped web app.
    App,
}

/// What the window glue should do when the runtime changes state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    /// Navigate to the app (the `ready` url).
    ShowApp(String),
    /// Navigate to the local restarting page.
    ShowRestarting,
    /// Nothing to do (already showing the right thing, or a diagnostic).
    Nothing,
}

/// State to UI. The starting page is the window's initial page; from there
/// `ready` shows the app, and any state in which the app is gone or on its
/// way back shows the restarting page (never a dead or blank page).
pub fn ui_action(
    to: crate::State,
    ready_url: Option<&str>,
    showing: Showing,
    shown_app_url: Option<&str>,
) -> UiAction {
    use crate::State::*;
    match to {
        Ready => match ready_url {
            Some(u) if showing == Showing::Local || shown_app_url != Some(u) => {
                UiAction::ShowApp(u.to_string())
            }
            _ => UiAction::Nothing,
        },
        // Unresponsive is a diagnostic: the page may still recover, and a
        // navigation away would throw the user's state out for nothing.
        Unresponsive | Starting => UiAction::Nothing,
        Restarting | Failed | Stopped if showing == Showing::App => UiAction::ShowRestarting,
        _ => UiAction::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::State;

    fn u(s: &str) -> tauri::Url {
        tauri::Url::parse(s).unwrap()
    }
    fn local() -> LocalOrigin {
        LocalOrigin::new(None, false)
    }
    const APP: &str = "http://127.0.0.1:51234/";

    #[test]
    fn the_app_origin_is_allowed_everything_else_is_not() {
        let app = u(APP);
        for ok in [
            "http://127.0.0.1:51234/",
            "http://127.0.0.1:51234/page?q=1#frag",
            "http://127.0.0.1:51234/_stcore/stream",
            "about:blank",
        ] {
            assert_eq!(
                decide_navigation(&u(ok), Some(&app), &local()),
                NavDecision::Allow,
                "{ok}"
            );
        }
        for external in [
            "https://example.com/",
            "http://example.com:51234/",
            "http://localhost:51234/",  // a different origin than 127.0.0.1
            "http://127.0.0.1:51235/",  // another port
            "https://127.0.0.1:51234/", // another scheme
            "http://127.0.0.1@evil.com/", // userinfo trick
            "http://127.0.0.1:51234@evil.com/",
            "http://evil.com/?http://127.0.0.1:51234/",
            "http://[::1]:51234/",
            "mailto:someone@example.com",
        ] {
            assert_eq!(
                decide_navigation(&u(external), Some(&app), &local()),
                NavDecision::External,
                "{external}"
            );
        }
    }

    #[test]
    fn a_port_suffix_trick_is_not_even_a_url() {
        // "port" 51234.evil.com does not parse, so the webview can never hand
        // such a navigation to the policy; same_origin agrees it is foreign.
        assert!(tauri::Url::parse("http://127.0.0.1:51234.evil.com/").is_err());
        assert!(!same_origin(APP, "http://127.0.0.1:51234.evil.com/"));
    }

    #[test]
    fn schemes_the_os_must_never_be_handed_are_denied() {
        let app = u(APP);
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>1</script>",
            "ftp://example.com/",
            "tauri://localhost.evil.com/",
            "ms-msdt:/id",
        ] {
            assert_eq!(
                decide_navigation(&u(bad), Some(&app), &local()),
                NavDecision::Deny,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_carriage_return_cannot_change_the_origin_or_the_path() {
        let app = u(APP);
        // The WHATWG parser drops tab, CR and LF anywhere in the input, so a
        // trailing CR (the Windows stderr-scrape bug in the W0 spike) leaves
        // the same url...
        assert_eq!(u("http://127.0.0.1:51234\r/"), u(APP));
        assert_eq!(
            decide_navigation(&u("http://127.0.0.1:51234\r/"), Some(&app), &local()),
            NavDecision::Allow
        );
        // ...but a LITERAL backslash-r (what a JSON-escaped CR becomes if it
        // is not decoded) reads the backslash as a slash and navigates to
        // /r. Same origin, so the policy allows it; the url validator
        // (`validate_ready_url`) is what refuses to ever produce one.
        let slash_r = u("http://127.0.0.1:51234\\r");
        assert_eq!(slash_r.path(), "/r");
        // Anything that spoofs the host with a CR in the middle is a
        // different host once normalised, and goes to the browser.
        assert_eq!(
            decide_navigation(
                &u("http://127.0.0.1\r.evil.com:51234/"),
                Some(&app),
                &local()
            ),
            NavDecision::External
        );
    }

    #[test]
    fn local_pages_are_allowed_without_an_app_and_with_one() {
        let l = local();
        let restarting = l.page("restarting.html");
        assert!(l.contains(&restarting));
        assert_eq!(decide_navigation(&restarting, None, &l), NavDecision::Allow);
        assert_eq!(
            decide_navigation(&restarting, Some(&u(APP)), &l),
            NavDecision::Allow
        );
        // Before the server is ready the app origin is not known: a link to
        // a loopback port is just another external url.
        assert_eq!(decide_navigation(&u(APP), None, &l), NavDecision::External);
        // A look-alike of the local origin is not local.
        assert!(!l.contains(&u("http://tauri.localhost.evil.com/")));
        assert!(!l.contains(&u("tauri://localhost.evil.com/")));
    }

    #[test]
    fn the_custom_scheme_origin_of_macos_and_linux_is_local_too() {
        // tauri://localhost has an opaque origin: same_origin() says two of
        // them differ. The local rule must not (found by the Linux CI leg).
        let l = LocalOrigin::from_base(u("tauri://localhost/"));
        assert!(!same_origin("tauri://localhost/", "tauri://localhost/"));
        assert!(l.contains(&l.page("restarting.html")));
        assert!(l.contains(&u("tauri://localhost/index.html?x=1#y")));
        assert_eq!(
            l.page("restarting.html"),
            u("tauri://localhost/restarting.html")
        );
        for foreign in [
            "tauri://localhost.evil.com/",
            "tauri://evil/",
            "tauri://user@localhost/",
            "http://localhost/",
            "https://tauri.localhost/",
            "http://tauri.localhost/",
        ] {
            assert!(!l.contains(&u(foreign)), "{foreign}");
        }
        let app = u(APP);
        assert_eq!(
            decide_navigation(&l.page("restarting.html"), Some(&app), &l),
            NavDecision::Allow
        );
        assert_eq!(
            decide_navigation(&u("http://tauri.localhost/"), Some(&app), &l),
            NavDecision::External
        );
        // And the Windows shape.
        let w = LocalOrigin::from_base(u("http://tauri.localhost/"));
        assert!(w.contains(&u("http://tauri.localhost/restarting.html")));
        assert!(!w.contains(&u("tauri://localhost/")));
        assert!(!w.contains(&u("https://tauri.localhost/")));
    }

    #[test]
    fn the_dev_server_origin_is_the_local_origin_under_tauri_dev() {
        let dev = u("http://localhost:1420");
        let l = LocalOrigin::new(Some(&dev), false);
        assert!(l.contains(&u("http://localhost:1420/restarting.html")));
        assert_eq!(
            l.page("restarting.html"),
            u("http://localhost:1420/restarting.html")
        );
        assert!(!l.contains(&u("http://localhost:1421/")));
    }

    #[test]
    fn new_windows_follow_the_navigation_rule() {
        let app = u(APP);
        assert_eq!(
            decide_new_window(&u("http://127.0.0.1:51234/x"), Some(&app), &local()),
            NavDecision::Allow
        );
        assert_eq!(
            decide_new_window(&u("https://docs.example.com/"), Some(&app), &local()),
            NavDecision::External
        );
        assert_eq!(
            decide_new_window(&u("file:///C:/Windows/win.ini"), Some(&app), &local()),
            NavDecision::Deny
        );
    }

    #[test]
    fn file_names_cannot_escape_the_directory() {
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\..\\Windows\\win.ini"), "win.ini");
        assert_eq!(safe_file_name("a/b/c.txt"), "c.txt");
        assert_eq!(safe_file_name("C:\\x\\y.txt"), "y.txt");
        assert_eq!(safe_file_name(""), "download");
        assert_eq!(safe_file_name("..."), "download");
        assert_eq!(safe_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(safe_file_name("tab\there.txt"), "tab_here.txt");
        assert_eq!(suggested_name(Path::new("/tmp/x/out.csv")), "out.csv");
    }

    #[test]
    fn saving_to_downloads_never_overwrites() {
        let dir = Path::new("/dl");
        let taken = |p: &Path| {
            ["/dl/a.txt", "/dl/a (1).txt", "/dl/noext", "/dl/.hidden"]
                .iter()
                .any(|t| p == Path::new(t))
        };
        assert_eq!(unique_in(dir, "b.txt", taken), Path::new("/dl/b.txt"));
        assert_eq!(unique_in(dir, "a.txt", taken), Path::new("/dl/a (2).txt"));
        assert_eq!(unique_in(dir, "noext", taken), Path::new("/dl/noext (1)"));
        assert_eq!(
            unique_in(dir, ".hidden", taken),
            Path::new("/dl/.hidden (1)")
        );
    }

    #[test]
    fn download_feedback_names_the_file_or_the_failure() {
        assert_eq!(
            download_feedback(Path::new("/dl/a.txt"), true),
            "Saved to /dl/a.txt"
        );
        assert_eq!(
            download_feedback(Path::new("/dl/a.txt"), false),
            "Download failed"
        );
    }

    #[test]
    fn state_changes_drive_the_window() {
        let app = Some(APP);
        // Starting page -> app once ready.
        assert_eq!(
            ui_action(State::Ready, app, Showing::Local, None),
            UiAction::ShowApp(APP.into())
        );
        // Already showing this app url: no reload on a pong's Ready.
        assert_eq!(
            ui_action(State::Ready, app, Showing::App, app),
            UiAction::Nothing
        );
        // A restart is a NEW url: navigate even though the app is showing.
        assert_eq!(
            ui_action(
                State::Ready,
                Some("http://127.0.0.1:5000/"),
                Showing::App,
                app
            ),
            UiAction::ShowApp("http://127.0.0.1:5000/".into())
        );
        // The app is gone or on its way back: restarting page, never a dead one.
        for to in [State::Restarting, State::Failed, State::Stopped] {
            assert_eq!(
                ui_action(to, None, Showing::App, app),
                UiAction::ShowRestarting,
                "{to:?}"
            );
            assert_eq!(ui_action(to, None, Showing::Local, None), UiAction::Nothing);
        }
        // Diagnostics and the first start leave the window alone.
        assert_eq!(
            ui_action(State::Unresponsive, app, Showing::App, app),
            UiAction::Nothing
        );
        assert_eq!(
            ui_action(State::Starting, None, Showing::App, app),
            UiAction::Nothing
        );
        // Ready without a url (a bridge runtime) never navigates anywhere.
        assert_eq!(
            ui_action(State::Ready, None, Showing::Local, None),
            UiAction::Nothing
        );
    }
}
