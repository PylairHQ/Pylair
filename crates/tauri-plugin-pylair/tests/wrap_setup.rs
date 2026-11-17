//! Wrap mode plugin setup (W2): `Builder::wrap()` reads the runtime file the
//! CLI wrote, starts the launcher, and reaches Ready with the server's url.
//!
//! The launcher here is a hand-written fake (`python -c`) that speaks
//! hello / ready / pong, so this pins the plugin's own behaviour: the spawn
//! spec generated from `pylair-wrap.json`, the Web kind, the ready timeout,
//! and a setup that survives a window it does not own.
//!
//! Not built on Windows: a test exe linking tauri's webview stack aborts at
//! load there (see plugin_setup.rs). The decisions the window glue makes are
//! unit-tested on every OS in `wrap::policy`; the installed self-test proves
//! the real webview.
#![cfg(not(windows))]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::test::{mock_context, noop_assets, MockRuntime};
use tauri::Manager;
use tauri_plugin_pylair::wrap::WrapRuntime;
use tauri_plugin_pylair::{Builder, PyRuntime, RuntimeKind, State};

/// Both tests set process-wide env vars: run one at a time.
static ENV: Mutex<()> = Mutex::new(());

const FAKE: &str = r#"
import sys, struct, json, threading, time
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[],"kind":"web"}})
time.sleep(0.5)
w({"jsonrpc":"2.0","method":"ready","params":{"url":"http://127.0.0.1:4242/"}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m.get("method") == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m.get("method") == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": {}}); break
"#;

fn python() -> String {
    std::env::var("PYLAIR_TEST_PYTHON").unwrap_or_else(|_| "python3".into())
}

fn write_runtime(
    dir: &std::path::Path,
    source: &str,
    launcher: serde_json::Value,
    secs: u64,
) -> PathBuf {
    let p = dir.join("pylair-wrap.json");
    std::fs::write(
        &p,
        serde_json::json!({
            "version": 1, "source": source, "launcher": launcher,
            "readyTimeoutSecs": secs, "downloads": "downloads"
        })
        .to_string(),
    )
    .unwrap();
    p
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pylair-wrap-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_runtime_file_becomes_the_launcher_spawn_spec() {
    let _g = ENV.lock().unwrap();
    let dir = scratch("spec");
    let src = dir.join("srv");
    std::fs::create_dir_all(&src).unwrap();
    let cfg = write_runtime(
        &dir,
        src.to_str().unwrap(),
        serde_json::json!([
            "-m",
            "pylair.wrap",
            "--framework",
            "command",
            "--cmd",
            "python",
            "-m",
            "x",
            "{port}"
        ]),
        45,
    );
    std::env::set_var("PYLAIR_WRAP_CONFIG", &cfg);
    std::env::set_var("PYLAIR_PYTHON", "/venv/bin/python");
    let app = tauri::test::mock_builder()
        .build(mock_context(noop_assets()))
        .expect("mock app");
    let rt = WrapRuntime::load(app.handle()).expect("dev runtime file loads");
    assert_eq!(rt.ready_timeout_secs, 45);
    let spec = rt.spawn_spec(app.handle()).expect("dev spec");
    assert_eq!(spec.program, PathBuf::from("/venv/bin/python"));
    // the generated launcher interface, verbatim
    assert_eq!(
        spec.args,
        [
            "-m",
            "pylair.wrap",
            "--framework",
            "command",
            "--cmd",
            "python",
            "-m",
            "x",
            "{port}"
        ]
    );
    // the server runs in the source dir, which is also on its path
    assert_eq!(spec.cwd.as_deref(), Some(src.as_path()));
    assert!(spec
        .env
        .iter()
        .any(|(k, v)| k == "PYTHONPATH" && PathBuf::from(v) == src));
    std::env::remove_var("PYLAIR_WRAP_CONFIG");
    std::env::remove_var("PYLAIR_PYTHON");
}

#[test]
fn a_missing_or_bad_runtime_file_is_a_clear_setup_error() {
    let _g = ENV.lock().unwrap();
    std::env::set_var("PYLAIR_WRAP_CONFIG", "/no/such/pylair-wrap.json");
    let app = tauri::test::mock_builder()
        .build(mock_context(noop_assets()))
        .expect("mock app");
    let err = WrapRuntime::load(app.handle()).unwrap_err().to_string();
    assert!(
        err.contains("wrap mode config") && err.contains("pylair-wrap.json"),
        "{err}"
    );
    std::env::remove_var("PYLAIR_WRAP_CONFIG");

    // And the plugin turns it into a setup failure, not a panic.
    std::env::set_var("PYLAIR_WRAP_CONFIG", "/no/such/pylair-wrap.json");
    let res = tauri::test::mock_builder()
        .plugin(Builder::<MockRuntime>::wrap().build())
        .build(mock_context(noop_assets()));
    assert!(
        res.is_err(),
        "setup must fail when pylair-wrap.json is unreadable"
    );
    std::env::remove_var("PYLAIR_WRAP_CONFIG");
}

#[test]
fn builder_wrap_reaches_ready_with_the_servers_url() {
    let _g = ENV.lock().unwrap();
    let dir = scratch("setup");
    let cfg = write_runtime(
        &dir,
        dir.to_str().unwrap(),
        serde_json::json!(["-c", FAKE]),
        30,
    );
    std::env::set_var("PYLAIR_WRAP_CONFIG", &cfg);
    std::env::set_var("PYLAIR_PYTHON", python());
    let app = tauri::test::mock_builder()
        .plugin(Builder::<MockRuntime>::wrap().build())
        .build(mock_context(noop_assets()))
        .expect("wrap setup must not panic or fail without a window of its own");
    let rt = app.try_state::<PyRuntime>().expect("runtime is managed");
    assert_eq!(
        rt.0.kind(),
        RuntimeKind::Web,
        "Builder::wrap is the Web kind"
    );
    assert_eq!(rt.0.ready_url(), None, "no url before the server is ready");
    let t0 = Instant::now();
    while rt.0.state() != State::Ready {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "never ready: {:?}",
            rt.0.state()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(rt.0.ready_url().as_deref(), Some("http://127.0.0.1:4242/"));
    drop(app);
    std::env::remove_var("PYLAIR_WRAP_CONFIG");
    std::env::remove_var("PYLAIR_PYTHON");
}
