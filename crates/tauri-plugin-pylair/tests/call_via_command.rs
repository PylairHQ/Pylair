//! rc.15 PR I — the REAL `python_call_stream` command end-to-end:
//! declared intent arg → supervisor registration → real pylair runtime
//! → per-call Channel payloads (fully observable via `Channel::new`).
//! This is the path a client `call()`/`stream()` takes — the seam the
//! fake-runtime unit tests and the pytest harness cannot see.
//!
//! Not built on Windows: same webview-link constraint as plugin_setup.
#![cfg(not(windows))]

use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::test::{mock_context, noop_assets, MockRuntime};
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{Limits, SpawnSpec, State};
use tauri_plugin_pylair::{
    python_call_impl, python_call_stream_impl, python_state_impl, Builder, PyRuntime, Registry,
    SupervisorConfig,
};

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
    panic!("no python >=3.12 found for command tests");
}

fn real_runtime_spec() -> SpawnSpec {
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

fn collecting_channel() -> (
    Channel<serde_json::Value>,
    Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = got.clone();
    let ch = Channel::<serde_json::Value>::new(move |body: InvokeResponseBody| {
        let InvokeResponseBody::Json(s) = body else {
            panic!("channel payload must be Json, got Raw");
        };
        sink.lock().unwrap().push(serde_json::from_str(&s).unwrap());
        Ok(())
    });
    (ch, got)
}

fn wait_until_drained(got: &Arc<Mutex<Vec<serde_json::Value>>>, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while got.lock().unwrap().len() < want {
        assert!(
            std::time::Instant::now() < deadline,
            "channel never delivered {want} payloads: {:?}",
            got.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn command_path_declared_intents_and_mismatches() {
    let mut cfg = SupervisorConfig::default();
    cfg.hello_timeout = Duration::from_secs(15);
    cfg.shutdown_grace = Duration::from_secs(2);
    let app = tauri::test::mock_builder()
        .plugin(
            Builder::<MockRuntime>::new(real_runtime_spec())
                .config(cfg)
                .build(),
        )
        .build(mock_context(noop_assets()))
        .expect("app builds with the plugin");
    let w: WebviewWindow<MockRuntime> =
        WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
            .build()
            .unwrap();

    let rt_state = app.state::<PyRuntime>();
    let reg = app.state::<Registry>();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while rt_state.0.state() != State::Ready {
        assert!(
            std::time::Instant::now() < deadline,
            "runtime never reached Ready (at {:?})",
            rt_state.0.state()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let rt = tokio::runtime::Runtime::new().unwrap();

    // call() intent on a plain fn: the ack carries the task id + bridge
    // protocol, then the terminal lands on the channel.
    let (ch, got) = collecting_channel();
    let ack = rt
        .block_on(python_call_stream_impl(
            &w,
            &reg,
            &rt_state,
            ch,
            "add".into(),
            json!({"a": 1, "b": 2}),
            Some(false),
        ))
        .expect("python_call_stream_impl failed");
    assert_eq!(ack.protocol, 2);
    wait_until_drained(&got, 1);
    let payloads = got.lock().unwrap().clone();
    assert_eq!(
        payloads,
        vec![json!({"task": ack.task, "done": true, "result": 3})],
        "a plain call through the command must return its terminal: {payloads:?}"
    );

    // call() intent on a generator → INVALID_PARAMS naming stream().
    let (ch, got) = collecting_channel();
    rt.block_on(python_call_stream_impl(
        &w,
        &reg,
        &rt_state,
        ch,
        "counter".into(),
        json!({"n": 1}),
        Some(false),
    ))
    .expect("python_call_stream_impl failed");
    wait_until_drained(&got, 1);
    let err = &got.lock().unwrap()[0]["error"]["message"];
    assert!(
        err.as_str()
            .unwrap()
            .contains("is a stream function — call it with stream()"),
        "wrong message: {err}"
    );

    // stream() intent on a plain fn → INVALID_PARAMS naming call().
    let (ch, got) = collecting_channel();
    rt.block_on(python_call_stream_impl(
        &w,
        &reg,
        &rt_state,
        ch,
        "add".into(),
        json!({"a": 1, "b": 2}),
        Some(true),
    ))
    .expect("python_call_stream_impl failed");
    wait_until_drained(&got, 1);
    let err = &got.lock().unwrap()[0]["error"]["message"];
    assert!(
        err.as_str()
            .unwrap()
            .contains("is not a stream function — call it with call()"),
        "wrong message: {err}"
    );

    // Legacy shape (stream: None): no field on the wire, no validation —
    // a plain fn still runs.
    let (ch, got) = collecting_channel();
    rt.block_on(python_call_stream_impl(
        &w,
        &reg,
        &rt_state,
        ch,
        "add".into(),
        json!({"a": 4, "b": 5}),
        None,
    ))
    .expect("python_call_stream_impl failed");
    wait_until_drained(&got, 1);
    let payloads = got.lock().unwrap().clone();
    assert_eq!(
        payloads,
        vec![json!({"task": payloads[0]["task"], "done": true, "result": 9})],
        "legacy no-intent call must succeed unvalidated: {payloads:?}"
    );
}

fn wait_until(mut ok: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition never met: {what}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 0006 PR 0 (amends 0005 §3): event subscription and the fairness live
/// set are separate memberships. A window that only polls `python_state`
/// receives events but reserves no share — it must not shrink a live
/// window's ceiling. Its first `python_call` joins, and ceilings move.
/// Driven through the real command bodies, not the test-only
/// `fairness_join_window` hook.
#[test]
fn observer_window_joins_fairness_only_on_its_first_call() {
    let mut cfg = SupervisorConfig::default();
    cfg.hello_timeout = Duration::from_secs(15);
    cfg.shutdown_grace = Duration::from_secs(2);
    // cap 4, two live windows -> ceiling 3 (the fairness unit-test
    // worked example the selftest also relies on).
    cfg.limits = Limits::default().with_max_concurrent_tasks(4);
    let app = tauri::test::mock_builder()
        .plugin(
            Builder::<MockRuntime>::new(real_runtime_spec())
                .config(cfg)
                .build(),
        )
        .build(mock_context(noop_assets()))
        .expect("app builds with the plugin");
    let w_main: WebviewWindow<MockRuntime> =
        WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
            .build()
            .unwrap();
    let w_obs: WebviewWindow<MockRuntime> =
        WebviewWindowBuilder::new(&app, "observer", WebviewUrl::default())
            .build()
            .unwrap();

    let rt_state = app.state::<PyRuntime>();
    let reg = app.state::<Registry>();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while rt_state.0.state() != State::Ready {
        assert!(
            std::time::Instant::now() < deadline,
            "runtime never reached Ready (at {:?})",
            rt_state.0.state()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    let h = &rt_state.0;

    // main's first python_call joins it to the live set.
    rt.block_on(python_call_impl(
        &w_main,
        &reg,
        &rt_state,
        "add".into(),
        json!({"a": 1, "b": 2}),
    ))
    .expect("main's first call");
    assert!(h.fairness_snapshot().is_live(Some("main")));
    assert_eq!(h.fairness_snapshot().live_count(), 1);

    // main holds 3 of the 4 task slots: echo_later stays in flight ~30s,
    // far past the end of this test.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(
            h.call_task(
                "echo_later",
                json!({"text": "x", "delay": 30.0}),
                Some("main".into()),
                Some(false),
            )
            .expect("main's hold calls are inside its share"),
        );
    }
    wait_until(
        || h.fairness_snapshot().usage(Some("main")).1 == 3,
        "main's three echo_later tasks in flight",
    );

    // The observer polls state — events reach it but it joins nothing:
    // liveness and main's ceiling are unchanged.
    for _ in 0..2 {
        assert_eq!(
            python_state_impl(&w_obs, &reg, &rt_state),
            json!("ready"),
            "python_state answers the bridge state string"
        );
    }
    assert!(
        !h.fairness_snapshot().is_live(Some("observer")),
        "a state-only observer must not be live"
    );
    assert_eq!(h.fairness_snapshot().live_count(), 1);

    // main's 4th call still fits: its ceiling is the whole cap while the
    // observer stays unjoined.
    rt.block_on(python_call_impl(
        &w_main,
        &reg,
        &rt_state,
        "add".into(),
        json!({"a": 3, "b": 4}),
    ))
    .expect("a state-only observer reserves nothing — main keeps the full cap");

    // The observer's first python_call joins it; ceilings move.
    rt.block_on(python_call_impl(
        &w_obs,
        &reg,
        &rt_state,
        "add".into(),
        json!({"a": 1, "b": 1}),
    ))
    .expect("observer's first call");
    assert!(h.fairness_snapshot().is_live(Some("observer")));
    assert_eq!(h.fairness_snapshot().live_count(), 2);

    // main's 3 in-flight tasks now meet its two-window ceiling of 3: the
    // next call is refused window_share — the identical call that was
    // admitted a moment ago.
    let err = rt
        .block_on(python_call_impl(
            &w_main,
            &reg,
            &rt_state,
            "add".into(),
            json!({"a": 5, "b": 6}),
        ))
        .expect_err("main is at its shrunken ceiling");
    assert_eq!(err.code, proto::LIMIT_EXCEEDED);
    let data = err.data.expect("window_share carries the ceiling");
    assert_eq!(data["limit"], json!("window_share"));
    assert_eq!(data["max"], json!(3));
    assert_eq!(data["window"], json!("main"));
}
