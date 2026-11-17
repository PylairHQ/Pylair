//! Per-function permissions against the REAL ACL. Every test drives the
//! plugin's real commands through Tauri's real IPC path
//! (`Webview::on_message`) on the mock runtime, with the ACL that
//! tauri-build compiled from `capabilities/*.json` and the plugin's
//! permission files (see `src/lib.rs`). Each scenario owns window labels, so
//! one compiled capability set covers every case.
//!
//! Not built on Windows: same webview-link constraint as the plugin's own
//! mock-window tests (event_scoping.rs, call_via_command.rs). CI runs it on
//! Linux and macOS through the workspace `cargo test`; the real-webview proof
//! on all three OSes is the installed self-test (win-b probe).
#![cfg(not(windows))]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponseBody};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{Listener, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_pylair::protocol::PERMISSION_DENIED;
use tauri_plugin_pylair::supervisor::{SpawnSpec, State};
use tauri_plugin_pylair::{events, Builder, PyRuntime, StderrDelivery, SupervisorConfig};

const CALL: &str = "python_call";
const STREAM: &str = "python_call_stream";

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        let ok = std::process::Command::new(cand)
            .arg("-c")
            .arg("import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return cand.into();
        }
    }
    panic!("no python >=3.12 found for the fn-scope ACL tests");
}

fn runtime_spec() -> SpawnSpec {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    SpawnSpec {
        program: python(),
        args: vec!["-m".into(), "pylair".into(), "testapp".into()],
        env: vec![(
            "PYTHONPATH".into(),
            root.join("python/pylair/src")
                .to_string_lossy()
                .into_owned(),
        )],
        cwd: Some(root.join("python/pylair/tests")),
        ..Default::default()
    }
}

fn config() -> SupervisorConfig {
    let mut cfg = SupervisorConfig::default();
    cfg.hello_timeout = Duration::from_secs(30);
    cfg.shutdown_grace = Duration::from_secs(2);
    cfg
}

type ChannelLog = Arc<Mutex<Vec<(String, Value)>>>;

struct Fx {
    app: tauri::App<MockRuntime>,
    windows: Vec<WebviewWindow<MockRuntime>>,
    /// Every message the plugin sent on any per-call channel: (label, body).
    channel: ChannelLog,
}

impl Fx {
    fn win(&self, label: &str) -> &WebviewWindow<MockRuntime> {
        self.windows
            .iter()
            .find(|w| w.label() == label)
            .unwrap_or_else(|| panic!("no window {label}"))
    }
}

/// An app built from the REAL compiled context, a live runtime, and one mock
/// window per label. The channel interceptor consumes every per-call channel
/// message so tests can see exactly what reached a Channel. Stderr goes to
/// every subscribed window, so a window's subscription is observable.
fn fixture(labels: &[&str]) -> Fx {
    let channel: ChannelLog = Default::default();
    let sink = channel.clone();
    let app = mock_builder()
        .channel_interceptor(move |wv, _cb, _idx, body| {
            let v = match body {
                InvokeResponseBody::Json(s) => serde_json::from_str(s).unwrap(),
                InvokeResponseBody::Raw(_) => json!("<raw>"),
            };
            sink.lock().unwrap().push((wv.label().to_string(), v));
            true
        })
        .plugin(
            Builder::<MockRuntime>::new(runtime_spec())
                .config(config())
                .stderr_delivery(StderrDelivery::All)
                .build(),
        )
        .build(fn_scope_acl_tests::context())
        .expect("app builds with the plugin and the compiled context");
    let windows = labels
        .iter()
        .map(|l| {
            WebviewWindowBuilder::new(&app, *l, WebviewUrl::default())
                .build()
                .unwrap()
        })
        .collect();
    let fx = Fx {
        app,
        windows,
        channel,
    };
    wait_ready(&fx);
    fx
}

fn wait_ready(fx: &Fx) {
    let rt = fx.app.state::<PyRuntime>();
    let deadline = Instant::now() + Duration::from_secs(30);
    while rt.0.state() != State::Ready {
        assert!(
            Instant::now() < deadline,
            "runtime never reached Ready (at {:?})",
            rt.0.state()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn request(cmd: &str, args: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: format!("plugin:pylair|{cmd}"),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().unwrap(),
        body: InvokeBody::Json(args),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
    get_ipc_response(w, request(cmd, args)).map(|b| b.deserialize::<Value>().unwrap())
}

static NEXT_CHANNEL: AtomicU32 = AtomicU32::new(100);

/// The JS-side argument shape for the command: camelCase keys, and the
/// channel as the `__CHANNEL__:<id>` string the Tauri JS API sends.
fn args_for(cmd: &str, name: &str, input: Value, stream: bool) -> Value {
    if cmd == CALL {
        json!({ "fnName": name, "input": input })
    } else {
        let id = NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed);
        json!({
            "onEvent": format!("__CHANNEL__:{id}"),
            "fnName": name,
            "input": input,
            "stream": stream,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Out {
    /// Reached the supervisor and succeeded.
    Ok,
    /// Refused by the function check (PERMISSION_DENIED).
    Denied,
    /// Reached the supervisor and the runtime answered with this error code.
    Runtime(i64),
    /// Tauri's ACL refused the command before any handler ran.
    Acl(String),
}

impl Out {
    fn reached_supervisor(&self) -> bool {
        matches!(self, Out::Ok | Out::Runtime(_))
    }
}

fn classify(r: Result<Value, Value>) -> Out {
    match r {
        Ok(_) => Out::Ok,
        Err(v) => match v.get("code").and_then(Value::as_i64) {
            Some(c) if c == PERMISSION_DENIED => Out::Denied,
            Some(c) => Out::Runtime(c),
            None => Out::Acl(v.to_string()),
        },
    }
}

fn inputs(name: &str) -> (Value, bool) {
    match name {
        "add" => (json!({"a": 1, "b": 2}), false),
        "counter" => (json!({"n": 2}), true),
        "spy" => (json!({"a": 1}), false),
        "spy_count" => (json!({}), false),
        "print_then_answer" => (json!({"text": "probe"}), false),
        other => panic!("no canned input for {other}"),
    }
}

fn via(w: &WebviewWindow<MockRuntime>, cmd: &str, name: &str) -> Out {
    let (input, stream) = inputs(name);
    classify(invoke(w, cmd, args_for(cmd, name, input, stream)))
}

// ---- deny ----------------------------------------------------------------

#[test]
fn a_deny_entry_denies_matching_functions_on_both_call_commands() {
    let fx = fixture(&["deny"]);
    let w = fx.win("deny");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: spy matches sp*");
        assert_eq!(
            via(w, cmd, "spy_count"),
            Out::Denied,
            "{cmd}: spy_count matches sp*"
        );
        assert!(
            via(w, cmd, "add").reached_supervisor(),
            "{cmd}: add must pass"
        );
    }
    // The set-level entry leaves the other default commands working.
    assert_eq!(
        invoke(w, "python_state", json!({})).unwrap(),
        json!("ready")
    );
}

#[test]
fn the_denial_error_names_the_function_and_the_window() {
    let fx = fixture(&["deny"]);
    let w = fx.win("deny");
    for cmd in [CALL, STREAM] {
        let (input, stream) = inputs("spy");
        let err = invoke(w, cmd, args_for(cmd, "spy", input, stream)).expect_err("spy is denied");
        assert_eq!(err["code"], json!(-32050), "{cmd}: {err}");
        assert_eq!(err["data"], json!({"fn": "spy", "window": "deny"}), "{cmd}");
        let msg = err["message"].as_str().unwrap();
        assert!(
            msg.contains("spy") && msg.contains("deny"),
            "{cmd}: message must name the function and the window: {msg}"
        );
    }
}

// ---- allow list ----------------------------------------------------------

#[test]
fn an_allow_list_miss_is_denied() {
    let fx = fixture(&["allow-miss"]);
    let w = fx.win("allow-miss");
    for cmd in [CALL, STREAM] {
        assert!(
            via(w, cmd, "add").reached_supervisor(),
            "{cmd}: exact allow"
        );
        assert!(
            via(w, cmd, "counter").reached_supervisor(),
            "{cmd}: glob allow count*"
        );
        assert_eq!(
            via(w, cmd, "spy"),
            Out::Denied,
            "{cmd}: allow list present, no match"
        );
    }
}

#[test]
fn deny_beats_an_allow_entry_that_also_matches() {
    let fx = fixture(&["deny-wins"]);
    let w = fx.win("deny-wins");
    for cmd in [CALL, STREAM] {
        assert_eq!(
            via(w, cmd, "spy"),
            Out::Denied,
            "{cmd}: allow sp* and deny spy"
        );
        assert!(
            via(w, cmd, "spy_count").reached_supervisor(),
            "{cmd}: allow sp*, not denied"
        );
        assert_eq!(
            via(w, cmd, "add"),
            Out::Denied,
            "{cmd}: allow list present, no match"
        );
    }
}

// ---- no entries ----------------------------------------------------------

#[test]
fn no_entries_allows_every_function() {
    let fx = fixture(&["open"]);
    let w = fx.win("open");
    for cmd in [CALL, STREAM] {
        for name in ["add", "counter", "spy", "spy_count"] {
            assert!(
                via(w, cmd, name).reached_supervisor(),
                "{cmd}: {name} must pass with no scope entries"
            );
        }
    }
}

// ---- several capabilities, several windows -------------------------------

#[test]
fn two_capabilities_on_one_window_union_their_allow_lists() {
    let fx = fixture(&["union"]);
    let w = fx.win("union");
    for cmd in [CALL, STREAM] {
        assert!(
            via(w, cmd, "add").reached_supervisor(),
            "{cmd}: allowed by the first capability"
        );
        assert!(
            via(w, cmd, "counter").reached_supervisor(),
            "{cmd}: allowed by the second capability"
        );
        assert_eq!(
            via(w, cmd, "spy"),
            Out::Denied,
            "{cmd}: in neither allow list"
        );
    }
}

#[test]
fn two_windows_see_only_their_own_capability() {
    let fx = fixture(&["win-x", "win-y"]);
    let (x, y) = (fx.win("win-x"), fx.win("win-y"));
    for cmd in [CALL, STREAM] {
        assert!(via(x, cmd, "add").reached_supervisor(), "{cmd}: x allows add");
        assert_eq!(via(x, cmd, "counter"), Out::Denied, "{cmd}: x denies counter");
        assert!(
            via(y, cmd, "counter").reached_supervisor(),
            "{cmd}: y allows counter"
        );
        assert_eq!(via(y, cmd, "add"), Out::Denied, "{cmd}: y denies add");
    }
}

#[test]
fn a_window_no_capability_covers_is_rejected_by_the_acl() {
    let fx = fixture(&["uncovered"]);
    let rt = fx.app.state::<PyRuntime>();
    let watermark = rt.0.task_watermark();
    let w = fx.win("uncovered");
    for cmd in [CALL, STREAM] {
        let out = via(w, cmd, "add");
        assert!(
            matches!(out, Out::Acl(_)),
            "{cmd}: an uncovered window never reaches the handler, got {out:?}"
        );
    }
    assert_eq!(rt.0.task_watermark(), watermark, "nothing may reach the supervisor");
}

// ---- malformed entries ---------------------------------------------------

#[test]
fn a_malformed_scope_entry_fails_closed_instead_of_becoming_no_entry() {
    let fx = fixture(&["malformed"]);
    let rt = fx.app.state::<PyRuntime>();
    let watermark = rt.0.task_watermark();
    let w = fx.win("malformed");
    for cmd in [CALL, STREAM] {
        let out = via(w, cmd, "add");
        assert!(
            matches!(out, Out::Acl(_)),
            "{cmd}: a scope entry that does not parse must refuse the call, got {out:?}"
        );
    }
    assert_eq!(rt.0.task_watermark(), watermark, "nothing may reach the supervisor");
}

// ---- a denial touches no state ------------------------------------------

#[test]
fn a_denied_call_takes_no_slot_subscribes_nothing_and_sends_nothing() {
    let fx = fixture(&["no-slot", "other"]);
    let (denied, other) = (fx.win("no-slot"), fx.win("other"));
    let rt = fx.app.state::<PyRuntime>();

    // A stderr listener per window: stderr goes to every SUBSCRIBED window
    // (this fixture's StderrDelivery::All), so it shows who is subscribed.
    let count = |w: &WebviewWindow<MockRuntime>| {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        w.listen(events::STDERR, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        n
    };
    let denied_stderr = count(denied);
    let other_stderr = count(other);

    let watermark = rt.0.task_watermark();
    let sent = fx.channel.lock().unwrap().len();

    for cmd in [STREAM, CALL] {
        let (input, stream) = inputs("counter");
        let err = invoke(denied, cmd, args_for(cmd, "counter", input, stream))
            .expect_err("counter is denied for no-slot");
        assert_eq!(err["code"], json!(PERMISSION_DENIED), "{cmd}: {err}");
        assert_eq!(err["data"]["fn"], json!("counter"), "{cmd}: {err}");
        assert_eq!(err["data"]["window"], json!("no-slot"), "{cmd}: {err}");
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        rt.0.task_watermark(),
        watermark,
        "no task id may be allocated"
    );
    assert_eq!(
        fx.channel.lock().unwrap().len(),
        sent,
        "nothing may reach the Channel"
    );

    // The other window makes an allowed call that writes a stderr line: it
    // subscribes `other`, and the denied window, never subscribed, hears
    // nothing.
    assert!(via(other, CALL, "print_then_answer").reached_supervisor());
    let deadline = Instant::now() + Duration::from_secs(15);
    while other_stderr.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the stderr probe never arrived");
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        denied_stderr.load(Ordering::SeqCst),
        0,
        "a denied call must not subscribe its window"
    );

    // Control: an allowed call from the denied window takes exactly the next
    // id (the probe took `watermark`) and delivers its terminal on its
    // Channel.
    let sent = fx.channel.lock().unwrap().len();
    let ack = invoke(
        denied,
        STREAM,
        args_for(STREAM, "add", json!({"a": 1, "b": 2}), false),
    )
    .expect("add is allowed for no-slot");
    assert_eq!(
        ack["task"],
        json!(watermark + 1),
        "the denied calls must not have consumed an id"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while fx.channel.lock().unwrap().len() <= sent {
        assert!(Instant::now() < deadline, "the allowed call never delivered");
        std::thread::sleep(Duration::from_millis(25));
    }
    let (label, body) = fx.channel.lock().unwrap()[sent].clone();
    assert_eq!(label, "no-slot");
    assert_eq!(
        body,
        json!({"task": watermark + 1, "done": true, "result": 3})
    );
}

// ---- behaviour the docs and the CLI check describe ----------------------

#[test]
fn a_scoped_allow_narrows_the_unscoped_default_grant() {
    let fx = fixture(&["narrow"]);
    let w = fx.win("narrow");
    for cmd in [CALL, STREAM] {
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: on the allow list");
        assert_eq!(
            via(w, cmd, "counter"),
            Out::Denied,
            "{cmd}: pylair:default grants every function, but the scoped allow list narrows it"
        );
    }
}

#[test]
fn a_scope_on_only_one_call_command_leaves_the_other_open() {
    // The trap the CLI check flags: the page can invoke either command.
    let fx = fixture(&["one-command"]);
    let w = fx.win("one-command");
    assert_eq!(via(w, CALL, "spy"), Out::Denied, "scoped command");
    assert!(
        via(w, STREAM, "spy").reached_supervisor(),
        "the unscoped command is still open"
    );
}
