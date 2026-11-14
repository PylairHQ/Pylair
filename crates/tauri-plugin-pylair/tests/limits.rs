//! RC.8A: every configured limit returns an explicit, typed error naming
//! the limit (`LIMIT_EXCEEDED` + `data.limit`), the system recovers once
//! load drains, and nothing grows without bound.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, Limits, RuntimeEvent, SpawnSpec, State, StreamItem,
    SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(20);

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

/// The REAL runtime: `python -m pylair testapp`.
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

/// Well-behaved: hello, echo calls, pong pings, exit on shutdown.
const FAKE_READY: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif "id" in m:
        w({"jsonrpc":"2.0","id": m["id"],"result": m["params"].get("input")})
"#;

/// Hello, then never reads stdin again — the data and control lanes fill.
const FAKE_WEDGED: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time
time.sleep(600)
"#;

fn fast_config() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(5);
    c.ping_interval = Duration::from_secs(60); // no liveness interference
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 2;
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 8;
    c.cancel_timeout = Duration::from_secs(5);
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

fn assert_limit(err: &tauri_plugin_pylair::ErrorObj, limit: &str) {
    assert_eq!(err.code, proto::LIMIT_EXCEEDED, "wrong code: {err:?}");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some(limit),
        "wrong limit name: {err:?}"
    );
}

// ---- max_concurrent_tasks ----

#[tokio::test]
async fn task_cap_rejects_then_recovers() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits.max_concurrent_tasks = 2;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Two in-flight tasks.
    let ct1 = h
        .call_task("sleep_forever", json!({}), None, Some(false))
        .unwrap();
    let ct2 = h
        .call_task("sleep_forever", json!({}), None, Some(false))
        .unwrap();
    // ct.task resolves when the actor registers the task — the cap is
    // counted on the Rust side, so this IS the registration barrier.
    let t1 = timeout(T, ct1.task).await.unwrap().unwrap();
    let t2 = timeout(T, ct2.task).await.unwrap().unwrap();

    let err = h
        .call("add", json!({"a": 1, "b": 2}), None, Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "max_concurrent_tasks");

    // Drain: cancel one task. Awaiting the cancelled task's own terminal
    // (not just the cancel ack) is what retires it from the task count.
    let oc = h.cancel(t1, None).await.unwrap();
    assert_eq!(oc, CancelOutcome::Cancelled);
    let _ = timeout(T, ct1.result).await;
    let out = h
        .call("add", json!({"a": 1, "b": 2}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!(3));

    let _ = h.cancel(t2, None).await;
    h.shutdown().await;
}

// ---- max_concurrent_streams ----

#[tokio::test]
async fn stream_cap_rejects_second_stream() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits.max_concurrent_streams = 1;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (task, mut items) = h
        .call_stream("infinite", json!({}), None, Some(true))
        .await
        .unwrap();
    let err = h
        .call_stream("infinite", json!({}), None, Some(true))
        .await
        .err()
        .unwrap();
    assert_limit(&err, "max_concurrent_streams");

    // The first stream is unaffected and still produces.
    let first = timeout(T, items.recv()).await.unwrap().unwrap();
    assert!(matches!(first, StreamItem::Chunk { .. }));
    let _ = h.cancel(task, None).await;
    h.shutdown().await;
}

// ---- max_queued_commands ----

#[tokio::test]
async fn full_command_queue_is_limit_error_not_stopped() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits.max_queued_commands = 1;
    cfg.shutdown_grace = Duration::from_secs(3);
    let h = spawn_h(spec(FAKE_WEDGED), cfg, ev);
    wait_state(&h, State::Ready).await;

    // The wedged runtime never exits on `shutdown`, so once the spawned
    // shutdown command is dequeued the actor parks in the grace window —
    // a guaranteed queue-not-draining window. The sleep gives the spawn
    // a head start on that dequeue; it only has to beat the 3s grace
    // period, which it does by a wide margin. (No observable exists for
    // "actor parked on shutdown" — this is the one wall-clock wait left.)
    let sh = h.clone();
    tokio::spawn(async move { sh.shutdown().await });
    sleep(Duration::from_millis(200)).await;

    // Capacity 1: the first call queues, the second MUST fail with
    // LIMIT_EXCEEDED{limit:"max_queued_commands"} — not "runtime is stopped".
    let first = h.call_task("x", json!({}), None, Some(false));
    let err = match first {
        Err(e) => e,
        Ok(_) => h
            .call_task("x", json!({}), None, Some(false))
            .err()
            .unwrap(),
    };
    assert_limit(&err, "max_queued_commands");
}

// ---- max_stream_credit ----

/// A fake runtime that registers the stream but NEVER produces — its
/// outstanding credit stays at the initial grant, which makes the
/// cap arithmetic deterministic (a real producer consumes credit
/// continuously and races the assertion).
const FAKE_HOLD_STREAM: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hold","kind":"generator"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

#[tokio::test]
async fn credit_cap_rejects_overgrant() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits.max_stream_credit = 8;
    cfg.initial_stream_credit = 4;
    let h = spawn_h(spec(FAKE_HOLD_STREAM), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (task, _items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    // Outstanding = initial grant = 4 (the fake never produces).
    // 4 + 4 == 8: allowed, exactly at the cap.
    h.grant_credit(task, 4, None).await.unwrap();
    // 8 + 1 > 8: rejected, names the limit.
    let err = h.grant_credit(task, 1, None).await.err().unwrap();
    assert_limit(&err, "max_stream_credit");
    // A single oversized grant is rejected the same way.
    let err = h.grant_credit(task, 100, None).await.err().unwrap();
    assert_limit(&err, "max_stream_credit");

    let _ = h.cancel(task, None).await;
    h.shutdown().await;
}

// ---- stream_buffer_bytes (per stream) ----

/// The cap trip emits a Stderr diagnostic from the actor — waiting for
/// it is the event-driven form of "let the buffer fill": no sleeps,
/// and it can't fire before the trip actually happens.
async fn wait_for_diag(rx: &mut mpsc::Receiver<RuntimeEvent>, needle: &str) {
    timeout(T, async {
        while let Some(e) = rx.recv().await {
            if let RuntimeEvent::Stderr(l) = e {
                if l.contains(needle) {
                    return;
                }
            }
        }
        panic!("event channel closed before {needle:?}");
    })
    .await
    .unwrap_or_else(|_| panic!("diagnostic {needle:?} never arrived"));
}

#[tokio::test]
async fn per_stream_byte_cap_ends_stream_with_limit_error() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits.stream_buffer_bytes = 128; // ~30 int chunks
    cfg.initial_stream_credit = 100_000;
    cfg.stream_buffer = 100_000; // count cap must NOT trip first
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (task, mut items) = h
        .call_stream("infinite_counted", json!({}), None, Some(true))
        .await
        .unwrap();
    // Don't drain while the flood accumulates — every recv releases a
    // byte lease, so draining first would hide the cap. The diagnostic
    // fires exactly when the cap trips.
    wait_for_diag(&mut rx, "over the byte cap").await;
    let mut end = None;
    let mut chunks = 0;
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        match item {
            StreamItem::Chunk { .. } => chunks += 1,
            StreamItem::End(r) => {
                end = Some(r);
                break;
            }
        }
    }
    assert!(chunks > 0 && chunks < 10_000, "chunks={chunks}");
    let err = end.expect("no terminal").unwrap_err();
    assert_limit(&err, "stream_buffer_bytes");

    // Producer was cancelled, supervisor still serves calls.
    let out = h
        .call("add", json!({"a": 2, "b": 3}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!(5));
    let _ = h.cancel(task, None).await;
    h.shutdown().await;
}

// ---- stream_buffer_bytes_total ----

#[tokio::test]
async fn total_byte_cap_trips_across_streams() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    // The total cap can only bind BELOW the per-stream cap (validation
    // raises total >= per) — so it takes multiple streams jointly
    // overfilling the shared budget while each stays under its own.
    cfg.limits.stream_buffer_bytes = 8 * 1024;
    cfg.limits.stream_buffer_bytes_total = 10 * 1024;
    cfg.initial_stream_credit = 100_000;
    cfg.stream_buffer = 100_000;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (_t1, mut s1) = h
        .call_stream("infinite_counted", json!({}), None, Some(true))
        .await
        .unwrap();
    let (_t2, mut s2) = h
        .call_stream("infinite_counted", json!({}), None, Some(true))
        .await
        .unwrap();
    let (_t3, mut s3) = h
        .call_stream("infinite_counted", json!({}), None, Some(true))
        .await
        .unwrap();
    // Don't drain before the trip — the diagnostic fires exactly then.
    wait_for_diag(&mut rx, "over the shared byte cap").await;
    // Whichever stream's chunk pushes the shared total over the cap gets
    // the terminal; assert at least one names the shared limit.
    let mut saw = false;
    for items in [&mut s1, &mut s2, &mut s3] {
        while let Ok(Some(item)) = timeout(T, items.recv()).await {
            if let StreamItem::End(r) = item {
                if let Err(e) = r {
                    if e.code == proto::LIMIT_EXCEEDED
                        && e.data.as_ref().and_then(|d| d["limit"].as_str())
                            == Some("stream_buffer_bytes_total")
                    {
                        saw = true;
                    }
                }
                break;
            }
        }
    }
    assert!(saw, "no stream hit stream_buffer_bytes_total");
    h.shutdown().await;
}

// ---- max_frame on the WRITE path ----

#[tokio::test]
async fn oversized_call_fails_limit_not_runtime() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.max_frame = 1024;
    let h = spawn_h(spec(FAKE_READY), cfg, ev);
    wait_state(&h, State::Ready).await;

    // A call whose serialized frame exceeds max_frame must fail THIS call
    // with LIMIT_EXCEEDED — not trip the peer's read cap and kill the runtime.
    let big = "x".repeat(4 * 1024);
    let err = h
        .call("echo", json!({"input": big}), None, Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "max_frame");

    // Runtime survived: normal calls still work.
    let out = h
        .call("echo", json!({"input": "ok"}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!({"input": "ok"}));
    h.shutdown().await;
}

// ---- validation & effective-limits logging ----

#[tokio::test]
async fn absurd_limits_are_clamped_and_logged() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits = Limits::default()
        .with_max_concurrent_tasks(0)
        .with_max_queued_commands(0)
        .with_stream_buffer_bytes_total(1);
    cfg.initial_stream_credit = 999_999;
    cfg.limits.max_stream_credit = 4;
    let h = spawn_h(spec(FAKE_READY), cfg, ev);
    wait_state(&h, State::Ready).await;

    let mut lines = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::Stderr(l) = e {
            lines.push(l);
        }
    }
    let joined = lines.join("\n");
    assert!(joined.contains("pylair limits:"), "no limits log: {joined}");
    assert!(joined.contains("max_concurrent_tasks=0"), "{joined}");
    assert!(joined.contains("initial_stream_credit"), "{joined}");

    // Clamps actually applied: a call still works (task cap >= 1).
    let out = h
        .call("echo", json!({"ok": true}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!({"ok": true}));
    h.shutdown().await;
}

// ---- Python-enforced: max_isolated_workers ----

#[tokio::test]
async fn isolated_worker_cap_is_typed_error() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let cfg = fast_config();
    let mut spec = real_runtime_spec();
    spec.env
        .push(("PYLAIR_MAX_ISOLATED_WORKERS".into(), "2".into()));
    let h = spawn_h(spec, cfg, ev);
    wait_state(&h, State::Ready).await;

    // Three concurrent isolated calls, cap 2.
    let r1 = h.call("isolated_sleep", json!({"seconds": 3.0}), None, Some(false));
    let r2 = h.call("isolated_sleep", json!({"seconds": 3.0}), None, Some(false));
    let r3 = h.call("isolated_sleep", json!({"seconds": 3.0}), None, Some(false));
    let (r1, r2, r3) = tokio::join!(r1, r2, r3);
    let errs: Vec<_> = [r1, r2, r3].into_iter().filter_map(|r| r.err()).collect();
    assert_eq!(errs.len(), 1, "exactly one call must be refused: {errs:?}");
    assert_limit(&errs[0], "max_isolated_workers");
    h.shutdown().await;
}

// ---- memory bound under flood (RC.8A: nothing grows without bound) ----

#[tokio::test]
async fn byte_capped_stream_bounds_memory_under_flood() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.limits.stream_buffer_bytes = 4 * 1024; // tiny budget
    cfg.initial_stream_credit = 1_000_000;
    cfg.stream_buffer = 1_000_000;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (_task, mut items) = h
        .call_stream("infinite_counted", json!({}), None, Some(true))
        .await
        .unwrap();
    // The flood runs >1s with no consumer; buffered bytes stay <= 4KiB
    // (the producer is cancelled at the cap, so the flood STOPS — that's
    // the bound). Then confirm the supervisor is still healthy.
    sleep(Duration::from_secs(1)).await;
    let mut end = None;
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        if let StreamItem::End(r) = item {
            end = Some(r);
            break;
        }
    }
    let err = end.expect("no terminal after flood").unwrap_err();
    assert_limit(&err, "stream_buffer_bytes");
    let out = timeout(T, h.call("add", json!({"a": 1, "b": 1}), None, Some(false)))
        .await
        .expect("supervisor died under flood")
        .unwrap();
    assert_eq!(out, json!(2));
    h.shutdown().await;
}
