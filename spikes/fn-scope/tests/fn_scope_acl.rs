//! SPIKE ONLY: evidence for per-function permissions via Tauri command
//! scopes. Every test drives the plugin's real commands through Tauri's real
//! IPC path (`Webview::on_message`) on the mock runtime, against the ACL that
//! tauri-build compiled from `capabilities/*.json` and the plugin's
//! permission files. Each scenario owns one window label, so one compiled
//! capability set covers every question.
//!
//! Not built on Windows: same webview-link constraint as the plugin's own
//! mock-window tests (plugin_setup.rs, call_via_command.rs).
#![cfg(not(windows))]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponseBody};
use tauri::test::{
    get_ipc_response, mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY,
};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_pylair::fn_scope::{observed, warnings, Seen};
use tauri_plugin_pylair::protocol::PERMISSION_DENIED;
use tauri_plugin_pylair::supervisor::{SpawnSpec, State};
use tauri_plugin_pylair::{Builder, PyRuntime, Registry, SupervisorConfig};

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
    panic!("no python >=3.12 found for the fn-scope spike");
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
    cfg.hello_timeout = Duration::from_secs(15);
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
/// message so tests can see exactly what reached a Channel.
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
                .build(),
        )
        .build(fn_scope_spike::context())
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

fn request(url: &str, cmd: &str, args: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: format!("plugin:pylair|{cmd}"),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: url.parse().unwrap(),
        body: InvokeBody::Json(args),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

const LOCAL: &str = "tauri://localhost";

fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
    invoke_from(w, LOCAL, cmd, args)
}

fn invoke_from(
    w: &WebviewWindow<MockRuntime>,
    url: &str,
    cmd: &str,
    args: Value,
) -> Result<Value, Value> {
    get_ipc_response(w, request(url, cmd, args))
        .map(|b| b.deserialize::<Value>().unwrap())
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
    /// Refused by the fn-scope check (PERMISSION_DENIED).
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
        other => panic!("no canned input for {other}"),
    }
}

fn via(w: &WebviewWindow<MockRuntime>, cmd: &str, name: &str) -> Out {
    let (input, stream) = inputs(name);
    classify(invoke(w, cmd, args_for(cmd, name, input, stream)))
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn seen(label: &str, cmd: &str) -> Seen {
    let s = observed(label, cmd)
        .unwrap_or_else(|| panic!("the {cmd} handler never recorded a scope for window {label}"));
    Seen {
        allow: sorted(s.allow),
        deny: sorted(s.deny),
    }
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// ---- Q1: does an object entry reach the handler's CommandScope? ----------

#[test]
fn q1_object_deny_entry_reaches_the_handler_and_denies() {
    let fx = fixture(&["q1"]);
    let w = fx.win("q1");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: spy matches sp*");
        assert_eq!(via(w, cmd, "spy_count"), Out::Denied, "{cmd}: spy_count matches sp*");
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: add must pass");
        assert_eq!(
            seen("q1", cmd),
            Seen { allow: vec![], deny: names(&["sp*"]) },
            "{cmd}: the handler must see exactly the capability's deny entry"
        );
    }
}

#[test]
fn q1_allow_list_present_and_no_match_is_denied() {
    let fx = fixture(&["q1-allow"]);
    let w = fx.win("q1-allow");
    for cmd in [CALL, STREAM] {
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: exact allow");
        assert!(via(w, cmd, "counter").reached_supervisor(), "{cmd}: glob allow count*");
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: allow list present, no match");
        assert_eq!(
            seen("q1-allow", cmd),
            Seen { allow: names(&["add", "count*"]), deny: vec![] }
        );
    }
}

#[test]
fn q1_deny_beats_an_allow_entry_that_also_matches() {
    let fx = fixture(&["q1-both"]);
    let w = fx.win("q1-both");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: allow sp* and deny spy");
        assert!(via(w, cmd, "spy_count").reached_supervisor(), "{cmd}: allow sp*, not denied");
        assert_eq!(via(w, cmd, "add"), Out::Denied, "{cmd}: allow list present, no match");
    }
}

// ---- Q2: pylair:default AND a scoped entry on the same permission --------

#[test]
fn q2_scoped_deny_beats_the_unscoped_grant_in_one_capability() {
    let fx = fixture(&["q2"]);
    let w = fx.win("q2");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: deny beats the unscoped grant");
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: everything else still allowed");
        assert_eq!(seen("q2", cmd), Seen { allow: vec![], deny: names(&["sp*"]) });
    }
    // The unscoped grant is still what authorises the other commands.
    assert_eq!(invoke(w, "python_state", json!({})).unwrap(), json!("ready"));
}

#[test]
fn q2_a_scoped_allow_next_to_the_unscoped_grant_narrows_it() {
    // Surprise under test: the unscoped grant contributes no scope entries,
    // so it cannot widen an allow list. The handler sees only the scoped
    // entry.
    let fx = fixture(&["q2b"]);
    let w = fx.win("q2b");
    for cmd in [CALL, STREAM] {
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: add is on the allow list");
        assert_eq!(
            via(w, cmd, "counter"),
            Out::Denied,
            "{cmd}: pylair:default grants every function, but the scoped allow list narrows it"
        );
        assert_eq!(seen("q2b", cmd), Seen { allow: names(&["add"]), deny: vec![] });
    }
}

#[test]
fn q2_scope_on_the_default_set_applies_to_both_call_commands() {
    let fx = fixture(&["q2c"]);
    let w = fx.win("q2c");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: set-level deny");
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: add allowed");
        assert_eq!(seen("q2c", cmd), Seen { allow: vec![], deny: names(&["sp*"]) });
    }
    assert_eq!(invoke(w, "python_state", json!({})).unwrap(), json!("ready"));
}

#[test]
fn q2_a_scope_on_only_one_call_command_leaves_the_other_open() {
    let fx = fixture(&["q2d"]);
    let w = fx.win("q2d");
    assert_eq!(via(w, STREAM, "spy"), Out::Denied, "scoped command is closed");
    assert!(
        via(w, CALL, "spy").reached_supervisor(),
        "python_call carries no scope here, so the same function is reachable through it"
    );
}

// ---- Q3: two capabilities covering the same window ------------------------

#[test]
fn q3_allow_entries_from_two_capabilities_union() {
    let fx = fixture(&["q3"]);
    let w = fx.win("q3");
    for cmd in [CALL, STREAM] {
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}: from capability q3-a");
        assert!(via(w, cmd, "counter").reached_supervisor(), "{cmd}: from capability q3-b");
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: in neither");
        assert_eq!(seen("q3", cmd), Seen { allow: names(&["add", "counter"]), deny: vec![] });
    }
}

#[test]
fn q3_an_unscoped_grant_in_another_capability_is_narrowed_by_a_scoped_allow() {
    let fx = fixture(&["q3b"]);
    let w = fx.win("q3b");
    for cmd in [CALL, STREAM] {
        assert!(via(w, cmd, "add").reached_supervisor(), "{cmd}");
        assert_eq!(
            via(w, cmd, "counter"),
            Out::Denied,
            "{cmd}: capability q3b-a grants everything, q3b-b's allow list still narrows it"
        );
        assert_eq!(seen("q3b", cmd), Seen { allow: names(&["add"]), deny: vec![] });
    }
}

#[test]
fn q3_a_deny_in_one_capability_beats_an_allow_in_another() {
    let fx = fixture(&["q3c"]);
    let w = fx.win("q3c");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: allowed by q3c-a, denied by q3c-b");
        assert_eq!(seen("q3c", cmd), Seen { allow: names(&["spy"]), deny: names(&["spy"]) });
    }
}

#[test]
fn q3_deny_entries_from_two_capabilities_union() {
    let fx = fixture(&["q3d"]);
    let w = fx.win("q3d");
    for cmd in [CALL, STREAM] {
        assert_eq!(via(w, cmd, "spy"), Out::Denied, "{cmd}: denied by q3d-a");
        assert_eq!(via(w, cmd, "add"), Out::Denied, "{cmd}: denied by q3d-b");
        assert!(via(w, cmd, "counter").reached_supervisor(), "{cmd}: denied by neither");
        assert_eq!(seen("q3d", cmd), Seen { allow: vec![], deny: names(&["add", "spy"]) });
    }
}

// ---- Q4: two windows, different capabilities ------------------------------

#[test]
fn q4_each_window_sees_only_its_own_capability() {
    let fx = fixture(&["q4-a", "q4-b", "q4-none"]);
    let (a, b) = (fx.win("q4-a"), fx.win("q4-b"));
    for cmd in [CALL, STREAM] {
        assert!(via(a, cmd, "add").reached_supervisor(), "{cmd}: q4-a may add");
        assert_eq!(via(a, cmd, "counter"), Out::Denied, "{cmd}: q4-a may not counter");
        assert_eq!(via(b, cmd, "add"), Out::Denied, "{cmd}: q4-b may not add");
        assert!(via(b, cmd, "counter").reached_supervisor(), "{cmd}: q4-b may counter");
        assert_eq!(seen("q4-a", cmd), Seen { allow: names(&["add"]), deny: vec![] });
        assert_eq!(seen("q4-b", cmd), Seen { allow: names(&["counter"]), deny: vec![] });
    }
    // A window no capability covers never reaches the handler at all.
    let none = fx.win("q4-none");
    for cmd in [CALL, STREAM] {
        assert!(matches!(via(none, cmd, "add"), Out::Acl(_)), "{cmd}: not covered by any capability");
        assert!(observed("q4-none", cmd).is_none(), "{cmd}: handler must not run");
    }
}

// ---- Q5: no scope entries --------------------------------------------------

#[test]
fn q5_no_entries_means_empty_allow_and_empty_deny_and_everything_allowed() {
    let fx = fixture(&["q5"]);
    let w = fx.win("q5");
    for cmd in [CALL, STREAM] {
        for name in ["add", "counter", "spy"] {
            assert!(via(w, cmd, name).reached_supervisor(), "{cmd}: {name}");
        }
        assert_eq!(seen("q5", cmd), Seen { allow: vec![], deny: vec![] });
    }
}

#[test]
fn q5_an_explicit_empty_allow_array_is_indistinguishable_from_no_entries() {
    // Footgun under test: `allow: []` reads like "allow nothing" but the
    // handler sees the same empty lists as an unscoped grant.
    let fx = fixture(&["q5b"]);
    let w = fx.win("q5b");
    for cmd in [CALL, STREAM] {
        assert!(via(w, cmd, "spy").reached_supervisor(), "{cmd}: allow [] allows everything");
        assert_eq!(seen("q5b", cmd), Seen { allow: vec![], deny: vec![] });
    }
}

// ---- Q6: can the check be tested without a real window? -------------------

#[test]
fn q6_the_stock_mock_context_has_an_empty_acl_and_rejects_every_plugin_command() {
    let app = mock_builder()
        .plugin(
            Builder::<MockRuntime>::new(runtime_spec())
                .config(config())
                .build(),
        )
        .build(mock_context(noop_assets()))
        .unwrap();
    let w = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
        .build()
        .unwrap();
    let r = invoke(&w, "python_state", json!({}));
    let msg = r.expect_err("the empty mock ACL must reject plugin commands").to_string();
    assert!(msg.contains("not allowed"), "unexpected rejection: {msg}");
}

#[test]
fn q6_the_real_authority_is_consulted_for_origin_and_window() {
    let fx = fixture(&["q2"]);
    let w = fx.win("q2");
    // Same window, same command, local origin: allowed.
    assert_eq!(invoke(w, "python_state", json!({})).unwrap(), json!("ready"));
    // Same window, remote origin: the capability is local-only.
    let r = invoke_from(w, "https://evil.example/", "python_state", json!({}));
    let msg = r.expect_err("remote origin must be refused").to_string();
    assert!(msg.contains("not allowed"), "unexpected rejection: {msg}");
}

// ---- Q7: a denied python_call_stream leaves the supervisor untouched ------

#[test]
fn q7_denied_calls_take_no_slot_subscribe_nothing_and_send_nothing() {
    let fx = fixture(&["q7"]);
    let w = fx.win("q7");
    let rt = fx.app.state::<PyRuntime>();
    let reg = fx.app.state::<Registry>();

    let watermark = rt.0.task_watermark();
    assert!(!reg.lock().unwrap().is_subscribed("q7"));
    let sent = fx.channel.lock().unwrap().len();

    for cmd in [STREAM, CALL] {
        let (input, stream) = inputs("counter");
        let err = invoke(w, cmd, args_for(cmd, "counter", input, stream))
            .expect_err("counter is denied for q7");
        assert_eq!(err["code"], json!(PERMISSION_DENIED), "{cmd}: {err}");
        assert_eq!(err["data"]["fn"], json!("counter"), "{cmd}: {err}");
        assert_eq!(err["data"]["window"], json!("q7"), "{cmd}: {err}");
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rt.0.task_watermark(), watermark, "no task id may be allocated");
    assert!(!reg.lock().unwrap().is_subscribed("q7"), "a denied call must not subscribe");
    assert_eq!(fx.channel.lock().unwrap().len(), sent, "nothing may reach the Channel");

    // Control: an allowed call from the same window takes exactly the next
    // id, subscribes the window and delivers its terminal on the Channel.
    let ack = invoke(w, STREAM, args_for(STREAM, "add", json!({"a": 1, "b": 2}), false))
        .expect("add is allowed for q7");
    assert_eq!(ack["task"], json!(watermark), "the denied calls must not have consumed an id");
    assert_eq!(rt.0.task_watermark(), watermark + 1);
    assert!(reg.lock().unwrap().is_subscribed("q7"));
    let deadline = Instant::now() + Duration::from_secs(15);
    while fx.channel.lock().unwrap().len() <= sent {
        assert!(Instant::now() < deadline, "the allowed call never delivered");
        std::thread::sleep(Duration::from_millis(25));
    }
    let (label, body) = fx.channel.lock().unwrap()[sent].clone();
    assert_eq!(label, "q7");
    assert_eq!(body, json!({"task": watermark, "done": true, "result": 3}));
}

// ---- startup-warning prototype ----------------------------------------------

#[test]
fn a_pattern_matching_no_exposed_function_warns_once() {
    let fx = fixture(&["q9"]);
    let w = fx.win("q9");
    // The exposes manifest lands just after Ready: keep calling until the
    // prototype has the names to compare against.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !warnings().iter().any(|l| l.contains("q9")) {
        assert!(Instant::now() < deadline, "no warning for q9: {:?}", warnings());
        assert!(via(w, CALL, "add").reached_supervisor());
        std::thread::sleep(Duration::from_millis(50));
    }
    for _ in 0..3 {
        assert!(via(w, STREAM, "add").reached_supervisor());
    }
    let mine: Vec<String> = warnings().into_iter().filter(|l| l.contains("q9")).collect();
    assert!(mine.iter().any(|l| l.contains("typo_*")), "{mine:?}");
    assert!(!mine.iter().any(|l| l.contains("\"add\"")), "add matches a real function: {mine:?}");
    let typo = mine.iter().filter(|l| l.contains("typo_*")).count();
    assert_eq!(typo, 1, "warn once per window and pattern, got: {mine:?}");
}
