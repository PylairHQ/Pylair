//! Window-destroyed regression test.
//!
//! lib.rs's `WindowEvent::Destroyed` arm runs on Tauri's MAIN thread —
//! outside any entered Tokio runtime. The owner's cancel sweep used to
//! `tokio::spawn` internally and panicked the whole app on every window
//! close (supervisor.rs:470, found by Windows dogfood of rc.6). Drive
//! the exact call-site pattern from a bare thread: no panic, and the
//! runtime must see the cancel for the owner's task — and only that task.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);

/// Hello, pongs, answers `shutdown` — but calls get no response (they
/// stay pending) and every `cancel` prints a marker to stderr so the
/// test can prove the request physically reached the child for that task.
const FAKE_CANCEL_WATCH: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hang","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m["method"] == "cancel":
        print("CANCEL_SEEN task=%s" % m["params"]["task"], file=sys.stderr); sys.stderr.flush()
        w({"jsonrpc":"2.0","id": m["id"],"result": {"cancelled": True, "status": "terminated"}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

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

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: None,
        ..Default::default()
    }
}

fn config() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(5);
    c.ping_interval = Duration::from_millis(150);
    c.missed_pongs_to_kill = 30;
    c.backoff_base = Duration::from_millis(50);
    c.backoff_cap = Duration::from_millis(200);
    c.breaker_crashes = 5;
    c.breaker_window = Duration::from_secs(60);
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 8;
    c.stream_buffer = 256;
    c.loop_stall_kill = None;
    c.cancel_timeout = Duration::from_secs(5);
    c.max_frame = proto::max_frame();
    c.restart_trigger = None;
    c
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

#[tokio::test]
async fn window_destroyed_cancels_only_owned_tasks_from_main_thread() {
    let (tx, mut events) = mpsc::channel(256);
    let (h, actor) = spawn(spec(FAKE_CANCEL_WATCH), config(), tx);
    tokio::spawn(actor);
    wait_ready(&h).await;

    // Two calls, each owned by a different "window" — neither is ever
    // answered by the fake, so both stay pending until cancelled.
    let task_main = h
        .call_task("hang", json!({}), Some("main".into()), Some(false))
        .unwrap()
        .task
        .await
        .unwrap();
    let task_other = h
        .call_task("hang", json!({}), Some("other".into()), Some(false))
        .unwrap()
        .task
        .await
        .unwrap();

    // The lib.rs Destroyed arm, verbatim: a bare thread, no reactor
    // entered. Before the fix this expression could not even be reached —
    // cancel_owner swallowed a tokio::spawn that panicked here.
    let h2 = h.clone();
    std::thread::spawn(move || {
        tauri::async_runtime::spawn(async move { h2.cancel_owner("main".into()).await });
    })
    .join()
    .expect("window-close path panicked outside a runtime — the rc.3 bug");

    // The runtime must observe cancel for the owned task only: the
    // destroy must not leak into another window's work.
    timeout(T, async {
        let mut saw_main = false;
        while let Some(ev) = events.recv().await {
            let RuntimeEvent::Stderr(line) = ev else {
                continue;
            };
            assert!(
                !line.contains(&format!("CANCEL_SEEN task={task_other}")),
                "cancel escaped owner scoping into another window's task"
            );
            if line.contains(&format!("CANCEL_SEEN task={task_main}")) {
                saw_main = true;
                break;
            }
        }
        assert!(saw_main, "events channel closed before the cancel landed");
    })
    .await
    .expect("no cancel request reached the runtime for the owned task");

    h.shutdown().await;
}
