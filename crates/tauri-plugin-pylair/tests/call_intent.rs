//! rc.15 PR I — the host tells the runtime which client intent a call
//! carries. `Command::Call` (plain call) writes `"stream": false`;
//! `Command::CallStream` writes `"stream": true`. The runtime uses the
//! flag to reject call()/stream() mismatches with an actionable error;
//! without it a `call` request is ambiguous.

use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
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

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(10);
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.shutdown_grace = Duration::from_secs(2);
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
    .unwrap_or_else(|_| panic!("never reached {want:?}"));
}

/// Echo the `stream` field of every `call` request back as its result,
/// and whether a `stream.credit` notification arrived for its task.
const ECHO_STREAM_FLAG: &str = r#"
import sys, struct, json
credited = set()
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m.get("method") == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m.get("method") == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif m.get("method") == "stream.credit":
        credited.add(m["params"]["task"])
    elif m.get("method") == "call":
        w({"jsonrpc":"2.0","id": m["id"],
            "result": {"stream": m["params"].get("stream", "ABSENT"),
                       "credited": m["params"]["task"] in credited}})
"#;

#[tokio::test]
async fn plain_call_sends_stream_false_and_call_stream_sends_true() {
    let (ev, rx) = mpsc::channel(64);
    let h = spawn_h(spec(ECHO_STREAM_FLAG), cfg(), ev);
    wait_state(&h, State::Ready).await;

    let v = timeout(T, h.call("echo", serde_json::json!({}), None, Some(false)))
        .await
        .expect("call timed out")
        .expect("call failed");
    assert_eq!(
        v["stream"],
        serde_json::json!(false),
        "a plain call must declare stream:false"
    );
    assert_eq!(
        v["credited"],
        serde_json::json!(false),
        "a plain call must NOT receive the initial credit grant"
    );

    let (_task, mut items) = timeout(
        T,
        h.call_stream("echo", serde_json::json!({}), None, Some(true)),
    )
    .await
    .expect("call_stream timed out")
    .expect("call_stream failed");
    let mut seen = None;
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        if let StreamItem::End(r) = item {
            seen = Some(r.expect("stream terminal error"));
        }
    }
    let seen = seen.expect("no terminal item");
    assert_eq!(
        seen["stream"],
        serde_json::json!(true),
        "a call_stream must declare stream:true"
    );
    assert_eq!(
        seen["credited"],
        serde_json::json!(true),
        "a real stream must receive the initial credit grant"
    );

    // Legacy shape: no `stream` arg at all — the wire field is OMITTED
    // (no validation possible) but accounting stays "stream": slot + the
    // initial credit grant, exactly as before the flag existed.
    let (_task, mut items) = timeout(T, h.call_stream("echo", serde_json::json!({}), None, None))
        .await
        .expect("legacy call_stream timed out")
        .expect("legacy call_stream failed");
    let mut seen = None;
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        if let StreamItem::End(r) = item {
            seen = Some(r.expect("stream terminal error"));
        }
    }
    let seen = seen.expect("no terminal item");
    assert_eq!(
        seen["stream"],
        serde_json::json!("ABSENT"),
        "a pre-flag client must send NO stream field — no validation applies"
    );
    assert_eq!(
        seen["credited"],
        serde_json::json!(true),
        "a legacy call still receives the initial credit grant"
    );

    drop(rx);
    let _ = h.shutdown().await;
}

// ---- end-to-end: the real command path and the REAL runtime ----
//
// The client's call() and stream() both invoke
// `plugin:pylair|python_call_stream`; that command is exactly
// `call_stream(...)` + `forward_stream(...)`, so driving the same entry
// points against `python -m pylair testapp` (below) exercises the full
// host+runtime path a real app takes — the seam the fake-runtime test
// above cannot see. (A mock-Tauri IPC invoke is covered non-Windows by
// `get_ipc_response` in this file too.)

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

async fn collect_terminal(
    items: &mut tauri_plugin_pylair::supervisor::StreamRx,
) -> Result<serde_json::Value, tauri_plugin_pylair::protocol::ErrorObj> {
    use tauri_plugin_pylair::supervisor::StreamItem;
    loop {
        match timeout(T, items.recv()).await.expect("no terminal") {
            Some(StreamItem::End(r)) => return r,
            Some(StreamItem::Chunk { .. }) => {}
            None => panic!("stream closed without a terminal"),
        }
    }
}

/// A client `call()` on a plain function. The client's call() invokes
/// `python_call_stream` (it needs the early task id for cancel/deadline),
/// so a plain call reaches the runtime through this command — with
/// `stream: false` declared. The runtime must run it.
#[tokio::test]
async fn plain_call_through_call_stream_succeeds() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_state(&h, State::Ready).await;

    let (_task, mut items) = timeout(
        T,
        h.call_stream(
            "add",
            serde_json::json!({"a": 1, "b": 2}),
            Some("win".into()),
            Some(false),
        ),
    )
    .await
    .expect("call_stream timed out")
    .expect("call_stream failed");
    let v = collect_terminal(&mut items)
        .await
        .expect("plain call through python_call_stream must succeed");
    assert_eq!(v, serde_json::json!(3));
    h.shutdown().await;
}

/// stream() intent on a generator: unchanged path, chunks arrive.
#[tokio::test]
async fn stream_intent_on_generator_delivers_chunks() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_state(&h, State::Ready).await;

    let (_task, mut items) = timeout(
        T,
        h.call_stream(
            "counter",
            serde_json::json!({"n": 2}),
            Some("w".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream timed out")
    .expect("call_stream failed");
    let mut vals = vec![];
    let r = loop {
        match timeout(T, items.recv()).await.expect("no item") {
            Some(StreamItem::Chunk { value, .. }) => vals.push(value),
            Some(StreamItem::End(r)) => break r,
            None => panic!("closed without terminal"),
        }
    };
    assert_eq!(vals, vec![serde_json::json!(0), serde_json::json!(1)]);
    r.expect("stream terminal failed");
    h.shutdown().await;
}

/// The two mismatch directions, end-to-end: the runtime's flag check
/// must name the API the caller should have used.
#[tokio::test]
async fn mismatch_intents_fail_with_actionable_errors() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_state(&h, State::Ready).await;

    // call() intent on a generator.
    let (_t, mut items) = timeout(
        T,
        h.call_stream(
            "counter",
            serde_json::json!({"n": 2}),
            Some("w".into()),
            Some(false),
        ),
    )
    .await
    .expect("call_stream timed out")
    .expect("call_stream failed");
    let e = collect_terminal(&mut items)
        .await
        .expect_err("call() intent on a generator must error");
    assert!(
        e.message
            .contains("is a stream function — call it with stream()"),
        "wrong message: {e:?}"
    );

    // stream() intent on a plain function.
    let (_t, mut items) = timeout(
        T,
        h.call_stream(
            "add",
            serde_json::json!({"a": 1, "b": 2}),
            Some("w".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream timed out")
    .expect("call_stream failed");
    let e = collect_terminal(&mut items)
        .await
        .expect_err("stream() intent on a plain fn must error");
    assert!(
        e.message
            .contains("is not a stream function — call it with call()"),
        "wrong message: {e:?}"
    );
    h.shutdown().await;
}

/// A pre-flag client (no `stream` arg) gets NO validation: a plain
/// function still runs through the channel path without a flag.
#[tokio::test]
async fn legacy_no_intent_skips_validation() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_state(&h, State::Ready).await;

    let (_t, mut items) = timeout(
        T,
        h.call_stream(
            "add",
            serde_json::json!({"a": 2, "b": 3}),
            Some("w".into()),
            None,
        ),
    )
    .await
    .expect("call_stream timed out")
    .expect("call_stream failed");
    let v = collect_terminal(&mut items)
        .await
        .expect("legacy no-intent call must succeed unvalidated");
    assert_eq!(v, serde_json::json!(5));
    h.shutdown().await;
}

/// 33 concurrent plain calls must ALL succeed while
/// max_concurrent_streams is still 32 — plain calls take task slots
/// only. Afterwards, real streams are still bounded at 32.
#[tokio::test]
async fn plain_calls_take_no_stream_slots_and_streams_stay_bounded() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut c = cfg();
    c.limits.max_concurrent_streams = 32;
    let h = spawn_h(real_runtime_spec(), c, ev);
    wait_state(&h, State::Ready).await;

    // 33 in-flight plain calls — before the fix the 33rd died on the
    // stream limit even though nothing streams.
    let mut pending = Vec::new();
    for _ in 0..33 {
        let (_task, items) = timeout(
            T,
            h.call_stream(
                "echo_later",
                serde_json::json!({"text": "x", "delay": 1.5}),
                Some("w".into()),
                Some(false),
            ),
        )
        .await
        .expect("plain call timed out")
        .expect("plain call hit a stream limit — it must take no stream slot");
        pending.push(items);
    }
    for mut items in pending {
        collect_terminal(&mut items)
            .await
            .expect("plain call failed");
    }

    // Real streams stay bounded: hold 32, the 33rd is LIMIT_EXCEEDED…
    let mut held = Vec::new();
    for i in 0..32 {
        let (_t, items) = timeout(
            T,
            h.call_stream(
                "infinite",
                serde_json::json!({}),
                Some("w".into()),
                Some(true),
            ),
        )
        .await
        .expect("stream timed out")
        .expect("stream failed");
        held.push(items);
        // Each held stream costs one control-lane credit grant; pace the
        // opens so the 16-slot ctrl queue can drain between batches.
        if i % 8 == 7 {
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }
    let e = match timeout(
        T,
        h.call_stream(
            "infinite",
            serde_json::json!({}),
            Some("w".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream timed out")
    {
        Err(e) => e,
        Ok(_) => panic!("a 33rd stream must still be rejected"),
    };
    assert_eq!(e.code, tauri_plugin_pylair::protocol::LIMIT_EXCEEDED);
    assert!(
        e.message.contains("too many in-flight streams"),
        "wrong message: {e:?}"
    );

    // …but a plain call proceeds even with every stream slot held.
    let (_t, mut items) = timeout(
        T,
        h.call_stream(
            "add",
            serde_json::json!({"a": 1, "b": 1}),
            Some("w".into()),
            Some(false),
        ),
    )
    .await
    .expect("plain call timed out")
    .expect("plain call must not take a stream slot");
    collect_terminal(&mut items)
        .await
        .expect("plain call failed");

    drop(held);
    h.shutdown().await;
}

/// Legacy (no `stream` field) still consumes a stream slot — its shape
/// is ambiguous, so conservative accounting is unchanged.
#[tokio::test]
async fn legacy_no_intent_still_counts_as_stream() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut c = cfg();
    c.limits.max_concurrent_streams = 1;
    let h = spawn_h(real_runtime_spec(), c, ev);
    wait_state(&h, State::Ready).await;

    let (_t, _items) = timeout(
        T,
        h.call_stream("infinite", serde_json::json!({}), Some("w".into()), None),
    )
    .await
    .expect("legacy call timed out")
    .expect("legacy call failed");
    let e = match timeout(
        T,
        h.call_stream(
            "infinite",
            serde_json::json!({}),
            Some("w".into()),
            Some(true),
        ),
    )
    .await
    .expect("second call timed out")
    {
        Err(e) => e,
        Ok(_) => panic!("legacy call must occupy the stream slot"),
    };
    assert_eq!(e.code, tauri_plugin_pylair::protocol::LIMIT_EXCEEDED);
    h.shutdown().await;
}

/// Cancel works on a plain-intent call — the early task id + terminal
/// arrive exactly as for streams.
#[tokio::test]
async fn cancel_still_works_on_a_plain_intent_call() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), cfg(), ev);
    wait_state(&h, State::Ready).await;

    let (task, mut items) = timeout(
        T,
        h.call_stream(
            "echo_later",
            serde_json::json!({"text": "x", "delay": 30.0}),
            Some("w".into()),
            Some(false),
        ),
    )
    .await
    .expect("plain call timed out")
    .expect("plain call failed");

    let outcome = timeout(T, h.cancel(task, Some("w".into())))
        .await
        .expect("cancel timed out")
        .expect("cancel errored");
    assert_eq!(
        outcome,
        tauri_plugin_pylair::supervisor::CancelOutcome::Cancelled
    );

    let e = collect_terminal(&mut items)
        .await
        .expect_err("cancelled plain call must end with an error terminal");
    assert_eq!(e.code, tauri_plugin_pylair::protocol::TASK_CANCELLED);
    h.shutdown().await;
}
