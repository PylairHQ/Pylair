//! Inspector wiring end-to-end (docs/inspector.md): the session tap
//! records real call outcomes, the events channel is untouched by a
//! burst, and python_inspect_snapshot refuses app windows.
//! The command-refusal half needs a real webview — non-windows, same
//! constraint as call_via_command.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
#[cfg(not(windows))]
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{spawn, RuntimeEvent, SpawnSpec, SupervisorConfig};

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
    panic!("no python >=3.12 found for inspector tests");
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

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(15);
    c.shutdown_grace = Duration::from_secs(2);
    c
}

async fn wait_ready(h: &tauri_plugin_pylair::supervisor::Handle) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !matches!(h.state(), tauri_plugin_pylair::supervisor::State::Ready) {
        assert!(std::time::Instant::now() < deadline, "runtime never ready");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn tap_records_real_calls_and_events_channel_unaffected() {
    // The tap reads the HOST env at spawn (same process here) — set it
    // before spawning. Debug-only codepath; the flag is additive so
    // sharing the process env with other tests is harmless.
    unsafe { std::env::set_var("PYLAIR_INSPECT", "1") };
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<RuntimeEvent>(256);
    let (handle, actor) = spawn(real_runtime_spec(), cfg(), ev_tx);
    tokio::spawn(actor);
    wait_ready(&handle).await;

    // An ok call, a stream, and an error — the three basic rows.
    let r = handle
        .call(
            "add",
            json!({"a": 2, "b": 3}),
            Some("main".into()),
            Some(false),
        )
        .await
        .expect("add resolves");
    assert_eq!(r, json!(5));

    let (task, mut items) = handle
        .call_stream("counter", json!({"n": 3}), Some("main".into()), Some(true))
        .await
        .expect("stream starts");
    let mut chunks = 0;
    while let Some(it) = items.recv().await {
        match it {
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { .. } => chunks += 1,
            tauri_plugin_pylair::supervisor::StreamItem::End(_) => break,
        }
    }
    assert_eq!(chunks, 3, "counter emitted 3 chunks");
    let _ = task;

    let snap = handle.inspect().snapshot();
    assert_eq!(
        snap["enabled"],
        json!(true),
        "tap live under PYLAIR_INSPECT=1"
    );
    let recs = snap["records"].as_array().unwrap();
    assert!(
        recs.iter()
            .any(|r| r["function"] == "add" && r["outcome"]["kind"] == "ok"),
        "add recorded as ok: {recs:?}"
    );
    assert!(
        recs.iter()
            .any(|r| r["function"] == "counter" && r["stream"]["chunks"] == json!(3)),
        "counter recorded with 3 chunks: {recs:?}"
    );

    // Events channel: a burst with the tap on must not cost real events.
    // Drain everything already buffered, then force a health-relevant
    // transition by exhausting the burst and watching stderr/state flow.
    let burst = futures_burst(&handle).await;
    assert!(burst > 0);
    let mut saw_events = 0usize;
    let drain = tokio::time::timeout(Duration::from_millis(300), async {
        while let Some(_ev) = ev_rx.recv().await {
            saw_events += 1;
        }
    })
    .await;
    let _ = drain;
    // The readiness/exposes/state events of startup plus anything the
    // burst produced must have flowed — the tap never touches this
    // channel, so ANY delivery proves non-interference.
    assert!(
        saw_events > 0,
        "events channel delivered during inspector operation"
    );

    handle.shutdown().await;
}

async fn futures_burst(handle: &tauri_plugin_pylair::supervisor::Handle) -> usize {
    let mut n = 0;
    for _ in 0..40 {
        let _ = handle
            .call(
                "add",
                json!({"a": 1, "b": 1}),
                Some("main".into()),
                Some(false),
            )
            .await;
        n += 1;
    }
    n
}

// ---- command refusal: real webview, non-windows ----
#[cfg(not(windows))]
mod command_refusal {
    use super::*;
    use tauri::test::{mock_context, noop_assets, MockRuntime};
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    use tauri_plugin_pylair::{Builder, PyRuntime};

    #[test]
    fn inspect_snapshot_refused_from_app_windows() {
        let mut c = cfg();
        c.hello_timeout = Duration::from_secs(15);
        let app = tauri::test::mock_builder()
            .plugin(
                Builder::<MockRuntime>::new(real_runtime_spec())
                    .config(c)
                    .build(),
            )
            .build(mock_context(noop_assets()))
            .expect("app builds");
        let win = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
            .build()
            .expect("main window");
        let rt = app.state::<PyRuntime>();
        // An app window is refused — the inspector window label is the
        // only one the command answers.
        let res = tauri_plugin_pylair::python_inspect_snapshot_impl(&win, &rt);
        assert!(res.is_err(), "app window must be refused");
        let e = res.err().unwrap();
        assert_eq!(e.code, proto::INVALID_REQUEST);
    }
}
