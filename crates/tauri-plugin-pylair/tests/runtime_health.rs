//! Runtime health, PR 1 of docs/decisions/0006: `HealthShared` +
//! `Handle::health`. The snapshot sees what the supervisor knows —
//! before any `python_health` command exposes it.

use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, ExitKind, Handle, Limits, RuntimeEvent, RuntimeKind, SpawnSpec, State, SupervisorConfig,
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

/// Pongs every ping, but slower than the 50ms tick: each pong lands
/// just after the NEXT ping went out. The newest ping is in flight,
/// not missed — the miss count at pong arrival must be 0, not 1.
/// Pongs every ping but one seq BEHIND: ping seq s gets a pong for
/// s-1. At every pong arrival the newest queued ping is structurally
/// a fraction of an interval old — in flight, never missed — no
/// sleeps, so no runner jitter can flip the verdict (the earlier
/// sleep-based version flaked on macOS CI with 50ms margins).
const FAKE_SLOW_PONG: &str = r#"
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
        s = m["params"]["seq"]
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": s - 1 if s else 0, "loop_stale_ms": 0}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// loop_stale_ms flips by ping seq: healthy → 2500 (seqs 3-5) →
/// healthy (seq ≥6). Exactly two verdict flips: `{ok,busy}` then
/// `{ok,ok}` — the assumed-healthy baseline makes generation 1's
/// first `{ok,ok}` a non-event.
const FAKE_FLAP_LOOP: &str = r#"
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
        s = m["params"]["seq"]
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": s, "loop_stale_ms": 2500 if 3 <= s <= 5 else 0}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// Pongs seqs 1-2 healthy, lets seqs 3-4 go unanswered (missed climbs
/// to 2), then pongs every seq ≥5 with `loop_stale_ms: 2500`:
/// unresponsive-from-missed-pongs → unresponsive-from-stalled-loop —
/// the intra-state cause flip `onState` cannot see. The transport
/// verdict stays `missing` throughout (seqs 3-4 never resolve).
const FAKE_MISSED_THEN_STALE: &str = r#"
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
        s = m["params"]["seq"]
        if s <= 2:
            w({"jsonrpc":"2.0","method":"pong","params":{"seq": s, "loop_stale_ms": 0}})
        elif s >= 5:
            w({"jsonrpc":"2.0","method":"pong","params":{"seq": s, "loop_stale_ms": 2500}})
        # seqs 3,4 are read but never answered
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

/// The Web kind: hello, then `ready {url}` (Ready waits for it,
/// protocol-v1 4.1), then seq-echoing pongs with NO loop_stale_ms —
/// the wrapped server cannot measure the loop, matching the real
/// launcher (_launcher.py). Its steady verdict is {ok, unknown}.
const FAKE_WEB: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
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

/// The Web kind gone silent: hello + `ready {url}` (Ready still
/// arrives), then reads frames but never pongs — the transport
/// verdict flips {ok, unknown} → {missing, unknown}.
const FAKE_WEB_NO_PONG: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
w({"jsonrpc":"2.0","method":"ready","params":{"url":"http://127.0.0.1:4242/"}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    # every other frame is read and ignored — no pongs, ever
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

/// Collect every runtime event into a shared log so assertions can
/// count health payloads and state transitions together.
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

/// The `pylair://health` payloads seen so far, in arrival order.
fn health_events(log: &Arc<Mutex<Vec<RuntimeEvent>>>) -> Vec<serde_json::Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::HealthChanged(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

/// How many times `onState` carried a transition *into* Unresponsive.
fn unresponsive_arrivals(log: &Arc<Mutex<Vec<RuntimeEvent>>>) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(
                e,
                RuntimeEvent::StateChanged {
                    to: State::Unresponsive,
                    ..
                }
            )
        })
        .count()
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

/// A pong answering one seq behind still clears the transport: the
/// ping queued just before it lands is in flight — under one interval
/// old — not missed, so at the pong the transport verdict is `ok`
/// with `missed_pongs == 0` (the #120-review counting fix). Between
/// pongs the tick correctly reports one in-flight miss — the
/// assertion reads the pong-arrival instant, which `rtt_ms.is_some()`
/// guarantees.
#[tokio::test]
async fn a_pong_after_the_next_ping_is_not_a_miss() {
    let mut c = fast_config();
    c.missed_pongs_to_kill = 1000; // state may flap; never kill the worker
    let (events, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_SLOW_PONG), c, events);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| {
            let s = h.health(None);
            s.liveness.transport == "ok"
                && s.liveness.missed_pongs == 0
                && s.liveness.rtt_ms.is_some()
        })
        .await,
        "a pong landing after the next ping went out must not read as a miss"
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

/// §5: one emit per verdict flip, none on steady state. Generation 1's
/// baseline is assumed `{ok, ok}` (the runtime just reached Ready), so
/// a `0 → 2500 → 0` loop staleness flap emits exactly twice — busy,
/// then ok — and repeated identical pongs emit nothing.
#[tokio::test]
async fn a_stale_loop_flap_emits_busy_then_ok_and_stays_quiet() {
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_FLAP_LOOP), fast_config(), events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| health_events(&log).len() >= 2).await,
        "the busy and recovery flips never emitted"
    );
    let seen = health_events(&log);
    assert_eq!(seen[0]["liveness"]["transport"], json!("ok"));
    assert_eq!(seen[0]["liveness"]["loop"], json!("busy"));
    assert_eq!(seen[1]["liveness"]["transport"], json!("ok"));
    assert_eq!(seen[1]["liveness"]["loop"], json!("ok"));
    // §5 payload shape: versioned, generation-scoped, no window data.
    for e in &seen {
        assert_eq!(e["version"], json!(1));
        assert!(e["generation"].is_u64());
        assert!(e.get("self").is_none());
        assert!(e["liveness"].is_object());
    }
    // Repeated identical pongs emit nothing further: let five more
    // ping intervals elapse past the flap, then count again.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        health_events(&log).len(),
        2,
        "identical verdicts must not re-emit: {seen:?}"
    );
}

/// §5: a dead transport emits `missing` exactly once at the first miss
/// — identical subsequent misses re-emit nothing — and the
/// generation's end corrects the last verdict to `unknown`. The
/// restart's first verdict then emits again because the reset reseeded
/// the baseline.
#[tokio::test]
async fn a_dead_transport_emits_missing_once_then_unknown_at_teardown() {
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_NO_PONG), fast_config(), events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| health_events(&log).len() >= 2).await,
        "missing-then-teardown events never arrived"
    );
    let seen = health_events(&log);
    // Exactly one `missing` event before the kill, at the first miss;
    // the climbing count changes no verdict.
    assert_eq!(seen[0]["liveness"]["transport"], json!("missing"));
    assert_eq!(seen[0]["liveness"]["missed_pongs"], json!(1));
    // Teardown: the emitted verdict is corrected to unknown — a real
    // change from the last broadcast, not a steady-state echo.
    assert_eq!(seen[1]["liveness"]["transport"], json!("unknown"));
    assert_eq!(seen[1]["liveness"]["loop"], json!("unknown"));
    assert_eq!(seen[0]["generation"], seen[1]["generation"]);
    // The restarted generation's first verdict emits too — its
    // baseline was reseeded to the `unknown` observers last saw.
    assert!(
        eventually(|| health_events(&log)
            .iter()
            .any(|e| e["generation"] != seen[0]["generation"]))
        .await,
        "the restarted generation never emitted its first verdict"
    );
}

/// §5's motivating case: unresponsive-from-missed-pongs →
/// unresponsive-from-stalled-loop emits a health event while `onState`
/// cannot — `set_state` dedupes the same-state transition, so the
/// cause flip is invisible to state listeners. The transport stays
/// `missing` (seqs 3-4 never resolve); only the loop verdict flips.
/// Asserting a single `Unresponsive` arrival over two health events is
/// the onState byte-identity check.
#[tokio::test]
async fn the_unresponsive_cause_flip_emits_health_not_state() {
    let (events, rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_MISSED_THEN_STALE), fast_config(), events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| health_events(&log).len() >= 2).await,
        "the missed→stalled flip never emitted"
    );
    let seen = health_events(&log);
    assert_eq!(seen[0]["liveness"]["transport"], json!("missing"));
    assert_eq!(seen[0]["liveness"]["loop"], json!("ok"));
    assert_eq!(seen[1]["liveness"]["transport"], json!("missing"));
    assert_eq!(seen[1]["liveness"]["loop"], json!("busy"));
    assert_eq!(
        unresponsive_arrivals(&log),
        1,
        "the intra-state cause flip must not produce a second onState event"
    );
}

/// The per-kind baseline (#122 review): a Web runtime's steady verdict
/// is `{ok, unknown}` — its loop is never measured — so its first
/// pong is NOT news and must not manufacture a `{ok, ok} → {ok,
/// unknown}` event. The transport can still flip for real: a web
/// runtime that stops ponging emits `{missing, unknown}` — the loop
/// verdict stays `"unknown"` throughout, never a fabricated `"ok"`.
#[tokio::test]
async fn a_web_runtime_emits_nothing_steady_and_missing_on_silence() {
    let (events, rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.kind = RuntimeKind::Web;
    c.ready_timeout = Duration::from_secs(5);
    c.missed_pongs_to_kill = 1000; // probe liveness, not the kill
    let h = spawn_h(spec(FAKE_WEB), c, events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;
    // Several ping intervals of pongs: web's steady {ok, unknown} is
    // the assumed baseline, so nothing emits — the #122 nit.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        health_events(&log).is_empty(),
        "a wrapped app's first pongs must not emit: {:?}",
        health_events(&log)
    );
    h.shutdown().await;

    let (events, rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.kind = RuntimeKind::Web;
    c.ready_timeout = Duration::from_secs(5);
    c.missed_pongs_to_kill = 1000;
    let h = spawn_h(spec(FAKE_WEB_NO_PONG), c, events);
    let log = pump(rx);
    wait_state(&h, State::Ready).await;
    assert!(
        eventually(|| !health_events(&log).is_empty()).await,
        "the silent web transport never emitted"
    );
    let seen = health_events(&log);
    assert_eq!(seen[0]["liveness"]["transport"], json!("missing"));
    assert_eq!(seen[0]["liveness"]["loop"], json!("unknown"));
    assert_eq!(seen.len(), 1, "one flip, one event: {seen:?}");
    h.shutdown().await;
}
