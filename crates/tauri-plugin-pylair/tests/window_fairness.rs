//! Spike proof for docs/decisions/0005-per-window-fairness.md: limits are
//! per RUNTIME, so one owner can exhaust a shared cap and starve another.
//! These tests assert TODAY'S unfair outcome deterministically - the
//! implementation PR flips each assertion to the fair outcome, which is
//! what makes the change verifiable.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, Limits, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
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

/// Answers hello, pongs and shutdown but NEVER answers calls: every
/// admitted call holds its task slot forever, so the task cap is the
/// resource under test with no timing dependence.
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
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    # calls are read so the pipe never backpressures, but never answered
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
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.shutdown_grace = Duration::from_secs(2);
    c.cancel_timeout = Duration::from_secs(5);
    c.limits = Limits::default().with_max_concurrent_tasks(4);
    c
}

async fn wait_state(h: &Handle, want: State) {
    let mut rx = h.subscribe_state();
    timeout(T, async {
        while rx.changed().await.is_ok() {
            if *rx.borrow() == want {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never became {want:?}"));
}

/// owner "a" holds every task slot; owner "b" is refused with the shared
/// cap. Proves the gap named in security.md section 6. After the fairness
/// work lands this flips: b is admitted on its reserved share and a's
/// next call fails LIMIT_EXCEEDED{limit:"window_share"}.
#[tokio::test]
async fn one_owner_starves_the_other_of_task_slots() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HOLDS_TASKS), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    // a fills all 4 task slots (each call registers a task, never finishes)
    let mut held = Vec::new();
    for _ in 0..4 {
        let ct = h
            .call_task("hold", json!({}), Some("win-a".into()), Some(false))
            .expect("a's call should be admitted under the global cap");
        held.push(ct);
    }

    // TODAY: b is starved out of the runtime entirely.
    let starved = h
        .call("hold", json!({}), Some("win-b".into()), Some(false))
        .await
        .expect_err("b loses the shared cap to a - the documented gap");
    assert_eq!(starved.code, proto::LIMIT_EXCEEDED);
    assert_eq!(
        starved.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_tasks")
    );

    // And a's extra calls fail identically - the cap cannot tell owners
    // apart, so a hog and its victim see the same error.
    let also = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("a is over the shared cap too - same error for both");
    assert_eq!(also.code, proto::LIMIT_EXCEEDED);

    h.shutdown().await;
}

/// Same as FAKE_HOLDS_TASKS, but the child exits after consuming a
/// third call frame. The test controls the crash timing entirely by
/// when it sends that call: the first two register and hold their
/// task slots for as long as the test needs, with no sleep anywhere
/// in the assertion path.
const FAKE_DIES_AFTER_THREE: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hold","kind":"sync"}]}})
calls = 0
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    else:
        calls += 1
        if calls >= 3: break  # die holding the tasks
"#;

/// hello, then immediate exit: every generation dies before it can
/// serve anything, so the crash-loop breaker parks the supervisor.
const FAKE_DIES_AFTER_HELLO: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
"#;

/// Counters settle on the actor's own schedule; poll instead of
/// sleeping a fixed time.
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

/// PR-1 counting: in_queue is charged at try_send and released at the
/// recv() site, so after a burst it is 0 and the debt sits in
/// in_flight.
#[tokio::test]
async fn in_queue_returns_to_zero_after_a_burst() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_HOLDS_TASKS), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("admitted under the global cap"),
        );
    }
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")) == (0, 4)).await,
        "queue debt must drain into in_flight: {:?}",
        h.fairness_snapshot()
    );
    h.shutdown().await;
}

/// A call dequeued while the supervisor sleeps between respawns is
/// fail-fasted AND frees its queue slot (release happens at recv(),
/// not at dispatch).
#[tokio::test]
async fn in_queue_returns_to_zero_after_restart_fail_fast() {
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    // A long backoff holds the Restarting state open deterministically;
    // shutdown() below cuts the sleep short.
    c.backoff_base = Duration::from_secs(30);
    c.backoff_cap = Duration::from_secs(30);
    let h = spawn_h(spec(FAKE_DIES_AFTER_THREE), c, ev);
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("admitted"),
        );
    }
    // The child eats the third call and exits -> crash -> Restarting.
    wait_state(&h, State::Restarting).await;

    let err = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("calls fail fast while the runtime restarts");
    assert_eq!(err.code, proto::RUNTIME_UNAVAILABLE);
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")).0 == 0).await,
        "the fail-fasted call must release its queue slot: {:?}",
        h.fairness_snapshot()
    );
    h.shutdown().await;
}

/// Same release rule inside the failed-park loop: once the breaker
/// parks the supervisor, calls fail fast and owe nothing.
#[tokio::test]
async fn in_queue_returns_to_zero_after_failed_park() {
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.breaker_crashes = 2;
    let h = spawn_h(spec(FAKE_DIES_AFTER_HELLO), c, ev);
    wait_state(&h, State::Failed).await;

    let err = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("a parked runtime fails calls fast");
    assert_eq!(err.code, proto::RUNTIME_UNAVAILABLE);
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")).0 == 0).await,
        "the fail-fasted call must release its queue slot: {:?}",
        h.fairness_snapshot()
    );
    h.shutdown().await;
}

/// in_flight is per-generation: a crash kills the Session and its task
/// map, so the next Ready reports the owner idle again.
#[tokio::test]
async fn in_flight_resets_across_a_crash() {
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.backoff_base = Duration::from_millis(50);
    let h = spawn_h(spec(FAKE_DIES_AFTER_THREE), c, ev);
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("admitted"),
        );
    }
    // The fixture holds both tasks for as long as we let it: in_flight
    // stays at 2 until the third call frame arrives and kills the
    // child. No sleep anywhere in the path.
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")).1 == 2).await,
        "both calls must register before the crash: {:?}",
        h.fairness_snapshot()
    );
    held.push(
        h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
            .expect("the crash trigger is just another admitted call"),
    );

    // Crash -> Session::drop ends the generation at TEARDOWN, inside
    // the restart backoff and before any respawn: the dead
    // generation's tasks never inflate in_use. The fixture holds every
    // task it is given, so in_flight can only reach 0 via the
    // generation reset - the poll IS the assertion.
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")) == (0, 0)).await,
        "in_flight must zero at session teardown, not the next spawn: {:?}",
        h.fairness_snapshot()
    );
    h.shutdown().await;
}
