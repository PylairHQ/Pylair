//! App readiness, PR 2 of docs/decisions/0007: the host side of
//! `app.component` — bounded latest-wins storage in `HealthShared`,
//! `health().app`, and `pylair://app` on phase transitions only.

use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, AppPhase, ExitKind, Handle, RuntimeEvent, RuntimeKind, SpawnSpec, State,
    SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

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

/// hello, a deterministic battery of `app.component` frames covering
/// every 0007 §4 rule, then the usual ping loop. Frame order is wire
/// order: by the time the runtime is Ready the whole battery is stored.
/// (A non-finite progress can't be probed end-to-end: a JSON number
/// that overflows f64, like `1e999`, is rejected at the transport as a
/// malformed frame — the arm's `is_finite` guard is defense in depth.)
const FAKE_APP_CORE: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
def comp(name, phase, **kw):
    p = {"name": name, "phase": phase}; p.update(kw)
    w({"jsonrpc":"2.0","method":"app.component","params":p})
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
comp("model", "loading", progress=0.0, detail="fetching weights")
comp("model", "loading", progress=0.5)
comp("model", "loading", progress=1.0, detail="almost there")
comp("model", "ready")
comp("index", "loading")
comp("index", "failed", detail="no space")
comp("n" * 80, "ready", detail="d" * 300)
comp("over", "loading", progress=1.7)
comp("under", "loading", progress=-0.2)
comp("texty", "loading", progress="half")
comp("objdetail", "ready", detail={"a": 1})
comp("badphase", "warming")
w({"jsonrpc":"2.0","method":"app.component","params":{"phase":"ready"}})
w({"jsonrpc":"2.0","method":"app.component","params":{"name":7,"phase":"ready"}})
w({"jsonrpc":"2.0","method":"app.component","params":"junk"})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"], "loop_stale_ms": 0}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// 40 components over the 32-name cap, then an update to an existing
/// name: the cap drops NEW names but never freezes existing ones
/// (0007 §4).
const FAKE_APP_CAP: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
for i in range(40):
    w({"jsonrpc":"2.0","method":"app.component","params":{"name":f"flood{i}","phase":"loading"}})
w({"jsonrpc":"2.0","method":"app.component","params":{"name":"flood0","phase":"ready","detail":"updated after the cap"}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"], "loop_stale_ms": 0}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// hello, one component report, immediate exit: the report belongs to
/// a dead generation and the restart re-reports as a first sighting.
const FAKE_APP_DIES: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
w({"jsonrpc":"2.0","method":"app.component","params":{"name":"gen","phase":"loading"}})
"#;

/// A ready bridge runtime that never sends `app.component`: the old
/// runtime shape — `health().app` stays null, never fabricated.
const FAKE_HEALTHY: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"], "loop_stale_ms": 0}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// A web runtime that (against contract) still sends `app.component`:
/// the launcher can never send these in practice, and if something
/// does, `health().app` still renders null on the web kind (0007 §6).
const FAKE_WEB_APP: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
w({"jsonrpc":"2.0","method":"app.component","params":{"name":"ghost","phase":"ready"}})
w({"jsonrpc":"2.0","method":"ready","params":{"url":"http://127.0.0.1:4242/"}})
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
    c.ping_interval = Duration::from_millis(50);
    c.missed_pongs_to_kill = 1000; // probe app state, not the kill path
    c.shutdown_grace = Duration::from_secs(2);
    c.cancel_timeout = Duration::from_secs(5);
    c
}

async fn wait_state(h: &Handle, want: State) {
    let mut rx = h.subscribe_state();
    timeout(T, async move {
        while *rx.borrow_and_update() != want {
            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for state {want:?}"));
}

/// Poll the pull snapshot — app frames land on the actor's schedule.
async fn eventually(cond: impl Fn() -> bool) -> bool {
    timeout(T, async {
        loop {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

fn pump(mut rx: mpsc::Receiver<RuntimeEvent>) -> Arc<Mutex<Vec<RuntimeEvent>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            sink.lock().unwrap().push(e);
        }
    });
    log
}

/// The `pylair://app` payloads seen so far, in arrival order.
fn app_events(log: &Arc<Mutex<Vec<RuntimeEvent>>>) -> Vec<serde_json::Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::AppChanged(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

fn events_for(log: &Arc<Mutex<Vec<RuntimeEvent>>>, name: &str) -> Vec<serde_json::Value> {
    app_events(log)
        .into_iter()
        .filter(|e| e["name"] == json!(name))
        .collect()
}

/// §4/§5: the battery lands in `health().app` with the snapshot shape,
/// phase transitions emit exactly one `pylair://app` each, progress
/// ticks emit nothing, and every malformed or out-of-contract frame is
/// dropped without hurting the runtime.
#[tokio::test]
async fn app_reports_land_in_health_and_transitions_emit() {
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_APP_CORE), fast_config(), events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;

    // All battery frames land before Ready's first pong arrives.
    assert!(
        eventually(|| h
            .health(None)
            .app
            .as_ref()
            .is_some_and(|a| a.components.len() == 7))
        .await,
        "the app battery never landed: {:?}",
        h.health(None).app
    );
    let s = h.health(None);
    let app = s.app.as_ref().unwrap();

    let model = &app.components["model"];
    assert_eq!(model.phase, AppPhase::Ready);
    assert_eq!(model.progress, None, "the ready frame carried none");
    assert_eq!(model.detail, None);
    let index = &app.components["index"];
    assert_eq!(index.phase, AppPhase::Failed);
    assert_eq!(index.detail.as_deref(), Some("no space"));

    // Bounds: 80-char name truncates to 64, 300-char detail to 256.
    let long = &app.components[&"n".repeat(64)];
    assert_eq!(long.phase, AppPhase::Ready);
    assert_eq!(long.detail.as_deref(), Some("d".repeat(256).as_str()));
    assert!(!app.components.contains_key(&"n".repeat(80)));

    // Out-of-range and non-numeric progress stores as null.
    for (name, phase) in [
        ("over", AppPhase::Loading),
        ("under", AppPhase::Loading),
        ("texty", AppPhase::Loading),
        ("objdetail", AppPhase::Ready),
    ] {
        let c = &app.components[name];
        assert_eq!(c.phase, phase, "{name}");
        assert_eq!(c.progress, None, "{name} progress must be null");
    }
    assert_eq!(app.components["objdetail"].detail, None);

    // Dropped outright: bad phase, missing/non-string name, non-object.
    for gone in ["badphase", "7", "junk", ""] {
        assert!(
            !app.components.contains_key(gone),
            "{gone:?} must not be stored"
        );
    }

    // §5 payloads: versioned, generation-scoped, name + new phase.
    for e in app_events(&log) {
        assert_eq!(e["version"], json!(1));
        assert_eq!(e["generation"], json!(0));
        assert!(e["name"].is_string() && e["phase"].is_string());
    }
    // Exactly one event per transition — progress ticks emit nothing.
    let model_evs = events_for(&log, "model");
    assert_eq!(model_evs.len(), 2, "{model_evs:?}");
    assert_eq!(model_evs[0]["phase"], json!("loading"));
    assert_eq!(model_evs[1]["phase"], json!("ready"));
    let index_evs = events_for(&log, "index");
    assert_eq!(index_evs.len(), 2, "{index_evs:?}");
    assert_eq!(index_evs[0]["phase"], json!("loading"));
    assert_eq!(index_evs[1]["phase"], json!("failed"));
    assert_eq!(events_for(&log, &"n".repeat(64)).len(), 1);
    // 2+2+1+3+1 = 9 transitions total; nothing else ever emits.
    assert_eq!(app_events(&log).len(), 9, "{:?}", app_events(&log));

    // The serde names are the v1 schema — assert the boundary shape.
    let j = serde_json::to_value(&s).unwrap();
    let comps = &j["app"]["components"];
    assert_eq!(comps["model"]["phase"], json!("ready"));
    assert!(comps["model"]["progress"].is_null());
    assert!(comps["model"]["detail"].is_null());
    assert!(comps["model"]["ms_ago"].is_u64());
    assert_eq!(comps["index"]["detail"], json!("no space"));
    h.shutdown().await;
}

/// §4: the map caps at 32 names per generation — the 33rd new name is
/// dropped, but an existing name keeps updating (phase and detail).
#[tokio::test]
async fn the_component_cap_drops_new_names_not_updates() {
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_APP_CAP), fast_config(), events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;

    assert!(
        eventually(|| h
            .health(None)
            .app
            .as_ref()
            .is_some_and(|a| a.components.len() == 32))
        .await,
        "the flood never filled the map: {:?}",
        h.health(None).app
    );
    let s = h.health(None);
    let app = s.app.as_ref().unwrap();
    assert_eq!(app.components.len(), 32);
    assert!(app.components.contains_key("flood31"));
    assert!(!app.components.contains_key("flood32"));
    // flood0's post-cap update landed — the cap drops new names only.
    let flood0 = &app.components["flood0"];
    assert_eq!(flood0.phase, AppPhase::Ready);
    assert_eq!(flood0.detail.as_deref(), Some("updated after the cap"));
    // 32 first-sighting events + flood0's transition = 33.
    assert_eq!(events_for(&log, "flood0").len(), 2);
    assert_eq!(app_events(&log).len(), 33, "{:?}", app_events(&log));
    h.shutdown().await;
}

/// §5: app state is per-generation — teardown clears it. With the
/// breaker parking after one crash the map must read null forever
/// after the generation dies, not show the dead one's report.
#[tokio::test]
async fn app_state_clears_when_the_generation_dies() {
    let mut c = fast_config();
    c.breaker_crashes = 1; // park `failed` after the first crash
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_APP_DIES), c, events);
    let log = pump(rx);

    // The event proves the report landed — the map itself lives only
    // until EOF (microseconds), so polling it would race the teardown.
    assert!(
        eventually(|| app_events(&log).len() == 1).await,
        "the generation's report never landed"
    );
    wait_state(&h, State::Failed).await;
    let s = h.health(None);
    assert!(
        s.app.is_none(),
        "a dead generation must not leak its app map: {:?}",
        s.app
    );
    assert_eq!(s.restarts.crashes_in_window, 1);
    assert_eq!(
        s.restarts.last_exit.as_ref().map(|e| e.kind),
        Some(ExitKind::Crashed)
    );
    assert_eq!(app_events(&log).len(), 1, "{:?}", app_events(&log));
    h.shutdown().await;
}

/// §5: a restarted generation reports independently — the same
/// component name arriving again is a first sighting of a NEW
/// generation and emits again (had the map survived, its loading ->
/// loading update would emit nothing). Generations in the payloads
/// climb past 0.
#[tokio::test]
async fn a_new_generation_reports_app_state_fresh() {
    let mut c = fast_config();
    c.breaker_crashes = 10; // keep restarting — never park `failed`
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_APP_DIES), c, events);
    let log = pump(rx);

    assert!(
        eventually(|| {
            let evs = app_events(&log);
            evs.len() >= 2 && evs.iter().any(|e| e["generation"] != json!(0))
        })
        .await,
        "the restarted generation never re-reported: {:?}",
        app_events(&log)
    );
    let evs = app_events(&log);
    assert_eq!(evs[0]["generation"], json!(0));
    assert_eq!(evs[0]["name"], json!("gen"));
    assert_eq!(evs[0]["phase"], json!("loading"));
    let later = &evs[1];
    assert_eq!(later["name"], json!("gen"));
    assert_eq!(later["phase"], json!("loading"));
    assert!(later["generation"].as_u64().unwrap() >= 1);
    h.shutdown().await;
}

/// §5: an old runtime that never sends `app.component` leaves
/// `health().app` null — never a fabricated empty map.
#[tokio::test]
async fn app_is_null_when_the_runtime_never_reports() {
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HEALTHY), fast_config(), events);
    wait_state(&h, State::Ready).await;
    // A few ping intervals past Ready: nothing ever reported.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let s = h.health(None);
    assert!(s.app.is_none(), "app must be null, not an empty map");
    let j = serde_json::to_value(&s).unwrap();
    assert!(j["app"].is_null(), "the wire field is null, not absent");
    h.shutdown().await;
}

/// §6: the web kind stays null — the wrapped app's launcher cannot
/// send `app.component`, and even a contract-violating report is never
/// surfaced as app state on `web`.
#[tokio::test]
async fn app_is_null_on_the_web_kind() {
    let (events, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.kind = RuntimeKind::Web;
    c.ready_timeout = Duration::from_secs(5);
    let h = spawn_h(spec(FAKE_WEB_APP), c, events);
    wait_state(&h, State::Ready).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let s = h.health(None);
    assert!(
        s.app.is_none(),
        "the web kind must never surface app state: {:?}",
        s.app
    );
    h.shutdown().await;
}
