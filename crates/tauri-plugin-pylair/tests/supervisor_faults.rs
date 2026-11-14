//! Fault-injection tests for the supervisor: kill mid-call, startup hang,
//! malformed frames, stray garbage on stdout, wedged runtime (no pongs),
//! crash-loop breaker. Each case spawns a real `python -c` process.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(15);

/// spawn() now returns (Handle, actor future) — tests drive the actor on
/// the test runtime.
fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

// ---- fake runtime scripts (stdin/stdout frame protocol, python) ----

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

/// Never sends hello — startup hang.
const FAKE_HANG: &str = "import time; time.sleep(600)";

/// Prints raw bytes to stdout — stray writes must be a protocol violation.
const FAKE_STRAY_STDOUT: &str = r#"import sys; sys.stdout.write("stray bytes on fd 1\n"); sys.stdout.flush(); import time; time.sleep(600)"#;

/// Valid frame envelope, invalid JSON body.
const FAKE_MALFORMED: &str = r#"
import sys, struct
sys.stdout.buffer.write(struct.pack(">I", 4) + b"nope"); sys.stdout.buffer.flush()
import time; time.sleep(600)
"#;

/// Hello, pong once, then wedge forever (never reads stdin again).
const FAKE_WEDGED: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time
time.sleep(600)
"#;

/// Dies on the first call.
const FAKE_DIE_ON_CALL: &str = r#"
import sys, struct, json, os
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"boom","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "call":
        os._exit(1)
"#;

/// Exits immediately — exercises restart + breaker.
const FAKE_EXIT_NOW: &str = "import sys; sys.exit(1)";

/// hello with the wrong protocol version — deployment bug, not a crash.
const FAKE_BAD_VERSION: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":99,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time; time.sleep(600)
"#;

/// The REAL runtime: `python -m pylair testapp` with the package and the
/// test app found via PYTHONPATH + cwd — no install step needed.
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

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        // Check exit status, not just spawn: on Windows `python3` can be the
        // Microsoft Store alias — it launches but does not run Python.
        // Require >=3.12: the runtime uses PEP 695 syntax.
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
    c.max_frame = proto::max_frame();
    c.restart_trigger = None;
    c
}

async fn wait_state(h: &Handle, want: State) {
    wait_state_within(h, want, T).await;
}

async fn wait_state_within(h: &Handle, want: State, limit: Duration) {
    let mut rx = h.subscribe_state();
    timeout(limit, async {
        while *rx.borrow() != want {
            rx.changed().await.expect("state channel closed");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never reached {want:?} (at {:?})", h.state()));
}

// ---- tests ----

#[tokio::test]
async fn hello_echo_pong_shutdown() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_READY), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    let out = h
        .call("echo", json!({"x": 1}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!({"x": 1}));
    h.shutdown().await;
    wait_state(&h, State::Stopped).await;
}

#[tokio::test]
async fn startup_hang_is_killed_and_fails() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    // breaker_crashes = 2 -> after two failed startups, state Failed
    let h = spawn_h(spec(FAKE_HANG), fast_config(), ev);
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn stray_stdout_is_protocol_violation() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_STRAY_STDOUT), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    // drain events: expect a Restarting transition (first crash) at least
    let mut saw_restart = false;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Restarting,
            ..
        } = e
        {
            saw_restart = true;
        }
    }
    assert!(saw_restart || h.state() == State::Failed);
}

#[tokio::test]
async fn malformed_frame_is_protocol_violation() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_MALFORMED), fast_config(), ev);
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn wedged_runtime_killed_on_missed_pongs() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_WEDGED), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    // wedged: hello sent, never pongs -> Unresponsive -> killed -> restart cycle
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn kill_during_call_fails_caller_with_crashed() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_DIE_ON_CALL), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    let err = h
        .call("boom", json!({}), None, Some(false))
        .await
        .unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_CRASHED);
}

#[tokio::test]
async fn crash_loop_breaker_trips() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_EXIT_NOW), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    let mut restarts = 0;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Restarting,
            ..
        } = e
        {
            restarts += 1;
        }
    }
    assert!(
        restarts >= 1,
        "expected at least one restart before breaker"
    );
    // and no more spawns after Failed — state stays Failed
    sleep(Duration::from_millis(300)).await;
    assert_eq!(h.state(), State::Failed);
    // calls fail fast with RUNTIME_UNAVAILABLE
    let err = h
        .call("x", Value::Null, None, Some(false))
        .await
        .unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_UNAVAILABLE);
}

// ---- end-to-end: real Rust supervisor <-> real Python runtime ----
// M1 exit criterion: the two sides are tested against each other, not only
// against scripted fakes — protocol drift cannot hide here.

#[tokio::test]
async fn e2e_real_runtime_full_flow() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), fast_config(), ev);

    wait_state(&h, State::Ready).await;
    eprintln!("e2e: ready");

    // exposes manifest from the real testapp
    let mut exposes = Value::Null;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::Exposes(x) = e {
            exposes = x;
        }
    }
    let names: Vec<&str> = exposes
        .as_array()
        .expect("exposes is an array")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    for f in ["add", "counter", "sleep_forever", "isolated_add"] {
        assert!(names.contains(&f), "missing expose {f} in {names:?}");
    }
    eprintln!("e2e: exposes ok {names:?}");

    // plain call
    let out = timeout(
        T,
        h.call("add", json!({"a": 20, "b": 22}), None, Some(false)),
    )
    .await
    .expect("call timed out")
    .unwrap();
    assert_eq!(out, json!(42));
    eprintln!("e2e: call ok");

    // streaming call: chunks then terminal
    let (_task, mut items) = timeout(
        T,
        h.call_stream("counter", json!({"n": 3}), None, Some(true)),
    )
    .await
    .expect("call_stream timed out")
    .unwrap();
    let mut seen = Vec::new();
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        match item {
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { value, .. } => {
                seen.push(value.as_i64().unwrap())
            }
            tauri_plugin_pylair::supervisor::StreamItem::End(res) => {
                assert!(res.is_ok());
                break;
            }
        }
    }
    assert_eq!(seen, vec![0, 1, 2]);
    eprintln!("e2e: stream ok");

    // cancel a never-ending async task — eager dispatch via call_task so the
    // task id is known before we cancel it.
    let ct = h
        .call_task("sleep_forever", json!({}), None, Some(false))
        .unwrap();
    let task = timeout(T, ct.task)
        .await
        .expect("task id timed out")
        .unwrap();
    let cancelled = timeout(T, h.cancel(task, None))
        .await
        .expect("cancel timed out")
        .unwrap();
    assert_eq!(
        cancelled,
        tauri_plugin_pylair::supervisor::CancelOutcome::Cancelled
    );
    let err = timeout(T, ct.result)
        .await
        .expect("result timed out")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, proto::TASK_CANCELLED);
    eprintln!("e2e: cancel ok");

    // stderr from the real runtime is drained (fd-1 hygiene lands there)
    h.shutdown().await;
    wait_state(&h, State::Stopped).await;
    eprintln!("e2e: stopped");
}

#[tokio::test]
async fn version_mismatch_goes_straight_to_failed() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_BAD_VERSION), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    // Fatal is not a crash: zero Restarting transitions must have occurred.
    let mut restarts = 0;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Restarting,
            ..
        } = e
        {
            restarts += 1;
        }
    }
    assert_eq!(
        restarts, 0,
        "version mismatch must not enter the restart loop"
    );
}

#[tokio::test]
async fn retry_respawns_after_failed() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_EXIT_NOW), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    h.retry().await.unwrap();
    // Retry must leave Failed: expect a Starting transition next.
    let saw_starting = timeout(T, async {
        while let Ok(Some(e)) = timeout(T, rx.recv()).await {
            if let RuntimeEvent::StateChanged {
                to: State::Starting,
                ..
            } = e
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_starting, "retry never respawned the runtime");
    // (the fake still fails -> back to Failed, which is fine)
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn healthy_runtime_never_flaps_unresponsive() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_READY), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    // sit through several ping intervals with a pinging runtime
    sleep(Duration::from_millis(600)).await;
    let mut flapped = false;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Unresponsive,
            ..
        } = e
        {
            flapped = true;
        }
    }
    assert!(!flapped, "healthy runtime flapped to Unresponsive");
}

#[tokio::test]
async fn stderr_is_drained_and_forwarded() {
    const NOISY: &str = r#"
import sys, struct, json, time
sys.stderr.write("hello from stderr\n"); sys.stderr.flush()
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
time.sleep(0.3)
"#;
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(NOISY), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    sleep(Duration::from_millis(400)).await;
    let mut lines = vec![];
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::Stderr(l) = e {
            lines.push(l);
        }
    }
    assert!(lines.iter().any(|l| l.contains("hello from stderr")));
}

#[tokio::test]
async fn restart_trigger_respawns_cleanly() {
    // Dev-mode hot reload: touching the trigger file respawns the runtime
    // without crash accounting — the state goes Restarting -> Ready and a
    // fresh generation serves calls.
    let dir = std::env::temp_dir().join(format!("pylair-trig-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let trigger = dir.join("trigger");
    std::fs::write(&trigger, "0").unwrap();

    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.restart_trigger = Some(trigger.clone());
    let h = spawn_h(spec(FAKE_READY), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Rewrite past the mtime granularity of the first write.
    sleep(Duration::from_millis(50)).await;
    std::fs::write(&trigger, "1").unwrap();

    let saw_restart_note = timeout(T, async {
        while let Some(e) = rx.recv().await {
            if let RuntimeEvent::Stderr(l) = e {
                if l.contains("source changed") {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_restart_note, "no restart notice on trigger change");

    // The respawned generation must serve calls again.
    wait_state(&h, State::Ready).await;
    let out = h
        .call("echo", json!({"after": "restart"}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!({"after": "restart"}));
    h.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- stream consumer failure (audit P1-5) ----

async fn produced(h: &Handle) -> i64 {
    timeout(T, h.call("produced_count", json!({}), None, Some(false)))
        .await
        .expect("produced_count timed out")
        .unwrap()
        .as_i64()
        .unwrap()
}

/// Lots of credit + a tiny buffer + a consumer that never drains: the
/// supervisor must stay live, the stream must still receive an explicit
/// terminal error (not hang), and the producer must be stopped.
#[tokio::test]
async fn stalled_consumer_gets_terminal_error_and_producer_stops() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.initial_stream_credit = 100_000;
    cfg.stream_buffer = 4;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (_task, mut items) = timeout(
        T,
        h.call_stream("infinite_counted", json!({}), None, Some(true)),
    )
    .await
    .expect("call_stream timed out")
    .unwrap();
    // Do not drain. The supervisor must keep serving other calls meanwhile.
    sleep(Duration::from_millis(500)).await;
    let out = timeout(T, h.call("add", json!({"a": 1, "b": 2}), None, Some(false)))
        .await
        .expect("supervisor wedged behind a stalled stream");
    assert_eq!(out.unwrap(), json!(3));

    // Producer was cancelled: its counter stops moving.
    let a = produced(&h).await;
    sleep(Duration::from_millis(400)).await;
    let b = produced(&h).await;
    assert_eq!(a, b, "producer kept running after its consumer stalled");

    // Late consumer: gets the buffered chunks, then the terminal error.
    let mut end = None;
    while let Some(item) = timeout(T, items.recv()).await.expect("recv timed out") {
        if let tauri_plugin_pylair::supervisor::StreamItem::End(r) = item {
            end = Some(r);
            break;
        }
    }
    let err = end.expect("stream never delivered a terminal").unwrap_err();
    assert!(err.message.contains("stalled"), "{err:?}");
    assert!(timeout(T, items.recv()).await.unwrap().is_none());
}

/// Receiver dropped (consumer vanished): the producer must be cancelled.
#[tokio::test]
async fn vanished_consumer_cancels_the_producer() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.initial_stream_credit = 100_000;
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (_task, items) = timeout(
        T,
        h.call_stream("infinite_counted", json!({}), None, Some(true)),
    )
    .await
    .expect("call_stream timed out")
    .unwrap();
    sleep(Duration::from_millis(200)).await;
    drop(items);
    sleep(Duration::from_millis(500)).await;
    let a = produced(&h).await;
    sleep(Duration::from_millis(400)).await;
    let b = produced(&h).await;
    assert_eq!(a, b, "producer kept running after its consumer vanished");
}

// ---- process containment (audit P1-6) ----

/// A fake runtime that spawns a heartbeat grandchild, then behaves like
/// FAKE_READY (or, with `crash`, exits right after hello). The heartbeat
/// file only keeps growing while the grandchild is alive.
///
/// The parent gates its `hello` on the grandchild's first byte: under
/// load the grandchild's interpreter boot can take many seconds, and a
/// test asserting on Ready/Failed plus a fixed delay was really just
/// measuring CPython startup. With the gate, reaching a state *proves*
/// the first byte landed; the only bound is the supervisor's own
/// hello_timeout (bumped for these tests via grandchild_config).
fn runtime_with_grandchild(beat: &std::path::Path, crash: bool) -> String {
    let path = beat.to_string_lossy().replace('\\', "/");
    format!(
        r#"
import sys, struct, json, subprocess, time, threading, os
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
subprocess.Popen([sys.executable, "-c",
    "import time\nf = open('{path}', 'a')\nwhile True:\n    f.write('x'); f.flush(); time.sleep(0.1)"])
while not (os.path.exists('{path}') and os.path.getsize('{path}') > 0):
    time.sleep(0.05)
w({{"jsonrpc":"2.0","method":"hello","params":{{"protocol":1,"exposes":[]}}}})
if {crash}:
    # Keep answering pings (so the supervisor does NOT take its wedged-kill
    # path), then die on our own: the natural-exit path.
    threading.Timer(0.6, lambda: os._exit(3)).start()
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({{"jsonrpc":"2.0","method":"pong","params":{{"seq": m["params"]["seq"]}}}})
    elif m["method"] == "shutdown":
        w({{"jsonrpc":"2.0","id": m["id"],"result": None}}); break
"#,
        path = path,
        crash = if crash { "True" } else { "False" },
    )
}

fn beat_len(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// The fixtures gate hello on the first beat byte, so reaching a state
/// already implies it landed — this poll is a cheap re-check that keeps
/// the vacuous-pass guard explicit (an empty file here would mean the
/// gate regressed, not merely a slow boot).
async fn wait_beat_started(beat: &std::path::Path, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while beat_len(beat) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "beat file still empty 10s after {what} — spawn or first heartbeat never landed"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn assert_heartbeat_stopped(beat: &std::path::Path, what: &str) {
    sleep(Duration::from_millis(500)).await;
    let a = beat_len(beat);
    sleep(Duration::from_millis(700)).await;
    let b = beat_len(beat);
    assert_eq!(a, b, "grandchild kept running after {what}");
}

/// The grandchild fixtures gate hello on the descendant's first byte,
/// so spawn→hello includes a full second interpreter boot — well past
/// fast_config's 2s hello timeout under load. The crash variant runs two
/// such spawn cycles before Failed, so the state wait needs the same
/// headroom.
fn grandchild_config() -> SupervisorConfig {
    let mut c = fast_config();
    c.hello_timeout = Duration::from_secs(60);
    c
}

fn beat_file(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("pylair-beat-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[tokio::test]
async fn shutdown_resolves_only_after_the_whole_tree_is_gone() {
    let beat = beat_file("shutdown");
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(
        spec(&runtime_with_grandchild(&beat, false)),
        grandchild_config(),
        ev,
    );
    wait_state_within(&h, State::Ready, Duration::from_secs(60)).await;
    wait_beat_started(&beat, "Ready").await;

    timeout(T, h.shutdown())
        .await
        .expect("shutdown never resolved");
    // The ack means cleanup finished: the grandchild must already be dead.
    let a = beat_len(&beat);
    sleep(Duration::from_millis(700)).await;
    assert_eq!(
        a,
        beat_len(&beat),
        "shutdown() resolved with a live descendant"
    );
    let _ = std::fs::remove_file(&beat);
}

#[tokio::test]
async fn runtime_exiting_on_its_own_does_not_leave_descendants() {
    let beat = beat_file("crash");
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = grandchild_config();
    cfg.breaker_crashes = 2;
    let h = spawn_h(spec(&runtime_with_grandchild(&beat, true)), cfg, ev);
    wait_state_within(&h, State::Failed, Duration::from_secs(60)).await;
    wait_beat_started(&beat, "Failed").await;
    assert_heartbeat_stopped(&beat, "the runtime crashed").await;
    let _ = std::fs::remove_file(&beat);
}

#[tokio::test]
async fn shutdown_during_restart_backoff_does_not_wait_out_the_backoff() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.breaker_crashes = 100; // stay in Restarting, never Failed
    cfg.backoff_base = Duration::from_secs(30);
    cfg.backoff_cap = Duration::from_secs(30);
    let h = spawn_h(spec(FAKE_EXIT_NOW), cfg, ev);
    wait_state(&h, State::Restarting).await;
    let t0 = std::time::Instant::now();
    timeout(Duration::from_secs(5), h.shutdown())
        .await
        .expect("shutdown blocked behind the restart backoff");
    assert!(t0.elapsed() < Duration::from_secs(5));
}

// ---- serving health vs transport health (audit P1-7) ----

/// Pongs prove the pipe is alive, not that asyncio is serving. A loop blocked
/// past the stall limit must be treated as wedged: the in-flight call fails
/// with RUNTIME_WEDGED instead of hanging forever behind healthy pongs.
#[tokio::test]
async fn blocked_event_loop_is_killed_and_the_call_fails_wedged() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.ping_interval = Duration::from_millis(100);
    cfg.loop_stall_kill = Some(Duration::from_millis(1500));
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let ct = h
        .call_task("wedge_the_loop", json!({}), None, Some(false))
        .unwrap();
    let err = timeout(T, ct.result)
        .await
        .expect("call hung behind a blocked loop with healthy pongs")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_WEDGED, "{err:?}");
}

/// A cancel the runtime cannot acknowledge must not hang the caller forever.
#[tokio::test]
async fn cancel_ack_is_bounded() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.cancel_timeout = Duration::from_millis(600);
    // loop_stall_kill stays off (the default): only the cancel deadline can save us.
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let ct = h
        .call_task("wedge_the_loop", json!({}), None, Some(false))
        .unwrap();
    let task = timeout(T, ct.task).await.unwrap().unwrap();
    sleep(Duration::from_millis(300)).await; // let the loop block
    let t0 = std::time::Instant::now();
    let r = timeout(Duration::from_secs(5), h.cancel(task, None))
        .await
        .expect("cancel() never returned");
    let err = r.expect_err("a blocked loop cannot have acknowledged the cancel");
    assert_eq!(err.code, proto::RUNTIME_WEDGED, "{err:?}");
    assert!(t0.elapsed() < Duration::from_secs(3));
}

/// Default behaviour (loop_stall_kill = None): a stalled loop is reported,
/// not killed. A multi-GB model load can legitimately block asyncio for
/// minutes — the runtime must NOT restart and queued calls must still
/// complete once the loop frees.
#[tokio::test]
async fn blocked_loop_warns_but_does_not_kill_by_default() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = fast_config();
    cfg.ping_interval = Duration::from_millis(100);
    // A blocked loop must not starve reader-thread pongs — but on a loaded CI
    // runner the reader can lag by scheduling alone. Give the transport check
    // a 3s dead-air budget so only a genuinely wedged transport kills.
    cfg.missed_pongs_to_kill = 30;
    // loop_stall_kill stays None — the shipped default.
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Block the loop ~1.5s: several ping intervals carry stale pongs. The
    // call resolves once the loop frees — no kill, no restart.
    let out = timeout(
        T,
        h.call("block_loop", json!({"seconds": 1.5}), None, Some(false)),
    )
    .await
    .expect("call never returned — the runtime may have been killed")
    .unwrap();
    assert_eq!(out, json!("unblocked"));

    wait_state(&h, State::Ready).await;
    let mut saw_stall_diag = false;
    let mut saw_restart = false;
    while let Ok(e) = rx.try_recv() {
        match e {
            RuntimeEvent::StateChanged {
                to: State::Unresponsive,
                detail,
                ..
            } if detail.contains("event loop blocked") => saw_stall_diag = true,
            RuntimeEvent::StateChanged {
                to: State::Restarting | State::Starting | State::Failed,
                ..
            } => saw_restart = true,
            _ => {}
        }
    }
    assert!(saw_stall_diag, "no loop-stall diagnostic was emitted");
    assert!(!saw_restart, "runtime restarted on a blocked loop");
    // Same generation still serves calls.
    let out = h
        .call("add", json!({"a": 1, "b": 2}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!(3));
}

/// P1-8: there is no stdout READY-string to spoof — readiness is the framed
/// `hello` notification, and stdout hygiene sends user prints to stderr.
/// A module that prints "READY" (and more) at import must still reach
/// Ready, with the prints arriving as stderr events.
#[tokio::test]
async fn user_prints_neither_corrupt_nor_spoof_readiness() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut noisy = real_runtime_spec();
    noisy.args[2] = "testapp_noisy".into();
    let h = spawn_h(noisy, fast_config(), ev);
    wait_state(&h, State::Ready).await; // framed hello, prints be damned
    let out = timeout(T, h.call("add", json!({"a": 2, "b": 3}), None, Some(false)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out, json!(5));
    sleep(Duration::from_millis(300)).await;
    let mut lines = vec![];
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::Stderr(l) = e {
            lines.push(l);
        }
    }
    assert!(
        lines.iter().any(|l| l.contains("import-time print")),
        "print() must reach stderr events, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("add called: 2 + 3")),
        "in-call print() must reach stderr events, got: {lines:?}"
    );
}
