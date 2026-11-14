//! B (RC.8): lifecycle stress — 200 generations of the same window label.
//!
//! Each iteration: the "window" starts streaming work, is destroyed
//! (`cancel_owner`, the same path lib.rs runs on WindowEvent::Destroyed),
//! and a new window with the SAME label starts a fresh stream. Asserts:
//!   - every destroyed task's stream ends with an explicit outcome —
//!     TASK_CANCELLED, or its own result if it finished first;
//!   - the new generation's stream starts at seq 0 — no stale event from
//!     the dead generation bleeds into the same-label successor;
//!   - nothing leaks: max_concurrent_tasks is kept at 4, so a task that
//!     survived its window's destruction trips the cap within a few
//!     generations and fails the test loudly.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);
const GENS: usize = 200;

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

/// The real pylair runtime against the real testapp.
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
    c.hello_timeout = Duration::from_secs(5);
    c.ping_interval = Duration::from_secs(60); // no liveness interference
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 2;
    c.breaker_window = Duration::from_secs(60);
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 8; // produce a few chunks then stall
    c.stream_buffer = 256;
    c.loop_stall_kill = None;
    c.cancel_timeout = Duration::from_secs(10);
    c.max_frame = proto::max_frame();
    c.restart_trigger = None;
    // Leak tripwire: a task that survives its window's destruction trips
    // the cap on a later generation instead of leaking silently.
    c.limits.max_concurrent_tasks = 4;
    c.limits.max_concurrent_streams = 4;
    c
}

fn spawn_h(spec: SpawnSpec, c: SupervisorConfig, ev: mpsc::Sender<RuntimeEvent>) -> Handle {
    let (h, actor) = spawn(spec, c, ev);
    tokio::spawn(actor);
    h
}

async fn wait_ready(h: &Handle) {
    let mut rx = h.subscribe_state();
    timeout(T, async {
        while *rx.borrow() != State::Ready {
            rx.changed().await.expect("state channel closed");
        }
    })
    .await
    .expect("runtime never reached Ready");
}

/// Drain until the terminal arrives; return its exact result.
async fn terminal(
    rx: &mut tauri_plugin_pylair::supervisor::StreamRx,
    gen: usize,
) -> Result<serde_json::Value, tauri_plugin_pylair::protocol::ErrorObj> {
    let mut n = 0;
    loop {
        match timeout(T, rx.recv()).await.unwrap_or_else(|_| {
            panic!("gen {gen}: stream never produced a terminal ({n} chunks drained)")
        }) {
            Some(StreamItem::End(r)) => return r,
            Some(StreamItem::Chunk { .. }) => {
                n += 1;
                continue;
            }
            None => panic!("gen {gen}: stream closed without a terminal"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_window_generations_no_leaks_no_stale_events() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_ready(&h).await;

    let label = "win".to_string();
    for gen in 0..GENS {
        // Create: streaming work owned by this window generation.
        let (task, mut rx) = h
            .call_stream(
                "infinite_counted",
                json!({}),
                Some(label.clone()),
                Some(true),
            )
            .await
            .unwrap_or_else(|e| panic!("gen {gen}: call_stream rejected: {e:?}"));

        // Destroy: the sweep must resolve the task's stream with an
        // explicit outcome — cancelled, or its own result if it finished.
        h.cancel_owner(label.clone()).await;
        // Ok means the task finished before the destroy — explicitly
        // reported too.
        if let Err(e) = terminal(&mut rx, gen).await {
            assert_eq!(
                e.code,
                proto::TASK_CANCELLED,
                "gen {gen}: task {task} ended with {} ({}), not an explicit \
                 cancellation",
                e.code,
                e.message
            );
        }

        // New window, SAME label: its stream must be a clean generation —
        // first event is seq 0 of a NEW task, never a stale chunk.
        let (task2, mut rx2) = h
            .call_stream(
                "infinite_counted",
                json!({}),
                Some(label.clone()),
                Some(true),
            )
            .await
            .unwrap_or_else(|e| panic!("gen {gen}: next-generation stream rejected: {e:?}"));
        assert_ne!(task, task2, "gen {gen}: task id reused across generations");
        match timeout(T, rx2.recv())
            .await
            .unwrap_or_else(|_| panic!("gen {gen}: new stream produced nothing"))
        {
            Some(StreamItem::Chunk { seq, value }) => {
                assert_eq!(
                    seq, 0,
                    "gen {gen}: first chunk seq {seq}, not 0 — stale event"
                );
                assert_eq!(value, json!(0), "gen {gen}: first chunk value not fresh");
            }
            Some(StreamItem::End(r)) => {
                panic!("gen {gen}: new stream ended before producing: {r:?}")
            }
            None => panic!("gen {gen}: new stream closed without events"),
        }

        // This generation is destroyed too — the next iteration is a new
        // window again.
        h.cancel_owner(label.clone()).await;
        if let Err(e) = terminal(&mut rx2, gen).await {
            assert_eq!(
                e.code,
                proto::TASK_CANCELLED,
                "gen {gen}: successor task {task2} ended with {} ({})",
                e.code,
                e.message
            );
        }
    }
    h.shutdown().await;
}

/// F3 (rc.15): respawns reuse ONE pdeathsig anchor thread. Pre-fix each
/// spawn parked a fresh `pylair-pdeath-anchor` thread forever, so crash
/// and retry respawns stacked threads (~8 MiB of reserved stack each)
/// that never unwound. Deltas, not absolute counts — other tests in this
/// process may hold anchors of their own.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn respawns_reuse_one_pdeath_anchor_thread() {
    use tauri_plugin_pylair::supervisor::pdeath_anchor_threads;

    /// Hello, then die — every generation crashes fast and respawns.
    const FAKE_DIE_AFTER_HELLO: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
sys.exit(3)
"#;
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut c = cfg();
    c.breaker_crashes = 50; // let generations cycle; never park in Failed
    let before = pdeath_anchor_threads();
    let h = spawn_h(
        SpawnSpec {
            program: python(),
            args: vec!["-c".into(), FAKE_DIE_AFTER_HELLO.into()],
            env: vec![],
            cwd: None,
            ..Default::default()
        },
        c,
        ev,
    );

    // First spawn brings the anchor up.
    timeout(T, async {
        while pdeath_anchor_threads() < before + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first spawn never created the anchor thread");

    // Generations cycle in ~spawn-time plus a 20-80ms backoff — three
    // seconds is many respawns even on a slow runner.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        pdeath_anchor_threads(),
        before + 1,
        "respawns stacked anchor threads instead of reusing one"
    );

    // Actor exit drops the job channel; the anchor thread exits too.
    h.shutdown().await;
    timeout(T, async {
        while pdeath_anchor_threads() > before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("anchor thread outlived its supervisor");
}
