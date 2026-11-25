//! The installed wrap-mode self-test (`PYLAIR_SELF_TEST_WRAP=<marker>`).
//!
//! Run by an installed app on a clean machine, it proves the whole wrap
//! path on the real webview: the supervisor reached `ready`, the window
//! navigated to the app url, a GET of that url returns the app's marker,
//! and **the wrapped page has no Tauri IPC** (docs/decisions/0003-wrap-mode.md:
//! the permanent check that a future capability change fails CI).
//!
//! The IPC check needs no cooperation from the app. [`PROBE_JS`] is injected
//! into every page the window loads (only when this self-test is requested),
//! acts only on the app's own loopback origin, tries a set of IPC calls and
//! writes what happened into the url's fragment. The Rust side reads the
//! fragment off the window's url: the one channel a page with no IPC still
//! has. Inert unless the env var is set, so shipping it grants nothing.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{Manager, Runtime};

use super::Ui;

/// Injected into every page when the self-test is on. On the wrapped page
/// (`http://127.0.0.1:*`) it records whether any Tauri IPC surface exists
/// and whether each call is accepted, then reports through `location.hash`.
pub(super) const PROBE_JS: &str = r#"
(function () {
  if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1') return;
  var res = { global: typeof window.__TAURI__ !== 'undefined', calls: {} };
  var inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  res.internals = typeof inv === 'function';
  var cmds = [
    'plugin:pylair|python_state', 'plugin:pylair|python_call',
    'plugin:pylair|python_call_stream', 'plugin:pylair|python_cancel',
    'plugin:event|emit', 'plugin:event|listen', 'plugin:window|set_title',
    'plugin:app|version', 'plugin:path|resolve_directory', 'pylair_no_such_command'
  ];
  var pending = cmds.length;
  function done() {
    if (--pending === 0) location.hash = 'pylair-ipc=' + encodeURIComponent(JSON.stringify(res));
  }
  cmds.forEach(function (c) {
    if (!res.internals) { res.calls[c] = 'unavailable'; return done(); }
    var args = c === 'plugin:pylair|python_call' ? { fnName: 'x', input: {} }
      : c === 'plugin:event|emit' ? { event: 'pylair-probe' } : {};
    try {
      Promise.resolve(inv(c, args)).then(
        function () { res.calls[c] = 'accepted'; done(); },
        function () { res.calls[c] = 'rejected'; done(); });
    } catch (e) { res.calls[c] = 'rejected'; done(); }
  });
})();
"#;

/// The verdict on a probe report. `Ok` carries a one-line summary.
pub(super) fn judge_probe(report: &Value) -> Result<String, String> {
    if report["global"].as_bool() != Some(false) {
        return Err("window.__TAURI__ is present on the wrapped page".into());
    }
    let calls = report["calls"]
        .as_object()
        .ok_or("the probe reported no calls")?;
    if calls.is_empty() {
        return Err("the probe reported no calls".into());
    }
    if let Some((c, _)) = calls.iter().find(|(_, v)| v.as_str() == Some("accepted")) {
        return Err(format!("the wrapped page's IPC call {c} was ACCEPTED"));
    }
    Ok(format!(
        "{} IPC calls from the wrapped page, none accepted (__TAURI_INTERNALS__.invoke {})",
        calls.len(),
        if report["internals"].as_bool() == Some(true) {
            "present, all denied"
        } else {
            "absent"
        }
    ))
}

/// `%XX`-decode a url fragment.
pub(super) fn decode_fragment(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `(status, body)` of a raw HTTP/1.x response.
pub(super) fn parse_response(raw: &str) -> Option<(u16, &str)> {
    let status = raw
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    Some((status, body))
}

fn http_get(url: &tauri::Url) -> Result<(u16, String), String> {
    let host = url.host_str().ok_or("no host")?;
    let port = url.port().ok_or("no port")?;
    let mut s = std::net::TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(
        s,
        "GET {} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n",
        url.path()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    parse_response(&text)
        .map(|(c, b)| (c, b.to_string()))
        .ok_or_else(|| "unparseable HTTP response".into())
}

async fn until<T>(
    what: &str,
    limit: Duration,
    mut f: impl FnMut() -> Option<T>,
) -> Result<T, String> {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return Ok(v);
        }
        if t0.elapsed() > limit {
            return Err(format!("timed out waiting for {what}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Kill exactly this process (not its tree).
fn kill_pid(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        // SAFETY: kill(2) with a pid we just read from the server's own log.
        if unsafe { libc::kill(pid as i32, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
    }
}

/// After the server went down (a trigger or a kill): the restarting page is
/// shown, a NEW ready url appears, the window follows it, and a GET of it
/// still returns the marker. Returns the new url.
async fn follow_restart<R: Runtime>(
    ui: &Arc<Ui<R>>,
    old: &tauri::Url,
    marker: &str,
    shown_before: u32,
) -> Result<tauri::Url, String> {
    let app = ui.app.clone();
    until(
        "the restarting page to be shown",
        Duration::from_secs(60),
        || {
            (ui.restarting_shown
                .load(std::sync::atomic::Ordering::SeqCst)
                > shown_before)
                .then_some(())
        },
    )
    .await?;
    let new_url = until("a new ready url", Duration::from_secs(180), || {
        ui.handle
            .ready_url()
            .and_then(|u| tauri::Url::parse(&u).ok())
            .filter(|u| u != old)
    })
    .await?;
    until(
        "the window to follow the new url",
        Duration::from_secs(60),
        || {
            let cur = app.get_webview_window(super::WINDOW)?.url().ok()?;
            crate::util::same_origin(cur.as_str(), new_url.as_str()).then_some(())
        },
    )
    .await?;
    let get_url = new_url.clone();
    let (status, body) = tauri::async_runtime::spawn_blocking(move || http_get(&get_url))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("GET {new_url} after the restart: {e}"))?;
    if status >= 500 || (!marker.is_empty() && !body.contains(marker)) {
        return Err(format!(
            "GET {new_url} after the restart: {status}, marker missing"
        ));
    }
    Ok(new_url)
}

/// Wait until the wrapped page at `url` has finished loading, so script sent
/// to the window runs in the page that stays (not one about to be replaced).
async fn wait_loaded<R: Runtime>(ui: &Arc<Ui<R>>, url: &tauri::Url) -> Result<(), String> {
    let want = url.origin().ascii_serialization();
    until(
        "the app page to finish loading",
        Duration::from_secs(60),
        || (ui.app_loaded.lock().unwrap().as_deref() == Some(want.as_str())).then_some(()),
    )
    .await
}

async fn run<R: Runtime>(ui: &Arc<Ui<R>>, marker: &str) -> Result<String, (i32, String)> {
    let fail = |code: i32| move |e: String| (code, e);
    let url = until("the server to be ready", Duration::from_secs(180), || {
        ui.handle
            .ready_url()
            .and_then(|u| tauri::Url::parse(&u).ok())
    })
    .await
    .map_err(fail(4))?;

    let app = ui.app.clone();
    until(
        "the window to show the app url",
        Duration::from_secs(60),
        || {
            let w = app.get_webview_window(super::WINDOW)?;
            let cur = w.url().ok()?;
            crate::util::same_origin(cur.as_str(), url.as_str()).then_some(())
        },
    )
    .await
    .map_err(fail(5))?;

    let get_url = url.clone();
    let (status, body) = tauri::async_runtime::spawn_blocking(move || http_get(&get_url))
        .await
        .map_err(|e| (6, e.to_string()))?
        .map_err(|e| (6, format!("GET {url}: {e}")))?;
    if status >= 500 {
        return Err((6, format!("GET {url} answered {status}")));
    }
    if !marker.is_empty() && !body.contains(marker) {
        return Err((
            6,
            format!("GET {url} ({status}) lacks the marker {marker:?}"),
        ));
    }

    // The navigation lock, on the real webview: the page tries to leave for a
    // scheme the policy refuses (a file: url, in place and as a new window).
    // Nothing may load, nothing may open, and the refusals are counted. (A
    // `file:` url never even reaches the handler: the webview itself blocks a
    // web page from loading one. An https link would be handed to the system
    // browser, not something to do on a CI machine; the policy's unit tests
    // cover that decision.)
    let w = app
        .get_webview_window(super::WINDOW)
        .ok_or((10, "no window".to_string()))?;
    wait_loaded(ui, &url).await.map_err(fail(10))?;
    // The probe, hard-learned (navlock-investigate): a real WKWebView does
    // not reliably even ASK the host about an in-place navigation to a
    // scheme it cannot handle — under load the attempt is dropped inside
    // WebKit before the policy delegate runs, so no event ever arrives
    // (evidence: the new-window refusal counted, the in-place one absent
    // for the whole 30s window, the page provably intact). That is platform
    // behaviour, not a lock miss: the navigation never happens either way,
    // which is the guarantee. What the lock must do is refuse every
    // attempt it IS asked about — so the probe retries the in-place
    // attempt (each try is an independent navigation, exactly like a user
    // clicking twice) until the host has refused both, and the page-stays
    // assertion below guards the actual property either way.
    let denied_before = ui.denied.load(std::sync::atomic::Ordering::SeqCst);
    w.eval("try{window.open('pylair-selftest://blocked/b')}catch(e){}")
        .map_err(|e| (10, e.to_string()))?;
    let mut last_try = std::time::Instant::now() - Duration::from_secs(2);
    until(
        "the navigation lock to refuse the page's navigations",
        Duration::from_secs(30),
        || {
            if ui.denied.load(std::sync::atomic::Ordering::SeqCst) >= denied_before + 2 {
                return Some(());
            }
            if last_try.elapsed() >= Duration::from_secs(1) {
                last_try = std::time::Instant::now();
                let _ = w.eval("try{location.href='pylair-selftest://blocked/a'}catch(e){}");
            }
            None
        },
    )
    .await
    .map_err(|e| {
        // A refusal miss means either the webview never asked the host
        // (platform behaviour — no product bug) or the host saw it and
        // failed to count/refuse (a real lock bug). The diagnostic dump
        // says which — it is in the failure line CI prints.
        format!("{e}; diagnostics: {}", ui.nav_diagnostics(denied_before))
    })
    .map_err(fail(10))?;
    let still = w.url().map_err(|e| (10, e.to_string()))?;
    if !crate::util::same_origin(still.as_str(), url.as_str()) {
        return Err((10, format!("the window left the app: {still}")));
    }

    let report = until("the IPC probe report", Duration::from_secs(60), || {
        let w = app.get_webview_window(super::WINDOW)?;
        let cur = w.url().ok()?;
        let frag = cur.fragment()?.strip_prefix("pylair-ipc=")?.to_string();
        serde_json::from_str::<Value>(&decode_fragment(&frag)?).ok()
    })
    .await
    .map_err(fail(7))?;
    let ipc = judge_probe(&report).map_err(fail(8))?;
    let mut summary = format!(
        "GET {url} -> {status} with the marker; navigation lock refused an unknown scheme in place and as a new window; {ipc}"
    );

    // Optional (PYLAIR_SELF_TEST_WRAP_RESTART, with PYLAIR_RESTART_TRIGGER set:
    // `pylair dev` sets it, and so can an installed app's launcher): restart
    // the server through the trigger file.
    let mut current = url.clone();
    if std::env::var_os("PYLAIR_SELF_TEST_WRAP_RESTART").is_some() {
        let trigger = std::env::var_os("PYLAIR_RESTART_TRIGGER")
            .ok_or((9, "PYLAIR_RESTART_TRIGGER is not set".to_string()))?;
        let before = ui
            .restarting_shown
            .load(std::sync::atomic::Ordering::SeqCst);
        std::fs::write(&trigger, format!("{:?}", Instant::now()))
            .map_err(|e| (9, format!("cannot touch the restart trigger: {e}")))?;
        let next = follow_restart(ui, &current, marker, before)
            .await
            .map_err(fail(9))?;
        summary.push_str(&format!(
            "; restart: restarting page shown, window followed {current} -> {next}"
        ));
        current = next;
    }

    // Optional (PYLAIR_SELF_TEST_WRAP_CRASH): KILL the server (its pid is in
    // the launcher's log line) and expect the supervisor to treat it as a
    // crash: Restarting with an exit reason, the restarting page, a new url,
    // and the app serving again.
    if std::env::var_os("PYLAIR_SELF_TEST_WRAP_CRASH").is_some() {
        let pid = until(
            "the server's pid in its log",
            Duration::from_secs(30),
            || ui.server_pid(),
        )
        .await
        .map_err(fail(12))?;
        *ui.restart_reason.lock().unwrap() = None;
        let before = ui
            .restarting_shown
            .load(std::sync::atomic::Ordering::SeqCst);
        kill_pid(pid).map_err(|e| (12, format!("cannot kill the server (pid {pid}): {e}")))?;
        let next = follow_restart(ui, &current, marker, before)
            .await
            .map_err(fail(12))?;
        let reason = ui
            .restart_reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default();
        if !reason.contains("exited") {
            return Err((
                12,
                format!("a killed server must restart as a crash, reason was {reason:?}"),
            ));
        }
        summary.push_str(&format!(
            "; crash: killed server pid {pid} ({reason}), restarting page shown, window followed {current} -> {next}"
        ));
        current = next;
    }
    let _ = &current;

    // Optional (PYLAIR_SELF_TEST_WRAP_DOWNLOAD=<path>|<text>): the page starts
    // a download of <path>, and with `downloads = "downloads"` it must land in
    // the Downloads folder, containing <text>. The one file it creates is
    // removed again. (A Save As dialog cannot be driven unattended.)
    if let Ok(spec) = std::env::var("PYLAIR_SELF_TEST_WRAP_DOWNLOAD") {
        let (path, text) = spec.split_once('|').unwrap_or((spec.as_str(), ""));
        let dir = app
            .path()
            .download_dir()
            .map_err(|e| (11, format!("no Downloads folder: {e}")))?;
        let current = ui
            .handle
            .ready_url()
            .and_then(|u| tauri::Url::parse(&u).ok());
        wait_loaded(ui, current.as_ref().unwrap_or(&url))
            .await
            .map_err(fail(11))?;
        *ui.last_download.lock().unwrap() = None;
        w.eval(format!(
            "location.href = {}",
            serde_json::to_string(path).unwrap()
        ))
        .map_err(|e| (11, e.to_string()))?;
        // The webview reports the download finished, with where it went.
        let (saved, ok) = until("the download to finish", Duration::from_secs(60), || {
            ui.last_download.lock().unwrap().clone()
        })
        .await
        .map_err(fail(11))?;
        let body = std::fs::read_to_string(&saved).unwrap_or_default();
        let _ = std::fs::remove_file(&saved);
        if !ok {
            return Err((11, format!("the download to {} failed", saved.display())));
        }
        if saved.parent() != Some(dir.as_path()) {
            return Err((
                11,
                format!(
                    "saved to {}, not the Downloads folder {}",
                    saved.display(),
                    dir.display()
                ),
            ));
        }
        if !text.is_empty() && !body.contains(text) {
            return Err((11, format!("the downloaded file lacks {text:?}: {body:?}")));
        }
        summary.push_str(&format!(
            "; download saved to Downloads as {}",
            saved.file_name().unwrap().to_string_lossy()
        ));
        let still = w.url().map_err(|e| (11, e.to_string()))?;
        let current = ui.handle.ready_url().unwrap_or_else(|| url.to_string());
        if !crate::util::same_origin(still.as_str(), &current) {
            return Err((
                11,
                format!("the window left the app after the download: {still}"),
            ));
        }
    }
    Ok(summary)
}

/// Run the self-test on Tauri's runtime and exit the app with its verdict.
pub(super) fn start<R: Runtime>(ui: Arc<Ui<R>>, marker: String) {
    crate::selftest::attach_parent_console();
    tauri::async_runtime::spawn(async move {
        let code = match run(&ui, &marker).await {
            Ok(summary) => {
                println!("wrap-self-test: ok: {summary}");
                0
            }
            Err((code, why)) => {
                eprintln!("wrap-self-test: FAILED ({code}): {why}");
                code
            }
        };
        ui.app.exit(code);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_page_with_no_ipc_passes() {
        let r = json!({"global": false, "internals": true, "calls": {
            "plugin:pylair|python_state": "rejected", "plugin:event|emit": "rejected"}});
        assert!(judge_probe(&r).unwrap().contains("none accepted"));
        let r = json!({"global": false, "internals": false, "calls": {"a": "unavailable"}});
        assert!(judge_probe(&r).unwrap().contains("absent"));
    }

    #[test]
    fn any_accepted_call_or_a_global_fails() {
        let accepted = json!({"global": false, "calls": {
            "plugin:pylair|python_state": "rejected", "plugin:event|emit": "accepted"}});
        let e = judge_probe(&accepted).unwrap_err();
        assert!(
            e.contains("plugin:event|emit") && e.contains("ACCEPTED"),
            "{e}"
        );
        assert!(judge_probe(&json!({"global": true, "calls": {"a": "rejected"}})).is_err());
        // A report that says nothing is a failure, not a pass.
        assert!(judge_probe(&json!({})).is_err());
        assert!(judge_probe(&json!({"global": false, "calls": {}})).is_err());
    }

    #[test]
    fn fragments_and_responses_parse() {
        assert_eq!(
            decode_fragment("%7B%22a%22%3A1%7D").as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(decode_fragment("plain").as_deref(), Some("plain"));
        assert_eq!(decode_fragment("bad%zz"), None);
        assert_eq!(decode_fragment("cut%4"), None);
        let (code, body) = parse_response("HTTP/1.1 200 OK\r\nA: b\r\n\r\nhello").unwrap();
        assert_eq!((code, body), (200, "hello"));
        assert!(parse_response("garbage").is_none());
    }
}
