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

/// Registered-handle count in the shared registry.
fn live_blobs(h: &Handle) -> usize {
    tauri_plugin_pylair::blobs::lock_blobs(&h.blobs()).len()
}

/// Whether `id` is still registered.
fn blob_registered(h: &Handle, id: &str) -> bool {
    tauri_plugin_pylair::blobs::lock_blobs(&h.blobs()).has(id)
}

/// `root` rendered as an indented `dir/` + `file` tree — interpolated
/// into assert messages so a stray dir or file explains itself.
fn dir_tree(root: &std::path::Path) -> String {
    fn walk(dir: &std::path::Path, depth: usize, out: &mut String) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        entries.sort();
        for p in entries {
            out.push_str(&"  ".repeat(depth));
            out.push_str(&p.file_name().unwrap_or_default().to_string_lossy());
            if p.is_dir() {
                out.push_str("/\n");
                walk(&p, depth + 1, out);
            } else {
                out.push('\n');
            }
        }
    }
    let mut out = String::new();
    walk(root, 0, &mut out);
    out
}

/// Files inside the current generation's `gen-N` dir under the blob
/// root. These tests spawn one runtime each, so the root must hold
/// exactly one `gen-*` dir — a second is residue a Windows teardown
/// could not unlink while a file was still open (or a swept leftover),
/// and reading it instead of the live one counts the wrong directory.
/// The count uses the highest N (the current generation) and the whole
/// tree prints on a mismatch so the failure explains itself.
fn gen_dir_files(root: &std::path::Path) -> usize {
    let gens: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("gen-"))
        })
        .collect();
    assert_eq!(
        gens.len(),
        1,
        "expected exactly one gen dir under {}:\n{}",
        root.display(),
        dir_tree(root),
    );
    let gen = gens
        .into_iter()
        .max_by_key(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .strip_prefix("gen-")
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
        })
        .unwrap();
    std::fs::read_dir(&gen)
        .unwrap()
        .filter(|e| e.as_ref().map(|x| x.path().is_file()).unwrap_or(false))
        .count()
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
    // The rejected payload was unlinked at the post-containment
    // rejection — an over-cap result must not leak uncounted disk.
    assert_eq!(gen_dir_files(&root), 0, "rejected payload must be unlinked");

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

/// Orphan release: a frame can carry several `$blob` tags — when the
/// third register breaks the cap, the two that already registered must
/// be released (they never reach the page), leaving the window's handle
/// count where it was before the call.
#[tokio::test]
async fn orphan_ids_released_on_replaced_response() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("orphan-call");
    let mut l = BlobLimits::from_env();
    l.max_blobs_per_window = 3;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    // One held handle — this one stays live through the whole test.
    let v = h
        .call("make_blob", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap();
    let keep = blob_id(&v).to_string();
    assert_eq!(live_blobs(&h), 1);

    // three_blobs: registers 1 and 2 land (2, 3 live), the third
    // breaks the cap — the error replaces the frame and the two
    // registered-but-undelivered ids must be released.
    let err = h
        .call("three_blobs", json!({}), Some(WIN.into()), Some(false))
        .await
        .unwrap_err();
    assert_limit(&err, "blob_handles");
    assert_eq!(
        live_blobs(&h),
        1,
        "orphaned handles must be released with the frame"
    );
    assert!(
        blob_registered(&h, &keep),
        "the pre-existing handle must still be live"
    );
    // Every orphan file is gone: the released ids' payloads are
    // unlinked by the registry, and the rejected register's own
    // payload was unlinked at the post-containment rejection — only
    // the still-live `keep` blob's file remains.
    assert_eq!(gen_dir_files(&root), 1, "orphan files must be unlinked");

    h.shutdown().await;
}

/// Same rule on the stream path: the refused chunk's earlier registers
/// are released, while ids from chunks that already delivered stay
/// live — the page genuinely holds them.
#[tokio::test]
async fn orphan_ids_released_on_replaced_chunk() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(4096);
    let root = blob_dir("orphan-stream");
    let mut l = BlobLimits::from_env();
    l.max_blobs_per_window = 3;
    let h = spawn_h(real_runtime_spec(), fast_config(&root, l), ev);
    wait_state(&h, State::Ready).await;

    // stream_mixed_blobs: chunk 1 carries one blob (delivered, stays
    // live), chunk 2 carries three — its first two register (2, 3
    // live), the third breaks the cap.
    let (_task, mut rx) = h
        .call_stream(
            "stream_mixed_blobs",
            json!({}),
            Some(WIN.into()),
            Some(true),
        )
        .await
        .unwrap();
    let first = match timeout(T, rx.recv()).await.unwrap().unwrap() {
        StreamItem::Chunk { value, .. } => value,
        StreamItem::End(res) => panic!("stream ended early: {res:?}"),
    };
    let keep = first[0]["$blob"]["id"]
        .as_str()
        .expect("chunk 1 must carry a $blob tag")
        .to_string();
    assert_eq!(live_blobs(&h), 1);

    let end = loop {
        match timeout(T, rx.recv()).await.unwrap().unwrap() {
            StreamItem::End(res) => break res,
            StreamItem::Chunk { .. } => {}
        }
    };
    let err = end.unwrap_err();
    assert_limit(&err, "blob_handles");

    // The refused chunk's two registered ids are released; chunk 1's
    // handle — already delivered — stays live. One file on disk: the
    // rejected register's payload was unlinked at the post-containment
    // rejection.
    assert_eq!(live_blobs(&h), 1);
    assert!(blob_registered(&h, &keep));
    assert_eq!(gen_dir_files(&root), 1);

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
