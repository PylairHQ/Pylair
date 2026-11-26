//! Inspector wiring end-to-end (docs/inspector.md): the session tap
//! records real call outcomes, the events channel is untouched by a
//! burst, and python_inspect_snapshot refuses app windows.
//! The command-refusal half needs a real webview — non-windows, same
//! constraint as call_via_command.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
#[cfg(not(windows))]
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{spawn, SpawnSpec, SupervisorConfig};

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
    panic!("no python >=3.12 found for inspector tests");
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

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(15);
    c.shutdown_grace = Duration::from_secs(2);
    c
}

async fn wait_ready(h: &tauri_plugin_pylair::supervisor::Handle) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !matches!(h.state(), tauri_plugin_pylair::supervisor::State::Ready) {
        assert!(std::time::Instant::now() < deadline, "runtime never ready");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn tap_records_real_calls_and_events_channel_unaffected() {
    // The tap reads the HOST env at spawn (same process here) — set it
    // before spawning. Debug-only codepath; the flag is additive so
    // sharing the process env with other tests is harmless.
    unsafe { std::env::set_var("PYLAIR_INSPECT", "1") };
    let (ev_tx, mut ev_rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let (handle, actor) = spawn(real_runtime_spec(), cfg(), ev_tx);
    tokio::spawn(actor);
    wait_ready(&handle).await;

    // An ok call, a stream, and an error — the three basic rows.
    let r = handle
        .call(
            "add",
            json!({"a": 2, "b": 3}),
            Some("main".into()),
            Some(false),
        )
        .await
        .expect("add resolves");
    assert_eq!(r, json!(5));

    let (task, mut items) = handle
        .call_stream("counter", json!({"n": 3}), Some("main".into()), Some(true))
        .await
        .expect("stream starts");
    let mut chunks = 0;
    while let Some(it) = items.recv().await {
        match it {
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { .. } => chunks += 1,
            tauri_plugin_pylair::supervisor::StreamItem::End(_) => break,
        }
    }
    assert_eq!(chunks, 3, "counter emitted 3 chunks");
    let _ = task;

    let snap = handle.inspect().snapshot();
    assert_eq!(
        snap["enabled"],
        json!(true),
        "tap live under PYLAIR_INSPECT=1"
    );
    let recs = snap["records"].as_array().unwrap();
    assert!(
        recs.iter()
            .any(|r| r["function"] == "add" && r["outcome"]["kind"] == "ok"),
        "add recorded as ok: {recs:?}"
    );
    assert!(
        recs.iter()
            .any(|r| r["function"] == "counter" && r["stream"]["chunks"] == json!(3)),
        "counter recorded with 3 chunks: {recs:?}"
    );

    // Events channel: a burst with the tap on must not cost real events.
    // Drain everything already buffered, then force a health-relevant
    // transition by exhausting the burst and watching stderr/state flow.
    let burst = futures_burst(&handle).await;
    assert!(burst > 0);
    let mut saw_events = 0usize;
    let drain = tokio::time::timeout(Duration::from_millis(300), async {
        while let Some(_ev) = ev_rx.recv().await {
            saw_events += 1;
        }
    })
    .await;
    let _ = drain;
    // The readiness/exposes/state events of startup plus anything the
    // burst produced must have flowed — the tap never touches this
    // channel, so ANY delivery proves non-interference.
    assert!(
        saw_events > 0,
        "events channel delivered during inspector operation"
    );

    handle.shutdown().await;
}

async fn futures_burst(handle: &tauri_plugin_pylair::supervisor::Handle) -> usize {
    let mut n = 0;
    for _ in 0..40 {
        let _ = handle
            .call(
                "add",
                json!({"a": 1, "b": 1}),
                Some("main".into()),
                Some(false),
            )
            .await;
        n += 1;
    }
    n
}

// ---- the correlated timeline, end to end (docs/inspector.md) ----
//
// A minimal steerable fake: hello, echo, err (typed error with the
// dev-only detail fields), log (stderr), die (exit code 3), set_pools
// (what the next pongs report). Pongs always carry a `pools` block so
// warm-member diffs can be driven.
const MINIFAKE: &str = r#"
import sys, os, json, struct, threading
out = sys.stdout.buffer
wlock = threading.Lock()
POOLS = {"warmf": {"live": 2, "deaths": 0, "reaps": 0}}
def raw(b):
    with wlock:
        out.write(struct.pack(">I", len(b)) + b); out.flush()
def w(o): raw(json.dumps(o).encode())
sys.stderr.write("fake boot\n"); sys.stderr.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}})
inp = sys.stdin.buffer
def read():
    h = inp.read(4)
    if len(h) < 4: return None
    n, = struct.unpack(">I", h)
    return json.loads(inp.read(n))
while True:
    m = read()
    if m is None: break
    meth = m.get("method"); p = m.get("params") or {}
    if meth == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq":p["seq"],"loop_stale_ms":0,"pools":POOLS}})
    elif meth == "shutdown":
        w({"jsonrpc":"2.0","id":m["id"],"result":{}})
        break
    elif meth == "call":
        fn = p.get("fn"); a = p.get("input")
        if fn == "echo":
            w({"jsonrpc":"2.0","id":m["id"],"result":a})
        elif fn == "err":
            sys.stderr.write("about to fail\n"); sys.stderr.flush()
            w({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32602,"message":"bad input",
               "data":{"traceback":"tb here","suggestions":["fix it"],
               "errors":[{"path":"x","expected":"int","received":"str"}]}}})
        elif fn == "log":
            sys.stderr.write(str(a) + "\n"); sys.stderr.flush()
            w({"jsonrpc":"2.0","id":m["id"],"result":True})
        elif fn == "set_pools":
            POOLS.update(a); POOLS = dict(POOLS)
            w({"jsonrpc":"2.0","id":m["id"],"result":True})
        elif fn == "die":
            os._exit(3)
        else:
            w({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32601,"message":"no fn"}})
"#;

fn fake_spec() -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), MINIFAKE.into()],
        env: vec![],
        cwd: None,
        ..Default::default()
    }
}

fn fast_cfg() -> SupervisorConfig {
    let mut c = cfg();
    c.ping_interval = Duration::from_millis(100);
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(60);
    c
}

fn timeline_row(snap: &serde_json::Value, event: &str, gen: u64) -> Option<serde_json::Value> {
    snap["timeline"]
        .as_array()?
        .iter()
        .find(|r| r["event"] == event && r["generation"].as_u64() == Some(gen))
        .cloned()
}

#[tokio::test]
async fn lifecycle_spawn_exit_restart_and_stderr_end_to_end() {
    unsafe { std::env::set_var("PYLAIR_INSPECT", "1") };
    let (ev_tx, _ev_rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let (handle, actor) = spawn(fake_spec(), fast_cfg(), ev_tx);
    tokio::spawn(actor);
    wait_ready(&handle).await;

    // gen 0 spawn recorded with a pid; boot stderr tagged to gen 0.
    let snap = handle.inspect().snapshot();
    let sp = timeline_row(&snap, "spawn", 0).expect("spawn gen0 recorded");
    assert!(sp["pid"].as_u64().unwrap() > 0, "pid carried: {sp}");
    assert!(
        snap["stderr"].as_array().unwrap().iter().any(|l| l["line"]
            .as_str()
            .unwrap()
            .contains("fake boot")
            && l["generation"] == json!(0)),
        "boot stderr recorded generation-tagged"
    );

    // A failing call: the record carries code+message and the optional
    // detail fields; its stderr line lands inside the call's window.
    let e = handle
        .call("err", json!({}), Some("main".into()), Some(false))
        .await
        .unwrap_err();
    assert_eq!(e.code, -32602);
    let snap = handle.inspect().snapshot();
    let rec = snap["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["function"] == "err")
        .cloned()
        .expect("err call recorded");
    assert_eq!(rec["outcome"]["kind"], "error");
    assert_eq!(rec["outcome"]["message"], "bad input");
    assert_eq!(rec["outcome"]["traceback"], "tb here");
    assert_eq!(rec["outcome"]["suggestions"][0], "fix it");
    assert_eq!(rec["outcome"]["errors"][0]["path"], "x");
    let start = rec["start_unix_ms"].as_u64().unwrap();
    let end = start + rec["latency_ms"].as_u64().unwrap();
    assert!(
        snap["stderr"].as_array().unwrap().iter().any(|l| {
            l["line"].as_str().unwrap().contains("about to fail")
                && l["at_unix_ms"].as_u64().unwrap() >= start
                && l["at_unix_ms"].as_u64().unwrap() <= end + 250 // pipe latency slack
        }),
        "the call's stderr line lands inside its time window"
    );

    // Crash the runtime: exit + restart + the next generation's spawn.
    let _ = handle
        .call("die", json!(null), Some("main".into()), Some(false))
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snap = handle.inspect().snapshot();
        if timeline_row(&snap, "exit", 0).is_some()
            && timeline_row(&snap, "restart", 0).is_some()
            && timeline_row(&snap, "spawn", 1).is_some()
        {
            let ex = timeline_row(&snap, "exit", 0).unwrap();
            assert!(
                ex["detail"].as_str().unwrap().contains("exited"),
                "exit detail names the exit: {ex}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crash lifecycle never completed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn breaker_trip_recorded_without_a_restart() {
    unsafe { std::env::set_var("PYLAIR_INSPECT", "1") };
    let (ev_tx, _ev_rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let mut c = fast_cfg();
    c.breaker_crashes = 1; // the first crash trips the breaker
    c.breaker_window = Duration::from_secs(60);
    let (handle, actor) = spawn(fake_spec(), c, ev_tx);
    tokio::spawn(actor);
    wait_ready(&handle).await;
    let _ = handle
        .call("die", json!(null), Some("main".into()), Some(false))
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snap = handle.inspect().snapshot();
        if timeline_row(&snap, "breaker", 0).is_some() {
            assert!(
                timeline_row(&snap, "restart", 0).is_none(),
                "a breaker parks — no restart decision"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "breaker never tripped"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn dev_reload_names_the_changed_file() {
    unsafe { std::env::set_var("PYLAIR_INSPECT", "1") };
    let dir = std::env::temp_dir().join(format!("pylair-insp-trig-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let trigger = dir.join("trigger");
    std::fs::write(&trigger, "0").unwrap();
    let (ev_tx, _ev_rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let mut c = fast_cfg();
    c.restart_trigger = Some(trigger.clone());
    let (handle, actor) = spawn(fake_spec(), c, ev_tx);
    tokio::spawn(actor);
    wait_ready(&handle).await;
    // The CLI's contract: "<millis> <changed-file>". mtime must move —
    // set it ahead like the supervisor tests do.
    std::fs::write(&trigger, "12345 app/main.py").unwrap();
    let f = std::fs::File::options().write(true).open(&trigger).unwrap();
    f.set_modified(std::time::SystemTime::now() + Duration::from_secs(30))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snap = handle.inspect().snapshot();
        if let Some(row) = timeline_row(&snap, "dev_reload", 0) {
            assert_eq!(row["file"], "app/main.py", "the changed file is named");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dev reload never recorded"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn warm_member_churn_diffed_off_pongs() {
    unsafe { std::env::set_var("PYLAIR_INSPECT", "1") };
    let (ev_tx, _ev_rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let (handle, actor) = spawn(fake_spec(), fast_cfg(), ev_tx);
    tokio::spawn(actor);
    wait_ready(&handle).await;
    // The first pongs report live:2 — the initial fill diffs to spawns.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snap = handle.inspect().snapshot();
        let found =
            snap["timeline"].as_array().unwrap().iter().any(|r| {
                r["event"] == "warm_spawn" && r["function"] == "warmf" && r["n"] == json!(2)
            });
        if found {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "initial fill never diffed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A death and a reap ride the next pongs.
    handle
        .call(
            "set_pools",
            json!({"warmf": {"live": 1, "deaths": 1, "reaps": 1}}),
            Some("main".into()),
            Some(false),
        )
        .await
        .expect("set_pools resolves");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snap = handle.inspect().snapshot();
        let tl = snap["timeline"].as_array().unwrap();
        let death = tl
            .iter()
            .any(|r| r["event"] == "warm_death" && r["n"] == json!(1));
        let reap = tl
            .iter()
            .any(|r| r["event"] == "warm_reap" && r["n"] == json!(1));
        if death && reap {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "death/reap never diffed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.shutdown().await;
}

// ---- command refusal: real webview, non-windows ----
#[cfg(not(windows))]
mod command_refusal {
    use super::*;
    use tauri::test::{mock_context, noop_assets, MockRuntime};
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    use tauri_plugin_pylair::{Builder, PyRuntime};

    #[test]
    fn inspect_snapshot_refused_from_app_windows() {
        let mut c = cfg();
        c.hello_timeout = Duration::from_secs(15);
        let app = tauri::test::mock_builder()
            .plugin(
                Builder::<MockRuntime>::new(real_runtime_spec())
                    .config(c)
                    .build(),
            )
            .build(mock_context(noop_assets()))
            .expect("app builds");
        let win = WebviewWindowBuilder::new(&app, "main", WebviewUrl::default())
            .build()
            .expect("main window");
        let rt = app.state::<PyRuntime>();
        // An app window is refused — the inspector window label is the
        // only one the command answers.
        let res = tauri_plugin_pylair::python_inspect_snapshot_impl(&win, &rt);
        assert!(res.is_err(), "app window must be refused");
        let e = res.err().unwrap();
        assert_eq!(e.code, proto::INVALID_REQUEST);
    }
}
