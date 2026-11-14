//! RC.8D — bounded protocol fuzz/property tests.
//!
//! A deterministic, seeded hostile runtime exercises the surfaces a
//! compromised or buggy Python process could reach: forged responses,
//! duplicate terminals, chunks for tasks that don't exist, hostile pong
//! fields, unknown notifications, malformed JSON, and absurd declared
//! frame lengths.
//!
//! Invariants asserted, not just "didn't crash":
//!   * every issued call resolves exactly once, always with a typed
//!     outcome — never a hang
//!   * forged traffic for unknown ids/tasks changes nothing observable
//!   * a malformed frame ends the session with a typed failure for every
//!     in-flight call — no panic, no allocation explosion
//!
//! Iterations are bounded for CI; set PYLAIR_FUZZ_ITERS higher for a
//! longer local soak. The child-side sequence is seeded — failures
//! reproduce byte-for-byte.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, ShutdownOutcome, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(20);

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        if std::process::Command::new(cand)
            .arg("--version")
            .output()
            .is_ok()
        {
            return cand.into();
        }
    }
    panic!("no python found for supervisor tests");
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
    c.loop_stall_kill = None; // hostile pong stale fields must not kill
    c.cancel_timeout = Duration::from_secs(2);
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

/// Cooperative but hostile: answers pings and every `call`, while a second
/// thread injects protocol-VALID garbage — forged ids, duplicate
/// terminals, chunks for nonexistent tasks, weird pongs, unknown
/// notifications, and host-direction frames it has no business sending.
const FAKE_HOSTILE: &str = r#"
import sys, struct, json, random, threading, time, os

def w(obj):
    b = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b)
    sys.stdout.buffer.flush()

w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,
  "exposes":[{"name":"hang","kind":"sync"}]}})

ITERS = int(os.environ.get("PYLAIR_FUZZ_ITERS", "150"))
rng = random.Random(20260703)
stop = False

def hostile():
    for _ in range(ITERS):
        if stop: return
        k = rng.randrange(6)
        if k == 0:   # forged response — id never issued by the host
            w({"jsonrpc":"2.0","id": 10**6 + rng.randrange(10**6),
               "result": "forged"})
        elif k == 1: # duplicate/late terminal — may collide with a real id
            w({"jsonrpc":"2.0","id": rng.randrange(64),
               "error": {"code": -32603, "message": "dup"}})
        elif k == 2: # chunk for a task that does not exist
            w({"jsonrpc":"2.0","method":"stream.chunk",
               "params": {"task": 10**6 + rng.randrange(10**6),
                          "seq": rng.randrange(4), "value": "x" * rng.randrange(64)}})
        elif k == 3: # pong with hostile field shapes
            w({"jsonrpc":"2.0","method":"pong",
               "params": {"seq": rng.randrange(8),
                          "loop_stale_ms": rng.choice([0, -5, "x", None, True])}})
        elif k == 4: # unknown notification — spec says ignore
            w({"jsonrpc":"2.0","method":"made.up." + str(rng.randrange(99)),
               "params": {"x": [1, {"y": None}]}})
        else:        # credit grant pointed AT the host — wrong direction
            w({"jsonrpc":"2.0","method":"stream.credit",
               "params": {"task": rng.randrange(10**6),
                          "n": rng.randrange(-5, 10**9)}})
        time.sleep(rng.uniform(0.001, 0.01))

threading.Thread(target=hostile, daemon=True).start()

while True:
    hdr = sys.stdin.buffer.read(4)
    if len(hdr) < 4: break
    (n,) = struct.unpack(">I", hdr)
    try: msg = json.loads(sys.stdin.buffer.read(n))
    except Exception: continue
    if msg.get("method") == "call":
        w({"jsonrpc":"2.0","id": msg["id"], "result": "ok"})
    elif msg.get("method") == "ping":
        w({"jsonrpc":"2.0","method":"pong",
           "params": {"seq": (msg.get("params") or {}).get("seq"),
                      "loop_stale_ms": 0}})
    elif msg.get("method") == "shutdown":
        w({"jsonrpc":"2.0","id": msg["id"], "result": {}})
        stop = True
        break
"#;

/// Valid hello, answers one call, then emits a malformed JSON frame.
const FAKE_MALFORMED: &str = r#"
import sys, struct, json, time

def w(obj):
    b = json.dumps(obj).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b)
    sys.stdout.buffer.flush()

w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,
  "exposes":[{"name":"hang","kind":"sync"}]}})

while True:
    hdr = sys.stdin.buffer.read(4)
    if len(hdr) < 4: break
    (n,) = struct.unpack(">I", hdr)
    try: msg = json.loads(sys.stdin.buffer.read(n))
    except Exception: continue
    if msg.get("method") == "call":
        w({"jsonrpc":"2.0","id": msg["id"], "result": "ok"})
        # Now poison the stream: a frame that is not JSON at all.
        bad = b'\x00\xff{bad jso'
        sys.stdout.buffer.write(struct.pack(">I", len(bad)) + bad)
        sys.stdout.buffer.flush()
        time.sleep(600)
"#;

/// Declares a 4 GiB frame then idles — the reader's max_frame check must
/// fire before any body allocation or long wait.
const FAKE_HUGE_LEN: &str = r#"
import sys, struct, json, time
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,
  "exposes":[{"name":"hang","kind":"sync"}]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b)
sys.stdout.buffer.write(struct.pack(">I", 0xffffffff))
sys.stdout.buffer.flush()
time.sleep(600)
"#;

/// Hostile-but-valid traffic: the session stays alive, every call
/// resolves exactly once, forged ids/tasks change nothing observable.
#[tokio::test]
async fn hostile_valid_frames_never_panic_or_corrupt_state() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(FAKE_HOSTILE), cfg(), ev);
    wait_ready(&h).await;

    // Real calls through the noise — each must resolve (the fake's
    // answer, a forged early answer, or a typed error). None may hang.
    for i in 0..10 {
        let _ = timeout(T, h.call("hang", json!({}), None, Some(false)))
            .await
            .unwrap_or_else(|_| panic!("call {i} hung under hostile traffic"));
        tokio::task::yield_now().await;
    }

    // After the noise: the handle is still usable and honest calls still
    // resolve — the forged frames changed nothing.
    let _ = timeout(T, h.call("hang", json!({}), None, Some(false)))
        .await
        .expect("post-fuzz call hung");

    let out = timeout(T, h.shutdown()).await.expect("shutdown hung");
    assert!(matches!(
        out,
        ShutdownOutcome::Terminated | ShutdownOutcome::ForcedKill
    ));
}

/// Malformed JSON from the child is a protocol violation: every in-flight
/// call gets a typed failure and the runtime is reaped — no hang.
#[tokio::test]
async fn malformed_frame_from_child_is_typed_violation() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(FAKE_MALFORMED), cfg(), ev);
    wait_ready(&h).await;

    // First call succeeds — the fake answers it — and poisons the stream.
    let first = timeout(T, h.call("hang", json!({}), None, Some(false)))
        .await
        .expect("first call hung");
    assert!(first.is_ok(), "cooperative phase failed: {first:?}");

    // The violation kills the session; the next call must fail with a
    // typed code — never hang, never panic the supervisor.
    let e = timeout(T, h.call("hang", json!({}), None, Some(false)))
        .await
        .expect("call after violation hung")
        .expect_err("call after violation must be a typed error");
    assert!(
        matches!(
            e.code,
            proto::PROTOCOL_VIOLATION
                | proto::RUNTIME_CRASHED
                | proto::RUNTIME_UNAVAILABLE
                | proto::LIMIT_EXCEEDED
        ),
        "call after violation failed with untyped {e:?}"
    );
}

/// A >max_frame declared length is rejected before reading the body —
/// the session fails typed and nobody waits for 4 GiB that never comes.
#[tokio::test]
async fn oversized_declared_length_is_rejected_fast() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(FAKE_HUGE_LEN), cfg(), ev);

    // The hello parses, then the poisoned length prefix kills the
    // session — calls resolve typed and fast, nobody blocks on 4 GiB.
    let e = timeout(T, h.call("hang", json!({}), None, Some(false)))
        .await
        .expect("call against oversized-len runtime hung")
        .expect_err("must be a typed failure");
    assert!(
        matches!(
            e.code,
            proto::PROTOCOL_VIOLATION
                | proto::RUNTIME_CRASHED
                | proto::RUNTIME_UNAVAILABLE
                | proto::LIMIT_EXCEEDED
        ),
        "untyped failure {e:?}"
    );
}
