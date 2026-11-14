//! Channel delivery lifecycle (event-scoping migration).
//!
//! Two guarantees are pinned here:
//!   1. The per-call channel receives exactly the payloads the old event
//!      bus carried — `{task, seq, value, done:false}` chunks and
//!      `{task, done:true, result|error}` terminals — unchanged.
//!   2. A dead consumer cancels the task and frees its stream slot: when
//!      `Channel::send` fails the forwarder stops and cancels.
//!
//! A webview RELOAD is the subtler dead-consumer case and is honest about
//! its coverage: `Channel::send` does NOT reliably error there — the
//! webview survives the navigation, `eval` lands in a dead JS context,
//! the callback no-ops, and `send` still returns `Ok` (see
//! `Channel::from_callback_fn`). The forwarder can only act on an actual
//! send failure, so reloads are covered server-side instead: the plugin's
//! `on_page_load` hook reaps the old document's tasks via
//! `cancel_owner_before` when a navigation commits (the client-side
//! `pagehide` hook stays as best effort). This test pins the Rust-side
//! contracts — send failure must mean per-task cancellation, and the
//! page-load watermark must spare the new document's calls.
//! (No webviews involved — runs on every OS.)

use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);

// ---- shared harness (same conventions as hardening_step0.rs) ----

fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

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
            root.join("python/pylair/src")
                .to_string_lossy()
                .into_owned(),
        )],
        cwd: Some(root.join("python/pylair/tests")),
        ..Default::default()
    }
}

fn fast_config() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(2);
    c.ping_interval = Duration::from_millis(150);
    c.missed_pongs_to_kill = 2;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 2;
    c.breaker_window = Duration::from_secs(60);
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 8;
    c.stream_buffer = 256;
    c.loop_stall_kill = None;
    c.cancel_timeout = Duration::from_secs(5);
    c.max_frame = tauri_plugin_pylair::protocol::max_frame();
    c.restart_trigger = None;
    c
}

async fn wait_state(h: &Handle, want: State) {
    let mut rx = h.subscribe_state();
    timeout(T, async {
        while *rx.borrow() != want {
            rx.changed().await.expect("state channel closed");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never reached {want:?} (at {:?})", h.state()));
}

// ---- tests ----

#[tokio::test]
async fn channel_carries_the_unchanged_payload_shape() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    let got = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let sink = got.clone();
    let ch = Channel::<serde_json::Value>::new(move |body: InvokeResponseBody| {
        let InvokeResponseBody::Json(s) = body else {
            panic!("channel payload must be Json, got Raw");
        };
        sink.lock().unwrap().push(serde_json::from_str(&s).unwrap());
        Ok(())
    });

    let (task, items) = timeout(
        T,
        h.call_stream("counter", json!({"n": 3}), Some("win-a".into()), Some(true)),
    )
    .await
    .expect("call_stream timed out")
    .unwrap();
    tauri_plugin_pylair::forward_stream(ch, items, h.clone(), task, "win-a".into())
        .await
        .expect("forwarder panicked");

    let got = got.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            json!({"task": task, "seq": 0, "value": 0, "done": false}),
            json!({"task": task, "seq": 1, "value": 1, "done": false}),
            json!({"task": task, "seq": 2, "value": 2, "done": false}),
            json!({"task": task, "done": true, "result": {"done": true}}),
        ],
        "channel payloads must match the old pylair://stream shape exactly"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn dead_channel_cancels_task_and_releases_stream_slot() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.limits.max_concurrent_streams = 1; // one slot — proves it frees
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (task, items) = timeout(
        T,
        h.call_stream(
            "infinite_counted",
            json!({}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream timed out")
    .unwrap();

    // A channel whose consumer is already gone: every send fails.
    let dead = Channel::<serde_json::Value>::new(|_body| {
        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "consumer gone").into())
    });
    timeout(
        T,
        tauri_plugin_pylair::forward_stream(dead, items, h.clone(), task, "win-a".into()),
    )
    .await
    .expect("forwarder did not exit on send failure")
    .expect("forwarder panicked");

    // The producer is infinite — a terminal tombstone can only mean the
    // forwarder's reap landed. Probe with a FOREIGN owner: `win-b` can
    // never cancel win-a's task, so a live task answers `Rejected` and
    // only a reaped one answers Completed/NotFound — the poll never
    // interferes with the outcome it observes.
    timeout(T, async {
        loop {
            match h.cancel(task, Some("win-b".into())).await {
                Ok(CancelOutcome::Completed | CancelOutcome::NotFound) => break,
                Ok(outcome) => {
                    assert_eq!(
                        outcome,
                        CancelOutcome::Rejected,
                        "foreign cancel must be rejected or see a tombstone, got {outcome:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("probe cancel errored: {e:?}"),
            }
        }
    })
    .await
    .expect("task was never reaped after the channel died");

    // ...and the stream slot is free again (the limit is 1).
    let (_t2, _items2) = timeout(
        T,
        h.call_stream(
            "infinite_counted",
            json!({}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("stream slot never released")
    .unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn dead_channel_reaps_only_that_task_not_same_owner_siblings() {
    // The forwarder used to `cancel_owner` on send failure — correct when
    // the window is truly gone, but a webview closed and reopened under
    // the SAME label could have its newer live calls killed by a stale
    // forwarder. The reap must be scoped to the dead task only.
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.limits.max_concurrent_streams = 2;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Task A gets a dead consumer; task B — same owner label — gets a
    // live one that counts deliveries.
    let (task_a, items_a) = timeout(
        T,
        h.call_stream(
            "infinite_counted",
            json!({}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream A timed out")
    .unwrap();
    let (task_b, items_b) = timeout(
        T,
        h.call_stream(
            "infinite_counted",
            json!({}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream B timed out")
    .unwrap();

    let dead = Channel::<serde_json::Value>::new(|_body| {
        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "consumer gone").into())
    });
    timeout(
        T,
        tauri_plugin_pylair::forward_stream(dead, items_a, h.clone(), task_a, "win-a".into()),
    )
    .await
    .expect("forwarder did not exit on send failure")
    .expect("forwarder panicked");

    let b_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seen = b_seen.clone();
    let live = Channel::<serde_json::Value>::new(move |_body| {
        seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    });
    let b_forwarder =
        tauri_plugin_pylair::forward_stream(live, items_b, h.clone(), task_b, "win-a".into());

    // A is reaped (foreign-owner probe: Rejected while live, tombstone
    // after) while B keeps receiving chunks the whole time.
    timeout(T, async {
        loop {
            match h.cancel(task_a, Some("win-b".into())).await {
                Ok(CancelOutcome::Completed | CancelOutcome::NotFound) => break,
                Ok(outcome) => {
                    assert_eq!(
                        outcome,
                        CancelOutcome::Rejected,
                        "foreign cancel must be rejected or see a tombstone, got {outcome:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("probe cancel errored: {e:?}"),
            }
        }
    })
    .await
    .expect("task A was never reaped after its channel died");

    // B is still live: owned by win-a, a foreign cancel is rejected and
    // its channel has been receiving real chunks all along.
    match timeout(T, h.cancel(task_b, Some("win-b".into()))).await {
        Ok(Ok(CancelOutcome::Rejected)) => {}
        other => panic!("task B must still be live and owned by win-a, got {other:?}"),
    }
    timeout(T, async {
        while b_seen.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("task B's channel must have kept receiving while A was reaped");

    // Cleanup: B's real owner may cancel it; the forwarder then ends.
    let _ = timeout(T, h.cancel(task_b, Some("win-a".into()))).await;
    let _ = timeout(T, b_forwarder).await;
    h.shutdown().await;
}

#[tokio::test]
async fn page_load_watermark_reaps_only_pre_navigation_tasks() {
    // The plugin's on_page_load hook calls cancel_owner_before(label, wm)
    // when a new document commits: tasks the OLD document owned die; a
    // call the NEW document raced in before the command lands (id >= wm)
    // survives. The hook itself needs a real webview engine — the mock
    // runtime produces no page loads — so this pins the supervisor
    // primitive it relies on.
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.limits.max_concurrent_streams = 2;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Old document's task.
    let (task_old, _items_old) = timeout(
        T,
        h.call_stream(
            "infinite_counted",
            json!({}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream old timed out")
    .unwrap();

    // PageLoadEvent::Started commits — the watermark is taken here.
    let wm = h.task_watermark();
    assert!(task_old < wm, "old task must predate the watermark");

    // The new document races a call in BEFORE the reap command lands.
    let (task_new, _items_new) = timeout(
        T,
        h.call_stream(
            "infinite_counted",
            json!({}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream new timed out")
    .unwrap();
    assert!(task_new >= wm, "new task must be at/after the watermark");

    h.cancel_owner_before("win-a".into(), wm).await;

    // Old task is reaped (foreign probe: Rejected while live, tombstone
    // after).
    timeout(T, async {
        loop {
            match h.cancel(task_old, Some("win-b".into())).await {
                Ok(CancelOutcome::Completed | CancelOutcome::NotFound) => break,
                Ok(outcome) => {
                    assert_eq!(
                        outcome,
                        CancelOutcome::Rejected,
                        "foreign cancel must be rejected or see a tombstone, got {outcome:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("probe cancel errored: {e:?}"),
            }
        }
    })
    .await
    .expect("old-document task was never reaped");

    // The raced-in new-document task survives — still owned by win-a.
    match timeout(T, h.cancel(task_new, Some("win-b".into()))).await {
        Ok(Ok(CancelOutcome::Rejected)) => {}
        other => panic!("new-document task must survive the watermark reap, got {other:?}"),
    }

    // Cleanup: its real owner may cancel it.
    let _ = timeout(T, h.cancel(task_new, Some("win-a".into()))).await;
    h.shutdown().await;
}
