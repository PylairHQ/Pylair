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
use tauri_plugin_pylair::supervisor::{SpawnSpec, State};
use tauri_plugin_pylair::{
    python_call_stream_impl, Builder, PyRuntime, Registry, SupervisorConfig,
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
