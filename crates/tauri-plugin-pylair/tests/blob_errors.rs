//! Decision-0010 §6: a rejected `blob.register` reaches the caller.
//! A register lands on the FIFO ahead of the frame carrying its `$blob`
//! tag, so the session records the rejection per task and the task's own
//! terminal (or its next `stream.chunk`) delivers it — caps as
//! `LIMIT_EXCEEDED` + `data.limit`, malformed registers as `INTERNAL`.
//! Without this the response ships a tag that fetches 404 and an app
//! that never calls `release()` silently breaks at 256 handles.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::blobs::BlobLimits;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
use tauri_plugin_pylair::ErrorObj;
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(30);
const WIN: &str = "main";

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

fn fast_config(blob_root: &std::path::Path, blob_limits: BlobLimits) -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(5);
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 2;
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 8;
    c.cancel_timeout = Duration::from_secs(5);
    c.blob_root = Some(blob_root.into());
    c.blob_limits = Some(blob_limits);
    c
}

fn blob_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pylair-bloberr-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
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

fn assert_limit(err: &ErrorObj, limit: &str) {
    assert_eq!(err.code, proto::LIMIT_EXCEEDED, "wrong code: {err:?}");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some(limit),
        "wrong limit name: {err:?}"
    );
    assert!(
        err.message.contains("PYLAIR_") || err.message.contains("share"),
        "message names no knob: {}",
        err.message
    );
    assert!(
        err.message.contains("release()") || limit == "blob_bytes",
        "message does not tell the developer to release: {}",
        err.message
    );
}

/// The blob id inside a `$blob` tag result, or panic.
fn blob_id(v: &serde_json::Value) -> &str {
    v["$blob"]["id"]
        .as_str()
        .expect("result is not a $blob tag")
}

/// The doc's literal cap: 256 live handles per window — the 257th
/// `blob.register` is refused and the call must see it, not a 404.
#[tokio::test]
async fn handles_cap_reaches_the_call() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("handles");
    let mut l = BlobLimits::from_env();
    l.max_blobs_per_window = 256;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    // 256 live handles — each call leaves one until released.
    for i in 0..256 {
        let v = timeout(
            T,
            h.call("make_blob", json!({}), Some(WIN.into()), Some(false)),
        )
        .await
        .unwrap_or_else(|_| panic!("call {i} timed out"))
        .unwrap_or_else(|e| panic!("call {i} failed: {e:?}"));
        blob_id(&v);
    }
    // The 257th: the tag's register is refused ahead of the terminal —
    // the caller gets LIMIT_EXCEEDED, not a dead URL.
    let err = h
        .call("make_blob", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "blob_handles");
    assert!(err.message.contains("PYLAIR_MAX_BLOBS_PER_WINDOW"));

    h.shutdown().await;
}

/// The same cap mid-stream: the chunk's register is refused, the stream
/// ends with the typed error, the producer is cancelled the way a client
/// cancel would, and the task is tombstoned — a later cancel reports
/// `Completed`, and the next call still works.
#[tokio::test]
async fn stream_rejection_ends_stream_and_recovers() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("stream");
    let mut l = BlobLimits::from_env();
    l.max_blobs_per_window = 1;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    // Fill the window's single handle slot.
    h.call("make_blob", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap();

    // stream_blob yields "lead" then a Blob — the first chunk passes,
    // the second's register is refused and ends the stream.
    let (task, mut rx) = h
        .call_stream("stream_blob", json!({}), Some(WIN.into()), Some(true))
        .await
        .unwrap();
    let first = timeout(T, rx.recv()).await.unwrap().unwrap();
    assert!(
        matches!(first, StreamItem::Chunk { .. }),
        "first: {first:?}"
    );
    let end = loop {
        match timeout(T, rx.recv()).await.unwrap().unwrap() {
            StreamItem::End(res) => break res,
            StreamItem::Chunk { .. } => {}
        }
    };
    let err = end.unwrap_err();
    assert_limit(&err, "blob_handles");

    // Tombstoned like a finished task: a cancel after the fact says
    // Completed, and the session is healthy for the next call.
    let oc = timeout(T, h.cancel(task, None)).await.unwrap().unwrap();
    assert_eq!(oc, CancelOutcome::Completed);
    let out = h
        .call(
            "add",
            json!({"a": 1, "b": 2}),
            Some(WIN.into()),
            Some(false),
        )
        .await
        .unwrap();
    assert_eq!(out, json!(3));

    h.shutdown().await;
}

/// Per-blob bytes cap (doc §6's `blob_bytes`): the blob itself is over
/// `PYLAIR_MAX_BLOB_BYTES` — releasing other handles cannot help.
#[tokio::test]
async fn per_blob_bytes_cap() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("bytes");
    let mut l = BlobLimits::from_env();
    l.max_blob_bytes = 8;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    let err = h
        .call(
            "sized_blob",
            json!({"n": 16}),
            Some(WIN.into()),
            Some(false),
        )
        .await
        .unwrap_err();
    assert_limit(&err, "blob_bytes");
    assert!(err.message.contains("PYLAIR_MAX_BLOB_BYTES"));

    h.shutdown().await;
}

/// Generation-pool cap (`blob_total`): within a fair share but the pool
/// is drained — the name stays the pool's, not the share's.
#[tokio::test]
async fn generation_total_cap() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("total");
    let mut l = BlobLimits::from_env();
    // max_blob_total below max_blob_bytes is raised to it — keep the
    // per-blob cap under the pool cap or the clamp eats the test.
    l.max_blob_bytes = 24;
    l.max_blob_total = 30;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    h.call(
        "sized_blob",
        json!({"n": 20}),
        Some(WIN.into()),
        Some(false),
    )
    .await
    .unwrap();
    let err = h
        .call(
            "sized_blob",
            json!({"n": 12}),
            Some(WIN.into()),
            Some(false),
        )
        .await
        .unwrap_err();
    assert_limit(&err, "blob_total");
    assert!(err.message.contains("PYLAIR_MAX_BLOB_TOTAL"));

    h.shutdown().await;
}

/// Fair share (`blob_bytes_share`): two windows, the hog's growth is
/// clipped at its fair bound even though the pool still has headroom —
/// the rejection is the hog's, not "the pool is full".
#[tokio::test]
async fn share_cap_names_the_fair_bound() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("share");
    let mut l = BlobLimits::from_env();
    l.max_blob_bytes = 100;
    l.max_blob_total = 100;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    let a = "win-a".to_string();
    let b = "win-b".to_string();
    h.call("sized_blob", json!({"n": 60}), Some(a.clone()), Some(false))
        .await
        .unwrap();
    h.call("sized_blob", json!({"n": 15}), Some(b.clone()), Some(false))
        .await
        .unwrap();
    // win-a over its reserve: ceiling = 100 - (50-15) = 65 → 60+10 > 65
    // while the pool would still admit 85 ≤ 100 → the SHARE name.
    let err = h
        .call("sized_blob", json!({"n": 10}), Some(a.clone()), Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "blob_bytes_share");
    // The small window's refusal at a drained pool names the pool.
    let err = h
        .call("sized_blob", json!({"n": 40}), Some(b.clone()), Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "blob_total");

    h.shutdown().await;
}

/// `release()` frees the handle slot: a call refused on `blob_handles`
/// succeeds after the client releases one.
#[tokio::test]
async fn release_frees_a_slot() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("release");
    let mut l = BlobLimits::from_env();
    l.max_blobs_per_window = 1;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    let v = h
        .call("make_blob", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap();
    let id = blob_id(&v).to_string();
    let err = h
        .call("make_blob", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "blob_handles");

    assert!(h.release_blob(&id, WIN).await.unwrap());
    h.call("make_blob", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap();

    h.shutdown().await;
}

/// A malformed register (path that never resolves) is a backend bug,
/// not a limit: the call surfaces INTERNAL, still not a silent 404.
#[tokio::test]
async fn bad_register_is_internal_not_404() {
    const FAKE: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"x","kind":"sync"}]}})
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
        t = m["params"].get("task")
        w({"jsonrpc":"2.0","method":"blob.register","params":{
            "id": "x", "path": "/nonexistent-dir/nothing.bin",
            "mime": "text/plain", "size": 1, "task_id": t}})
        w({"jsonrpc":"2.0","id": m["id"],
           "result": {"$blob": {"id": "x", "mime": "text/plain", "size": 1}}})
"#;
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("bad");
    let h = spawn_h(spec(FAKE), fast_config(&root, BlobLimits::from_env()), ev);
    wait_state(&h, State::Ready).await;

    let err = h
        .call("x", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap_err();
    assert_eq!(err.code, proto::INTERNAL, "wrong code: {err:?}");
    assert!(
        err.message.contains("blob.register"),
        "msg: {}",
        err.message
    );

    h.shutdown().await;
}

/// A rejection never leaks into a sibling task or the next call: two
/// back-to-back refused calls each get their own error, then a good
/// call is clean.
#[tokio::test]
async fn rejections_do_not_leak_between_tasks() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("leak");
    let mut l = BlobLimits::from_env();
    l.max_blob_bytes = 8;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    for _ in 0..2 {
        let err = h
            .call(
                "sized_blob",
                json!({"n": 16}),
                Some(WIN.into()),
                Some(false),
            )
            .await
            .unwrap_err();
        assert_limit(&err, "blob_bytes");
    }
    let out = h
        .call(
            "add",
            json!({"a": 2, "b": 3}),
            Some(WIN.into()),
            Some(false),
        )
        .await
        .unwrap();
    assert_eq!(out, json!(5));
    let v = h
        .call("sized_blob", json!({"n": 4}), Some(WIN.into()), Some(false))
        .await
        .unwrap();
    blob_id(&v);

    h.shutdown().await;
}
