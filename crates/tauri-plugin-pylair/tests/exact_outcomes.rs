//! C (RC.8): every hostile race resolves to an EXPLICIT, named outcome —
//! never "returns something", never an ambiguous bool.
//!
//!   1. cancel with the data lane saturated and the runtime wedged
//!      -> Err(RUNTIME_WEDGED): unacknowledged within cancel_timeout
//!   2. cancel after the pipe stopped accepting writes
//!      -> Ok(DeliveryFailed): the control lane is dead, loudly
//!   3. cancel that overtakes the call into _pending_cancels
//!      -> Cancelled|Completed + the stream still ends TASK_CANCELLED
//!   4. cancel while the Python event loop is blocked in native code
//!      -> Err(RUNTIME_WEDGED): the loop cannot dispatch the ack
//!   5. shutdown while a task is mid-flight
//!      -> ShutdownOutcome::Terminated + the task ends TASK_CANCELLED

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, RuntimeEvent, ShutdownOutcome, SpawnSpec, State, StreamItem,
    SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

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
            root.join("python/pylair/src")
                .to_string_lossy()
                .into_owned(),
        )],
        cwd: Some(root.join("python/pylair/tests")),
        ..Default::default()
    }
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

/// Hello, pongs pings — then never reads stdin again. The data lane and
/// the OS pipe fill; ctrl frames are written but never read.
const FAKE_WEDGED: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hang","kind":"sync"}]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time
time.sleep(600)
"#;

/// Hello, answers pings and reads ONE call (leaves it pending), then
/// CLOSES its stdin: the host's next write dies, the writer task exits,
/// every later control send reports the dead lane — while the child
/// stays alive so the supervisor keeps the session.
const FAKE_PIPE_CLOSER: &str = r#"
import sys, struct, json, os, time
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hang","kind":"sync"}]}})
h = sys.stdin.buffer.read(4)          # one frame: the call below
n, = struct.unpack(">I", h)
sys.stdin.buffer.read(n)              # body (the call — never answered)
os.close(0)                           # pipe stops accepting writes NOW
time.sleep(600)
"#;

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
    c.initial_stream_credit = 8;
    c.stream_buffer = 256;
    c.loop_stall_kill = None;
    c.cancel_timeout = Duration::from_secs(2); // bounded, explicit
    c.max_frame = proto::max_frame();
    c.restart_trigger = None;
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

/// 1. Data lane saturated + runtime wedged: the cancel frame is delivered
///    to the pipe but never read — the outcome is the bounded wedge
///    error, not a silent drop and not "cancelled".
#[tokio::test]
async fn cancel_with_data_lane_saturated_is_wedged_error() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(FAKE_WEDGED), cfg(), ev);
    wait_ready(&h).await;

    let ct = h.call_task("hang", json!({}), None, Some(false)).unwrap();
    let task = timeout(T, ct.task).await.unwrap().unwrap();

    // Saturate the data lane: the wedged runtime reads nothing, so the
    // 64-deep lane + the pipe buffer fill behind it.
    let big = json!({"pad": "x".repeat(64 * 1024)});
    for _ in 0..64 {
        if h.call_task("hang", big.clone(), None, Some(false)).is_err() {
            break;
        }
    }

    // A cancel racing the still-full command queue gets its own explicit
    // answer — LIMIT_EXCEEDED{max_queued_commands}. Retry through that
    // transient to reach the interesting outcome: the delivered,
    // never-acked cancel bounded by cancel_timeout.
    let res = timeout(T, async {
        loop {
            match h.cancel(task, None).await {
                Err(e) if e.code == proto::LIMIT_EXCEEDED => {
                    tokio::task::yield_now().await;
                    continue;
                }
                r => return r,
            }
        }
    })
    .await
    .expect("cancel itself must complete")
    .expect_err("an unacknowledged cancel is an error, never 'cancelled'");
    assert_eq!(
        res.code,
        proto::RUNTIME_WEDGED,
        "saturated-lane cancel reported {} ({}), not RUNTIME_WEDGED",
        res.code,
        res.message
    );
    h.shutdown().await;
}

/// 2. Pipe stops accepting writes: the writer task dies with it.
///    Cancels right after the break can still ride the open lane and
///    wedge (POSIX can accept a write into the kernel pipe buffer before
///    the close is recorded — delivered-but-unreadable is wedged); once
///    a write returns EPIPE the lane is dead and every later control
///    send reports DeliveryFailed. How many cancels squeeze in first is
///    platform timing, so cancel until the verdict — every intermediate
///    answer must also be a named outcome, never a hang.
#[tokio::test]
async fn cancel_after_pipe_dies_is_delivery_failed() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(FAKE_PIPE_CLOSER), cfg(), ev);
    wait_ready(&h).await;

    // The fake reads exactly one frame — this call's frame — then closes
    // its stdin, leaving the call pending in `tasks`.
    let ct = h.call_task("hang", json!({}), None, Some(false)).unwrap();
    let task = timeout(T, ct.task).await.unwrap().unwrap();

    let mut saw_wedged = false;
    let reached = timeout(T, async {
        loop {
            match h.cancel(task, None).await {
                Ok(CancelOutcome::DeliveryFailed) => break,
                Err(e) => {
                    assert_eq!(
                        e.code,
                        proto::RUNTIME_WEDGED,
                        "delivered-but-unacked cancel is RUNTIME_WEDGED, \
                         not another code: {e:?}"
                    );
                    saw_wedged = true;
                }
                other => panic!("dead-pipe cancel reported {other:?}"),
            }
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "cancel never reached DeliveryFailed on a dead pipe"
    );
    assert!(
        saw_wedged,
        "expected at least one cancel to take the wedge path"
    );

    // Shutdown still terminates the tree (the child is alive; it just
    // can't hear us) — a forced kill, reported as such.
    let out = timeout(T, h.shutdown()).await.expect("shutdown timed out");
    assert!(
        matches!(
            out,
            ShutdownOutcome::ForcedKill | ShutdownOutcome::Terminated
        ),
        "shutdown on a dead-pipe child reported {out:?}"
    );
}

/// 3. Cancel outruns registration: control frames can overtake the data
///    lane, so Python may see `cancel` before `call`. The task is held in
///    _pending_cancels and cancelled at registration — and its call's
///    terminal MUST still arrive (a pre-first-step task emits nothing
///    without the finisher; RC.8B fixed that hang). Either way the race
///    goes, the cancel's outcome is `Cancelled`: a live task reports
///    cancelled:true, an unregistered one reports status:"pending" —
///    the runtime records it and applies it at registration, so it is
///    never reported Completed.
#[tokio::test]
async fn cancel_that_outruns_registration_still_terminates_stream() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_ready(&h).await;

    let (task, mut rx) = h
        .call_stream("infinite_counted", json!({}), Some("w".into()), Some(true))
        .await
        .expect("call_stream rejected");
    let out = timeout(T, h.cancel(task, Some("w".into())))
        .await
        .expect("cancel must complete")
        .expect("cancel outcome is named");
    assert_eq!(
        out,
        CancelOutcome::Cancelled,
        "racing cancel reported {out:?} — pending registration must \
         still report Cancelled, never Completed"
    );

    // Whichever way the race went, the caller's terminal is the SAME
    // explicit outcome: the task never ran unobserved.
    let end = timeout(T, async {
        loop {
            match rx.recv().await {
                Some(StreamItem::End(r)) => return r,
                Some(_) => continue,
                None => panic!("stream closed without a terminal"),
            }
        }
    })
    .await
    .expect("stream never terminated after the racing cancel");
    let e = end.expect_err("a cancelled stream ends in an error");
    assert_eq!(
        e.code,
        proto::TASK_CANCELLED,
        "racing-cancel stream ended {} ({}) — expected TASK_CANCELLED",
        e.code,
        e.message
    );
    h.shutdown().await;
}

/// 4. Python's event loop blocked in native code: pongs keep flowing
///    (reader thread) but `cancel` is dispatched ON the loop — no ack can
///    come. The bounded outcome is the wedge error at cancel_timeout.
#[tokio::test]
async fn cancel_while_python_loop_blocked_is_wedged_error() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_ready(&h).await;

    // block_loop runs time.sleep INSIDE the async def — the event loop
    // stops dispatching but the reader thread keeps answering pings.
    let ct = h
        .call_task("block_loop", json!({"seconds": 30}), None, Some(false))
        .unwrap();
    let task = timeout(T, ct.task).await.unwrap().unwrap();
    sleep(Duration::from_millis(300)).await; // let the block actually start

    let res = timeout(T, h.cancel(task, None))
        .await
        .expect("cancel must complete")
        .expect_err("a loop-blocked runtime cannot ack — error, not 'cancelled'");
    assert_eq!(
        res.code,
        proto::RUNTIME_WEDGED,
        "loop-blocked cancel reported {} ({}), not RUNTIME_WEDGED",
        res.code,
        res.message
    );
    h.shutdown().await;
}

/// 5. Shutdown while a task is producing: the runtime drains (cancels the
///    task, flushes its terminal, then exits) inside the grace window —
///    Terminated, and the task's own reported end is TASK_CANCELLED, not
///    a crash or a hang.
#[tokio::test]
async fn shutdown_mid_stream_reports_terminated_and_cancelled() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_ready(&h).await;

    let (_task, mut rx) = h
        .call_stream("infinite_counted", json!({}), None, Some(true))
        .await
        .expect("call_stream rejected");

    let out = timeout(T, h.shutdown())
        .await
        .expect("shutdown must complete");
    assert_eq!(
        out,
        ShutdownOutcome::Terminated,
        "mid-flight shutdown reported {out:?}, not Terminated"
    );

    let end = timeout(T, async {
        loop {
            match rx.recv().await {
                Some(StreamItem::End(r)) => return r,
                Some(_) => continue,
                None => panic!("stream closed without a terminal"),
            }
        }
    })
    .await
    .expect("stream never terminated across shutdown");
    let e = end.expect_err("a shutdown-cancelled stream ends in an error");
    assert_eq!(
        e.code,
        proto::TASK_CANCELLED,
        "stream across shutdown ended {} ({}), not TASK_CANCELLED",
        e.code,
        e.message
    );
}
