//! RC.15 host hardening — stream pull-credit delivery and enforcement.
//!
//!   * The initial `stream.credit` grant travels the control lane: a
//!     saturated data lane can delay the call frame, but it cannot
//!     starve or reorder the credit behind it, and a full control queue
//!     fails the stream loudly instead of leaving it uncredited forever.
//!   * A `stream.chunk` that arrives with no outstanding credit is a
//!     protocol violation — the host fails in-flight work with
//!     `PROTOCOL_VIOLATION` and kills the runtime (same handling as a
//!     malformed frame), it does not silently accept the chunk.

use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(30);

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

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: None,
        ..Default::default()
    }
}

fn fast_config() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(5);
    c.ping_interval = Duration::from_secs(60); // no liveness interference
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 5;
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 1;
    c.cancel_timeout = Duration::from_secs(5);
    // The frame lane, not the command/task queues, must be the bound
    // that trips — pump calls would hit the defaults first.
    c.limits.max_concurrent_tasks = 10_000;
    c.limits.max_queued_commands = 4096;
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

/// Wait for a stderr diagnostic line containing `needle` — event-driven
/// synchronization with the fake runtime's own progress report.
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

/// Slow-starting, slow-reading fake runtime — the saturation harness.
///
/// Timing contract:
///   * it reads nothing for the first 3 s after `hello` — the fill
///     window in which pump frames saturate the OS pipe and then the
///     64-deep writer lane;
///   * it then reads ONE frame and reports `READ 1` on stderr — the
///     synchronization point: one frame's worth of pipe space just
///     drained, so the writer lane has exactly one free slot;
///   * it pauses 1.5 s (the action window — enough for a test to act
///     while the lane/queue depths hold), then reads on at 50 ms per
///     frame to drain the backlog quickly.
///
/// `stream.credit` notifications are stashed per task (the same
/// pending-credit shape the real runtime uses for a grant that beats
/// task registration). A `count_stream` call produces only while it
/// holds stashed credit, then answers its result — a stream whose first
/// chunk needs credit that never arrives ends empty. `echo` answers its
/// input; `cancel` answers terminated.
const FAKE_SLOW_READER: &str = r#"
import sys, os, struct, json, time
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
def rd(n):
    # os.read, not sys.stdin.buffer — a BufferedReader would drain ~8KiB
    # per frame and defeat the data-lane saturation this test relies on.
    out = b""
    while len(out) < n:
        c = os.read(0, n - len(out))
        if not c: return out
        out += c
    return out
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"},{"name":"count_stream","kind":"stream"}]}})
time.sleep(3)
credit = {}
n = 0
while True:
    h = rd(4)
    if len(h) < 4: break
    sz, = struct.unpack(">I", h)
    m = json.loads(rd(sz))
    n += 1
    print("READ %d" % n, file=sys.stderr, flush=True)
    method = m.get("method")
    params = m.get("params") or {}
    if method == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": params.get("seq")}})
    elif method == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif method == "cancel":
        w({"jsonrpc":"2.0","id": m["id"],"result": {"cancelled": True, "status": "terminated"}})
    elif method == "stream.credit":
        t = params.get("task")
        credit[t] = credit.get(t, 0) + params.get("n", 0)
    elif "id" in m:
        if params.get("fn") == "count_stream":
            t = params.get("task")
            seq = 0
            while credit.get(t, 0) > 0:
                credit[t] -= 1
                seq += 1
                w({"jsonrpc":"2.0","method":"stream.chunk","params":{"task": t, "seq": seq, "value": seq}})
            w({"jsonrpc":"2.0","id": m["id"],"result": None})
        else:
            w({"jsonrpc":"2.0","id": m["id"],"result": params.get("input")})
    # One slow tick after the first read — the action window — then fast
    # ticks to drain the backlog.
    time.sleep(1.5 if n == 1 else 0.05)
"#;

/// Pump data-lane call frames until the runtime's stdin queue is full —
/// a call comes back `frame_out_queue` only when the writer lane holds
/// all 64 pending frames (the OS pipe stopped draining). `pad` bytes of
/// input per call fill the OS pipe in a handful of frames. Returns the
/// task ids that were accepted so callers can cancel them later.
async fn saturate_data_lane(h: &Handle, rx: &mut mpsc::Receiver<RuntimeEvent>) -> Vec<u64> {
    let pad = "x".repeat(64 * 1024);
    let mut inflight = Vec::new();
    let mut lanes_full = false;
    // Burst-dispatch, then yield so the actor actually queues the frames:
    // the frame lane fills only while the writer is pipe-blocked, so the
    // pump must interleave dispatch with actor progress.
    for _ in 0..40 {
        for _ in 0..20 {
            match h.call_task("echo", json!(pad), None, Some(false)) {
                Ok(ct) => inflight.push(ct),
                Err(e) => panic!("command queue rejected pump call: {e:?}"),
            }
        }
        tokio::task::yield_now().await;
        // A frame_out_queue result means the lane is full NOW.
        inflight.retain_mut(|ct| match ct.result.try_recv() {
            Ok(Err(e)) => {
                if e.data.as_ref().and_then(|d| d["limit"].as_str()) == Some("frame_out_queue") {
                    lanes_full = true;
                }
                false
            }
            Ok(_) => false, // completed — forget it
            Err(_) => true,
        });
        if lanes_full {
            break;
        }
    }
    assert!(lanes_full, "800 pump calls never filled the data lane");
    // One fake-runtime read past the fill drains exactly one lane entry —
    // the next enqueue has a guaranteed free slot.
    wait_for_diag(rx, "READ").await;
    let mut tasks = Vec::new();
    for ct in &mut inflight {
        if let Ok(t) = ct.task.try_recv() {
            tasks.push(t);
        }
    }
    tasks
}

/// A stream whose first chunk needs credit must still start while the
/// data lane is saturated: the initial grant is control traffic, so it
/// overtakes the backlog of queued call frames, is buffered by the
/// runtime's pending-credit path, and greets the call when it finally
/// arrives. A grant queued behind the flood (or dropped when the lane
/// is momentarily full) leaves the producer with no credit — the stream
/// ends with zero chunks.
#[tokio::test]
async fn initial_credit_reaches_runtime_when_data_lane_saturated() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(512);
    let h = spawn_h(spec(FAKE_SLOW_READER), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    saturate_data_lane(&h, &mut rx).await;

    let (_task, mut items) = h
        .call_stream("count_stream", json!({}), None, Some(true))
        .await
        .expect("stream call rejected — the call frame should still enqueue");

    // The call waits out the remaining backlog (~64 slow reads); the
    // credit, if it ever arrives, must be there when the call is.
    let first = timeout(T, items.recv())
        .await
        .expect("no stream item at all")
        .expect("stream channel closed without a terminal");
    assert!(
        matches!(first, StreamItem::Chunk { .. }),
        "first item must be a chunk — the initial credit had to overtake the saturated data lane, got {first:?}"
    );
    h.shutdown().await;
}

/// If the control queue itself is full when the stream opens, the grant
/// cannot be delivered — the stream fails loudly with the same
/// `ctrl_queue` error a later GrantCredit would get, never silently
/// uncredited.
#[tokio::test]
async fn initial_credit_undeliverable_rejects_stream() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(512);
    let h = spawn_h(spec(FAKE_SLOW_READER), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    let tasks = saturate_data_lane(&h, &mut rx).await;

    // The writer is blocked on a data frame inside the action window;
    // cancels are control traffic for known tasks and pile onto the
    // 16-deep control lane. They must be fired CONCURRENTLY — awaiting
    // each ack serializes them on round-trip latency and the lane drains
    // between sends — and fullness must be detected WITHOUT joining:
    // joining waits for acks that only arrive as the lane drains, by
    // which time there is room again.
    let failed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for t in tasks.iter().take(40) {
        let h2 = h.clone();
        let t = *t;
        let failed = failed.clone();
        tokio::spawn(async move {
            if let Ok(CancelOutcome::DeliveryFailed) = h2.cancel(t, None).await {
                failed.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    timeout(Duration::from_secs(10), async {
        while failed.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("control lane never filled with cancels");

    let err = match h
        .call_stream("count_stream", json!({}), None, Some(true))
        .await
    {
        Ok(_) => panic!("stream opened despite an undeliverable initial grant"),
        Err(e) => e,
    };
    assert_eq!(
        err.code,
        proto::LIMIT_EXCEEDED,
        "expected a typed limit error, got {err:?}"
    );
    assert_eq!(
        err.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("ctrl_queue"),
        "rejection must name ctrl_queue, got {err:?}"
    );
    h.shutdown().await;
}

/// A chunk with no outstanding credit is a protocol violation, not a
/// shrug: the host fails in-flight work with PROTOCOL_VIOLATION and
/// kills the runtime — same treatment as a malformed frame.
#[tokio::test]
async fn uncredited_chunk_is_protocol_violation() {
    /// Well-formed runtime that ignores credit: answers count_stream with
    /// two chunks whatever the grant was (initial credit is 1).
    const FAKE_UNCREDITED: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"count_stream","kind":"stream"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    method = m.get("method")
    params = m.get("params") or {}
    if method == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": params.get("seq")}})
    elif method == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif "id" in m:
        t = params.get("task")
        for seq in (1, 2):
            w({"jsonrpc":"2.0","method":"stream.chunk","params":{"task": t, "seq": seq, "value": seq}})
        w({"jsonrpc":"2.0","id": m["id"],"result": None})
"#;
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(FAKE_UNCREDITED), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    let (_task, mut items) = h
        .call_stream("count_stream", json!({}), None, Some(true))
        .await
        .expect("stream call rejected");
    let mut terminal = None;
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        if let StreamItem::End(r) = item {
            terminal = Some(r);
            break;
        }
    }
    let err = terminal
        .expect("stream never ended")
        .expect_err("uncredited chunks accepted and the stream ended Ok");
    assert_eq!(
        err.code,
        proto::PROTOCOL_VIOLATION,
        "uncredited chunk must trip PROTOCOL_VIOLATION, got {err:?}"
    );
    h.shutdown().await;
}
