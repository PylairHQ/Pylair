//! Depth cap: the host parses frames with serde_json, which refuses
//! JSON nested deeper than 128 — a legitimate deep Python result was
//! indistinguishable from a protocol violation and killed the WHOLE
//! runtime, failing every in-flight call. The runtime must check depth
//! before sending and fail only that call (and that stream chunk).

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(20);

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
    panic!("no python >=3.12 found for supervisor tests");
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
            format!(
                "{}{}{}",
                root.join("python/pylair/src").display(),
                if cfg!(windows) { ";" } else { ":" },
                root.join("python/pylair/tests").display()
            ),
        )],
        cwd: Some(root.join("python/pylair/tests")),
        ..Default::default()
    }
}

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(10);
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.breaker_crashes = 10;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c
}

fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

async fn wait_ready(h: &Handle) {
    timeout(T, async {
        let mut rx = h.subscribe_state();
        while *rx.borrow() != State::Ready {
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("runtime never reached Ready");
}

/// deep(200) is a legal Python result but an unparseable frame: the
/// runtime must fail THIS call with the typed depth error while a
/// concurrent call succeeds — and the runtime must stay alive after.
#[tokio::test]
async fn deep_result_fails_one_call_and_runtime_survives() {
    let (tx, _rx) = mpsc::channel(256);
    let h = spawn_h(real_runtime_spec(), cfg(), tx);
    wait_ready(&h).await;

    let (deep, add) = tokio::join!(
        h.call("deep", json!({"n": 200}), None, Some(false)),
        h.call("add", json!({"a": 2, "b": 3}), None, Some(false)),
    );
    match deep {
        Err(e) => assert_eq!(e.code, proto::LIMIT_EXCEEDED, "wrong code: {e:?}"),
        Ok(v) => panic!("200-deep result reached the wire: {}", &v.to_string()[..60]),
    }
    assert_eq!(add.expect("concurrent call failed"), Value::from(5));

    // The runtime survived the malformed-response attempt.
    let again = timeout(T, h.call("add", json!({"a": 1, "b": 1}), None, Some(false)))
        .await
        .expect("runtime died — call never returned")
        .expect("post-depth call failed");
    assert_eq!(again, Value::from(2));
    h.shutdown().await;
}

/// A deep STREAM chunk fails the stream's terminal, not the runtime.
#[tokio::test]
async fn deep_stream_chunk_fails_the_stream_only() {
    let (tx, _rx) = mpsc::channel(256);
    let h = spawn_h(real_runtime_spec(), cfg(), tx);
    wait_ready(&h).await;

    let (_task, mut rx) = h
        .call_stream(
            "deep_stream",
            json!({"n": 200}),
            Some("w".into()),
            Some(true),
        )
        .await
        .expect("call_stream rejected");
    let end = timeout(T, async {
        loop {
            match rx.recv().await {
                Some(StreamItem::End(e)) => return e,
                Some(StreamItem::Chunk { .. }) => {}
                None => panic!("stream channel closed without End"),
            }
        }
    })
    .await
    .expect("stream never terminated");
    let e = end.expect_err("deep chunk must end the stream in error");
    assert_eq!(e.code, proto::LIMIT_EXCEEDED, "wrong code: {e:?}");

    let ok = timeout(T, h.call("add", json!({"a": 4, "b": 1}), None, Some(false)))
        .await
        .expect("runtime died after deep chunk")
        .expect("post-chunk call failed");
    assert_eq!(ok, Value::from(5));
    h.shutdown().await;
}

/// The isolated-worker path reports the same typed error (worker-side
/// check or the parent's — either way the call fails, the runtime lives).
#[tokio::test]
async fn deep_isolated_result_fails_the_call_only() {
    let (tx, _rx) = mpsc::channel(256);
    let h = spawn_h(real_runtime_spec(), cfg(), tx);
    wait_ready(&h).await;

    match timeout(
        T,
        h.call("isolated_deep", json!({"n": 200}), None, Some(false)),
    )
    .await
    .expect("isolated call never returned")
    {
        Err(e) => assert_eq!(e.code, proto::LIMIT_EXCEEDED, "wrong code: {e:?}"),
        Ok(v) => panic!(
            "200-deep isolated result reached the wire: {}",
            &v.to_string()[..60]
        ),
    }
    let ok = timeout(
        T,
        h.call("isolated_add", json!({"a": 1, "b": 2}), None, Some(false)),
    )
    .await
    .expect("runtime died after deep isolated result")
    .expect("post-depth isolated call failed");
    assert_eq!(ok, Value::from(3));
    h.shutdown().await;
}
