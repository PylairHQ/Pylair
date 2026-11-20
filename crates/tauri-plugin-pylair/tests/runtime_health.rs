//! Runtime health, PR 1 of docs/decisions/0006: `HealthShared` +
//! `Handle::health`. The snapshot sees what the supervisor knows —
//! before any `python_health` command exposes it.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, ExitKind, Handle, Limits, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
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

/// hello + seq-echoing pongs, `loop_stale_ms: 0`: the baseline
/// liveness shape — transport ok, loop ok, RTT measurable.
const FAKE_HEALTHY: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hold","kind":"sync"}]}})
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

/// Same shape but every pong reports `loop_stale_ms: 2500`: the
/// runtime answers pings while its event loop is blocked — the
/// busy-vs-hung split the health verdicts exist to name.
const FAKE_STALE_LOOP: &str = r#"
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
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"], "loop_stale_ms": 2500}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// Pongs WITHOUT a `seq`: last_pong_age updates but RTT can never be
/// measured — it must stay null, never fabricated.
const FAKE_NOSEQ_PONG: &str = r#"
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
        w({"jsonrpc":"2.0","method":"pong","params":{"loop_stale_ms": 0}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// Reads every frame so the pipe never backpressures, answers
/// shutdown, and NEVER pongs: missed pings climb at tick cadence until
/// the kill path trips.
const FAKE_NO_PONG: &str = r#"
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
    if m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    # pings and calls are read but never answered
"#;

/// hello, then immediate exit: every generation dies before it can
/// serve anything — crash accounting under test.
const FAKE_DIES_AFTER_HELLO: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
"#;

/// hello, pongs, and NEVER answers calls: every admitted call holds
/// its task slot — the fixture for the `load` totals.
const FAKE_HOLDS_TASKS: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hold","kind":"sync"}]}})
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

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: None,
        ..Default::default()
    }
}

/// Fast pings: liveness assertions land in ~100ms instead of the
/// default 5s tick. `missed_pongs_to_kill` small enough that a dead
/// runtime dies promptly.
fn fast_config() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(5);
    c.ping_interval = Duration::from_millis(50);
    c.missed_pongs_to_kill = 4;
    c.shutdown_grace = Duration::from_secs(2);
    c.cancel_timeout = Duration::from_secs(5);
    c.limits = Limits::default().with_max_concurrent_tasks(4);
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

/// Health is a pull snapshot; fields settle on the actor's own
/// schedule — poll instead of sleeping a fixed time.
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

/// A ready bridge runtime: the §4 schema fields all present with
/// correct null/absent rules, verdicts ok.
#[tokio::test]
async fn baseline_health_is_ok_when_ready() {
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HEALTHY), fast_config(), events);
    wait_state(&h, State::Ready).await;
    // First pong's landing is async: wait until liveness is populated.
    assert!(
        eventually(|| h.health(None).liveness.last_pong_age_ms.is_some()).await,
        "no pong ever reached health"
    );
    let s = h.health(None);
    assert_eq!(s.version, 1);
    assert_eq!(s.kind, "bridge");
    assert_eq!(s.state, "ready");
    assert_eq!(s.generation, 0);
    assert!(s.uptime_ms.is_some(), "a Ready generation has uptime");
    assert!(s.url.is_none(), "the bridge kind carries no url field");
    assert_eq!(s.liveness.transport, "ok");
    assert_eq!(s.liveness.loop_verdict, "ok");
    assert_eq!(s.liveness.missed_pongs, 0);
    assert_eq!(s.liveness.missed_pongs_to_kill, 4);
    assert!(
        s.liveness.rtt_ms.is_some(),
        "an echoed seq gives a measured RTT"
    );
    assert_eq!(s.liveness.loop_stale_ms, Some(Some(0)));
    assert!(s.restarts.last_exit.is_none());
    assert_eq!(s.restarts.crashes_in_window, 0);
    assert_eq!(s.restarts.breaker.crashes, 5);
    assert_eq!(s.restarts.breaker.window_ms, 60_000);
    assert!(s.self_usage.is_none(), "a host-side read has no self row");
    // The serde names are the v1 schema — assert the boundary shape.
    let j = serde_json::to_value(&s).unwrap();
    assert_eq!(j["liveness"]["loop"], "ok");
    assert!(
        j.get("url").is_none(),
        "url is absent, not null, for bridge"
    );
    assert!(j.get("self").is_some_and(|v| v.is_null()), "self is null");
    assert!(j.get("load").is_some_and(|v| v.is_object()));
}

/// `loop_stale_ms: 2500` pongs: transport stays ok but the loop
/// verdict goes busy — and the existing Unresponsive diagnostic is
/// unchanged.
#[tokio::test]
async fn stalled_loop_reports_busy_and_unresponsive() {
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_STALE_LOOP), fast_config(), events);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.state == "unresponsive" && s.liveness.loop_stale_ms == Some(Some(2500))
        })
        .await,
        "the stale-loop pong never surfaced"
    );
    let s = h.health(None);
    assert_eq!(s.liveness.loop_verdict, "busy", "2.5s of staleness is busy");
    assert_eq!(h.state(), State::Unresponsive);
    assert_eq!(s.state, "unresponsive");
    assert_eq!(
        s.liveness.transport, "ok",
        "pongs still answer — only the loop is blocked"
    );
}

/// A runtime that never pongs: misses climb at tick cadence
/// (transport: missing) until the existing kill path trips.
#[tokio::test]
async fn missing_pongs_report_missing_then_kill() {
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_NO_PONG), fast_config(), events);
    wait_state(&h, State::Ready).await;
    // Misses climb one per tick: assert two distinct rungs, each while
    // the snapshot still reads transport:missing.
    assert!(
        eventually(|| h.health(None).liveness.missed_pongs >= 1).await,
        "the first miss never registered"
    );
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.liveness.transport == "missing" && s.liveness.missed_pongs >= 2
        })
        .await,
        "misses never surfaced as transport:missing"
    );
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.state == "unresponsive" && s.liveness.transport == "missing"
        })
        .await,
        "missed pongs never surfaced the existing diagnostic"
    );
    // …then the existing kill path: last_exit crashed, crash counted,
    // generation bumped — and liveness goes unknown outside the live
    // generation.
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.restarts
                .last_exit
                .as_ref()
                .is_some_and(|e| e.kind == ExitKind::Crashed)
                && s.restarts.crashes_in_window >= 1
                && s.generation >= 1
        })
        .await,
        "the wedged runtime was never killed"
    );
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.restarts.last_exit.is_some() && s.liveness.transport == "unknown"
        })
        .await,
        "the post-kill window never read liveness as unknown"
    );
    let s = h.health(None);
    let last = s.restarts.last_exit.as_ref().unwrap();
    assert_eq!(last.kind, ExitKind::Crashed);
    assert!(
        last.detail.contains("missed pongs"),
        "detail should name the wedge: {:?}",
        last.detail
    );
}

/// An echoed seq measures an RTT; an absent seq must never fake one.
#[tokio::test]
async fn echoed_seq_gives_rtt_unmatched_seq_gives_none() {
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HEALTHY), fast_config(), events);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| h.health(None).liveness.rtt_ms.is_some()).await,
        "an echoed seq never produced an RTT"
    );

    let (events2, _rx2) = mpsc::channel(64);
    let h2 = spawn_h(spec(FAKE_NOSEQ_PONG), fast_config(), events2);
    wait_state(&h2, State::Ready).await;
    assert!(
        eventually(|| h2.health(None).liveness.last_pong_age_ms.is_some()).await,
        "no pong ever arrived"
    );
    let s = h2.health(None);
    assert!(
        s.liveness.rtt_ms.is_none(),
        "a pong without seq must not fabricate an RTT"
    );
    assert_eq!(s.liveness.transport, "ok");
}

/// Crash + restart: `last_exit` records the cause, the crash lands in
/// the breaker window, the generation bumps, and the new generation's
/// uptime starts over.
#[tokio::test]
async fn crash_restart_records_last_exit_and_bumps_generation() {
    let mut c = fast_config();
    c.breaker_crashes = 10; // keep restarting — never park `failed`
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_DIES_AFTER_HELLO), c, events);
    assert!(
        eventually(|| h.health(None).restarts.crashes_in_window == 1).await,
        "the first crash never landed in the breaker window"
    );
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.restarts
                .last_exit
                .as_ref()
                .is_some_and(|e| e.kind == ExitKind::Crashed && !e.detail.is_empty())
                && s.generation >= 1
        })
        .await,
        "crash accounting never landed"
    );
    // Uptime is per-generation: between the crash and the new Ready it
    // must be null, not the dead generation's.
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.generation >= 1 && s.uptime_ms.is_none()
        })
        .await,
        "the new generation still showed the dead one's uptime"
    );
}

/// `per_window_fairness` off: `self` is null outright, but the global
/// `load` totals still count — the unconditional-`totals` choice of
/// decisions 0006 §1.
#[tokio::test]
async fn fairness_off_hides_self_but_load_totals_still_count() {
    let mut c = fast_config();
    c.ping_interval = Duration::from_secs(60); // calls, not liveness
    c.missed_pongs_to_kill = 1000;
    c.limits = Limits::default()
        .with_max_concurrent_tasks(4)
        .with_per_window_fairness(false);
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HOLDS_TASKS), c, events);
    wait_state(&h, State::Ready).await;
    let task = h
        .call_task("hold", json!({}), Some("win-a".into()), None)
        .expect("call_task should admit");
    assert!(
        eventually(|| h.health(None).load.tasks == 1).await,
        "the held call never reached load.tasks"
    );
    let s = h.health(Some("win-a"));
    assert!(
        s.self_usage.is_none(),
        "self must be null when per_window_fairness is off"
    );
    assert_eq!(s.load.tasks, 1, "the held call still counts globally");
    assert_eq!(s.load.queued, 0);
    assert_eq!(s.load.streams, 0);
    assert_eq!(s.load.isolated, 0);
    drop(task);
}

/// Caller-scoped `self`: a second live owner shrinks every ceiling by
/// the same decisions-0005 rule the admission checks use.
#[tokio::test]
async fn a_second_owner_shrinks_self_ceilings() {
    let mut c = fast_config();
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HOLDS_TASKS), c, events);
    wait_state(&h, State::Ready).await;
    h.fairness_join_window("win-a");
    let solo = h.health(Some("win-a")).self_usage.unwrap();
    assert_eq!(solo.window, "win-a");
    assert_eq!(solo.tasks.ceiling, 4, "one live window gets the cap");
    h.fairness_join_window("win-b");
    let sa = h.health(Some("win-a")).self_usage.unwrap();
    assert_eq!(
        sa.tasks.ceiling, 3,
        "win-b's reserve shrank win-a's task ceiling"
    );
    assert_eq!(sa.tasks.cap, 4);
    assert_eq!(sa.tasks.in_use, 0);
    let sb = h.health(Some("win-b")).self_usage.unwrap();
    assert_eq!(sb.window, "win-b");
    assert_eq!(sb.tasks.ceiling, 3);
    // Every resource row exists — the caps are the Limits::default()
    // values beside the tasks cap the fixture set.
    assert_eq!(sa.queued.cap, 64);
    assert_eq!(sa.streams.cap, 32);
    assert_eq!(sa.isolated.cap, 8);
}
