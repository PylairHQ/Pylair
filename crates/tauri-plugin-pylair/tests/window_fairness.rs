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

/// A spec whose script gets an extra argv[1] - the gate file path for
/// FAKE_HELLO_GATE.
fn spec_argv(script: &str, arg: &std::path::Path) -> SpawnSpec {
    let mut s = spec(script);
    s.args.push(arg.to_string_lossy().into_owned());
    s
}

/// A path that does not exist yet; the test creates it to signal the
/// fixture.
fn temp_gate() -> PathBuf {
    std::env::temp_dir().join(format!(
        "pylair-fair-gate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
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
        // A watch receiver only sees values published after subscribe;
        // check the current state first or a fast transition is missed.
        if *rx.borrow() == want {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() == want {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never became {want:?}"));
}

/// The PR 3 flip of the original starvation spike: with two live
/// windows and `max_concurrent_tasks = 4`, win-a's task ceiling is 3
/// (win-b's idle reservation is one slot), so a's fourth call is
/// refused `window_share` and win-b's call is admitted and answered.
#[tokio::test]
async fn one_owner_can_no_longer_starve_the_other_of_task_slots() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), fast_config(), ev);
    h.fairness_join_window("win-a");
    h.fairness_join_window("win-b");
    wait_state(&h, State::Ready).await;

    // a holds up to its share: 3 in-flight tasks (each call registers
    // a task and never finishes).
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("a's call is inside its share"),
        );
    }
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")) == (0, 3)).await,
        "a's three tasks must be in flight: {:?}",
        h.fairness_snapshot()
    );

    // a's fourth call is over share: refused before a task exists.
    let refused = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("a is over its task-share ceiling");
    assert_eq!(refused.code, proto::LIMIT_EXCEEDED);
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("window_share")
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["max"].as_u64()),
        Some(3)
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["window"].as_str()),
        Some("win-a")
    );

    // b is the victim fairness protects: its call is admitted AND
    // answered, while a still holds its three slots.
    let answered = h
        .call("ok", json!({}), Some("win-b".into()), Some(false))
        .await
        .expect("b is under share and must be served");
    assert_eq!(answered, json!({"ok": "ok"}));

    h.shutdown().await;
    // Held CallTasks resolve with errors as the runtime shuts down.
    for ct in held {
        let _ = ct.result.await;
    }
}

/// The pre-fairness outcome, preserved verbatim under the off switch:
/// a fills all 4 task slots, b is starved out entirely, and hog and
/// victim see the same `max_concurrent_tasks` error.
#[tokio::test]
async fn one_owner_starves_the_other_when_fairness_is_off() {
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.limits = c.limits.with_per_window_fairness(false);
    let h = spawn_h(spec(FAKE_HOLDS_TASKS), c, ev);
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("a's call should be admitted under the global cap"),
        );
    }

    let starved = h
        .call("hold", json!({}), Some("win-b".into()), Some(false))
        .await
        .expect_err("b loses the shared cap to a - today's outcome");
    assert_eq!(starved.code, proto::LIMIT_EXCEEDED);
    assert_eq!(
        starved.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_tasks")
    );

    let also = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("a is over the shared cap too - same error for both");
    assert_eq!(also.code, proto::LIMIT_EXCEEDED);

    h.shutdown().await;
    for ct in held {
        let _ = ct.result.await;
    }
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

/// Withholds `hello` until a gate file exists, then answers every
/// call. While the gate is closed the actor serves no commands, so
/// calls accumulate in the supervisor queue exactly as long as the
/// test wants - queue admission is provable without a single sleep
/// in the assertion path.
const FAKE_HELLO_GATE: &str = r#"
import sys, struct, json, os, time
gate = sys.argv[1]
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
while not os.path.exists(gate):
    time.sleep(0.02)
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
    else:
        w({"jsonrpc":"2.0","id": m["id"],"result": {"ok": True}})
"#;

/// The fairness workhorse: `hold` calls register their task id and are
/// never answered (a held slot for as long as the test wants), `die`
/// calls exit the process with every task held, `cancel` gets an ack
/// plus a TASK_CANCELLED terminal for the held call it names, and
/// every other call answers `{"ok": <fn>}`. All timing lives in what
/// the test sends - there is no sleep in the fixture.
const FAKE_FAIRNESS_RUNTIME: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"hold","kind":"sync"},{"name":"ok","kind":"sync"},{"name":"die","kind":"sync"}]}})
held = {}
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    meth = m.get("method")
    if meth == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif meth == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif meth == "cancel":
        task = m["params"]["task"]
        w({"jsonrpc":"2.0","id": m["id"],"result": {"cancelled": True}})
        if task in held:
            w({"jsonrpc":"2.0","id": held.pop(task),"error": {"code": -32010,"message": "cancelled"}})
    elif meth == "call":
        fn = m["params"]["fn"]
        if fn == "die":
            break  # crash holding every task
        if fn == "hold":
            held[m["params"]["task"]] = m["id"]
        else:
            w({"jsonrpc":"2.0","id": m["id"],"result": {"ok": fn}})
    # stream.credit and friends are notifications: nothing to answer
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

/// The queue-share check (PR 2): with win-a and win-b live and
/// max_queued_commands = 8, win-b's idle reservation is one slot, so
/// win-a's ceiling is 7 - its eighth call is refused `window_share`,
/// while win-b's call still fits. Once hello lands every admitted
/// call answers and in_queue drains to nothing.
#[tokio::test]
async fn queue_flood_is_refused_at_window_share_not_the_global_cap() {
    let gate = temp_gate();
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.hello_timeout = Duration::from_secs(30);
    c.limits = Limits::default().with_max_queued_commands(8);
    let h = spawn_h(spec_argv(FAKE_HELLO_GATE, &gate), c, ev);
    h.fairness_join_window("win-a");
    h.fairness_join_window("win-b");

    let mut held = Vec::new();
    for _ in 0..7 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("within a's share"),
        );
    }
    let refused = h
        .call_task("hold", json!({}), Some("win-a".into()), Some(false))
        .err()
        .expect("a is over its reserved-share ceiling");
    assert_eq!(refused.code, proto::LIMIT_EXCEEDED);
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("window_share")
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["max"].as_u64()),
        Some(7)
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["window"].as_str()),
        Some("win-a")
    );

    // b has queued nothing: its call is admitted to the queue even
    // though a is over share.
    held.push(
        h.call_task("hold", json!({}), Some("win-b".into()), Some(false))
            .expect("b's share is untouched"),
    );

    // Open the gate: hello -> Ready -> every queued call is answered.
    std::fs::write(&gate, b"1").unwrap();
    wait_state(&h, State::Ready).await;
    for ct in held {
        let v = ct
            .result
            .await
            .expect("call delivered")
            .expect("call answered");
        assert_eq!(v, json!({"ok": true}));
    }
    // Live windows keep their owner entries; the counters drain.
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")) == (0, 0)
            && h.fairness_snapshot().usage(Some("win-b")) == (0, 0))
        .await,
        "in_queue and in_flight must drain to nothing: {:?}",
        h.fairness_snapshot()
    );
    let _ = std::fs::remove_file(&gate);
    h.shutdown().await;
}

/// Single-window parity: the only live window fills the queue to the
/// GLOBAL cap and the refusal is `max_queued_commands`, never
/// `window_share` - identical to today.
#[tokio::test]
async fn single_window_fills_the_global_cap_and_sees_max_queued_commands() {
    let gate = temp_gate();
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.hello_timeout = Duration::from_secs(30);
    c.limits = Limits::default().with_max_queued_commands(8);
    let h = spawn_h(spec_argv(FAKE_HELLO_GATE, &gate), c, ev);
    h.fairness_join_window("win-a");

    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("a lone window keeps the whole queue"),
        );
    }
    let refused = h
        .call_task("hold", json!({}), Some("win-a".into()), Some(false))
        .err()
        .expect("the global cap still binds");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_queued_commands"),
        "a lone window must see today's error, not window_share"
    );

    std::fs::write(&gate, b"1").unwrap();
    wait_state(&h, State::Ready).await;
    for ct in held {
        ct.result.await.unwrap().unwrap();
    }
    let _ = std::fs::remove_file(&gate);
    h.shutdown().await;
}

/// The host-side bucket: while `None` holds queued work it is live and
/// shares the queue by the same rule - and `window` is null in the
/// error. Once it drains it stops being live entirely.
#[tokio::test]
async fn host_bucket_shares_while_busy_and_stops_reserving_when_idle() {
    let gate = temp_gate();
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.hello_timeout = Duration::from_secs(30);
    c.limits = Limits::default().with_max_queued_commands(8);
    let h = spawn_h(spec_argv(FAKE_HELLO_GATE, &gate), c, ev);
    h.fairness_join_window("win-a");

    let mut held = Vec::new();
    for _ in 0..7 {
        held.push(
            h.call_task("hold", json!({}), None, Some(false))
                .expect("the host bucket shares the queue like a window"),
        );
    }
    let refused = h
        .call_task("hold", json!({}), None, Some(false))
        .err()
        .expect("the host bucket hits its ceiling too");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("window_share")
    );
    assert!(
        refused.data.as_ref().is_some_and(|d| d["window"].is_null()),
        "the host bucket is reported as window: null"
    );

    std::fs::write(&gate, b"1").unwrap();
    wait_state(&h, State::Ready).await;
    for ct in held {
        ct.result.await.unwrap().unwrap();
    }
    assert!(
        eventually(|| !h.fairness_snapshot().is_live(None)).await,
        "an idle host bucket must stop reserving: {:?}",
        h.fairness_snapshot()
    );
    let _ = std::fs::remove_file(&gate);
    h.shutdown().await;
}

/// `per_window_fairness = false` skips the share check entirely: the
/// same flood produces today's exact outcome - whoever queues last
/// sees `max_queued_commands`.
#[tokio::test]
async fn fairness_off_is_exactly_todays_outcome() {
    let gate = temp_gate();
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.hello_timeout = Duration::from_secs(30);
    c.limits = Limits::default()
        .with_max_queued_commands(8)
        .with_per_window_fairness(false);
    let h = spawn_h(spec_argv(FAKE_HELLO_GATE, &gate), c, ev);
    h.fairness_join_window("win-a");
    h.fairness_join_window("win-b");

    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("no share check: the global cap is the only bound"),
        );
    }
    for owner in [Some("win-a".to_string()), Some("win-b".to_string()), None] {
        let refused = h
            .call_task("hold", json!({}), owner, Some(false))
            .err()
            .expect("the global cap binds everyone identically");
        assert_eq!(
            refused.data.as_ref().and_then(|d| d["limit"].as_str()),
            Some("max_queued_commands")
        );
    }

    std::fs::write(&gate, b"1").unwrap();
    wait_state(&h, State::Ready).await;
    for ct in held {
        ct.result.await.unwrap().unwrap();
    }
    assert!(
        eventually(|| h.fairness_snapshot().owners.is_empty()).await,
        "disabled fairness counts nothing at all: {:?}",
        h.fairness_snapshot()
    );
    let _ = std::fs::remove_file(&gate);
    h.shutdown().await;
}

/// Single-window parity for task slots: the only live window fills
/// all `max_concurrent_tasks` and the refusal is the global error,
/// never `window_share`.
#[tokio::test]
async fn a_lone_window_fills_all_task_slots_and_sees_the_global_cap() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), fast_config(), ev);
    h.fairness_join_window("win-a");
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("a lone window keeps every task slot"),
        );
    }
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")) == (0, 4)).await,
        "all four tasks in flight: {:?}",
        h.fairness_snapshot()
    );

    let refused = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("the global cap still binds");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_tasks"),
        "a lone window must see today's error, not window_share"
    );

    h.shutdown().await;
    for ct in held {
        let _ = ct.result.await;
    }
}

/// Single-window parity for stream slots: the only live window opens
/// all `max_concurrent_streams` and the refusal is the global error.
#[tokio::test]
async fn a_lone_window_fills_all_stream_slots_and_sees_the_global_cap() {
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.limits = c.limits.with_max_concurrent_streams(2);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), c, ev);
    h.fairness_join_window("win-a");
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(
            h.call_stream("hold", json!({}), Some("win-a".into()), Some(true))
                .await
                .expect("a lone window keeps every stream slot"),
        );
    }
    assert!(
        eventually(|| h.fairness_snapshot().stream_usage(Some("win-a")) == 2).await,
        "both streams in flight: {:?}",
        h.fairness_snapshot()
    );

    let refused = h
        .call_stream("hold", json!({}), Some("win-a".into()), Some(true))
        .await
        .err()
        .expect("the global stream cap still binds");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_streams"),
        "a lone window must see today's error, not window_share"
    );

    h.shutdown().await;
    drop(held);
}

/// Stream slots are shared too: with `max_concurrent_streams = 2` and
/// two live windows, win-a's stream ceiling is 1 - its second stream is
/// refused `window_share` while win-b's stream is admitted.
#[tokio::test]
async fn stream_slots_are_shared_across_windows() {
    let (ev, _rx) = mpsc::channel(64);
    let mut c = fast_config();
    c.limits = c.limits.with_max_concurrent_streams(2);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), c, ev);
    h.fairness_join_window("win-a");
    h.fairness_join_window("win-b");
    wait_state(&h, State::Ready).await;

    let (a_task, a_rx) = h
        .call_stream("hold", json!({}), Some("win-a".into()), Some(true))
        .await
        .expect("a's first stream is inside its share");
    assert!(
        eventually(|| h.fairness_snapshot().stream_usage(Some("win-a")) == 1).await,
        "a's stream in flight: {:?}",
        h.fairness_snapshot()
    );

    let refused = h
        .call_stream("hold", json!({}), Some("win-a".into()), Some(true))
        .await
        .err()
        .expect("a is over its stream-share ceiling");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("window_share")
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["max"].as_u64()),
        Some(1)
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["window"].as_str()),
        Some("win-a")
    );

    // b's stream still fits: a is at its minimum so it reserves nothing.
    let (_b_task, _b_rx) = h
        .call_stream("hold", json!({}), Some("win-b".into()), Some(true))
        .await
        .expect("b's share is untouched");
    assert!(
        eventually(|| h.fairness_snapshot().stream_usage(Some("win-b")) == 1).await,
        "b's stream in flight: {:?}",
        h.fairness_snapshot()
    );

    h.shutdown().await;
    let _ = (a_task, a_rx);
}

/// Cancelled tasks free their owner's slots: win-b, previously refused
/// at the full global cap, fills what `cancel_owner` released - up to
/// its own share ceiling.
#[tokio::test]
async fn cancelled_slots_are_claimable_by_the_other_window() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), fast_config(), ev);
    h.fairness_join_window("win-a");
    h.fairness_join_window("win-b");
    wait_state(&h, State::Ready).await;

    // a to its ceiling (3), b's one call makes the global cap full.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("a's share"),
        );
    }
    held.push(
        h.call_task("hold", json!({}), Some("win-b".into()), Some(false))
            .expect("b's share"),
    );
    assert!(
        eventually(|| {
            let s = h.fairness_snapshot();
            s.usage(Some("win-a")) == (0, 3) && s.usage(Some("win-b")) == (0, 1)
        })
        .await,
        "3+1 tasks in flight fills the global cap: {:?}",
        h.fairness_snapshot()
    );
    let refused = h
        .call("hold", json!({}), Some("win-b".into()), Some(false))
        .await
        .expect_err("the full global cap still binds b");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_tasks")
    );

    // Free a's slots: b can now claim them, up to its own ceiling of 3
    // (a stays live but idle, still reserving one slot).
    h.cancel_owner("win-a".into()).await;
    assert!(
        eventually(|| h.fairness_snapshot().usage(Some("win-a")) == (0, 0)).await,
        "a's tasks must drain through the cancel path: {:?}",
        h.fairness_snapshot()
    );
    for _ in 0..2 {
        held.push(
            h.call_task("hold", json!({}), Some("win-b".into()), Some(false))
                .expect("b fills the slots a released"),
        );
    }
    let refused = h
        .call("hold", json!({}), Some("win-b".into()), Some(false))
        .await
        .expect_err("b stops at ITS share ceiling, not the global cap");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("window_share")
    );
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["max"].as_u64()),
        Some(3)
    );

    h.shutdown().await;
    for ct in held {
        let _ = ct.result.await;
    }
}

/// A runtime crash zeroes per-owner tasks AND streams at session
/// teardown - inside the restart backoff - and the lone live window
/// then reaches the full global cap in the next generation.
#[tokio::test]
async fn a_crash_zeroes_tasks_and_streams_and_the_cap_reopens() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), fast_config(), ev);
    h.fairness_join_window("win-a");
    wait_state(&h, State::Ready).await;

    // Two plain tasks plus one stream: in_flight 3, streams 1.
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("admitted"),
        );
    }
    let (_st, _srx) = h
        .call_stream("hold", json!({}), Some("win-a".into()), Some(true))
        .await
        .expect("the held stream is admitted too");
    assert!(
        eventually(|| {
            let s = h.fairness_snapshot();
            s.usage(Some("win-a")) == (0, 3) && s.stream_usage(Some("win-a")) == 1
        })
        .await,
        "2 tasks + 1 stream in flight: {:?}",
        h.fairness_snapshot()
    );

    // The die call is the crash trigger, fully under the test's
    // control: the generation dies holding all four tasks.
    let die = h
        .call_task("die", json!({}), Some("win-a".into()), Some(false))
        .expect("the trigger call is admitted");
    assert!(
        eventually(|| {
            let s = h.fairness_snapshot();
            s.usage(Some("win-a")) == (0, 0) && s.stream_usage(Some("win-a")) == 0
        })
        .await,
        "teardown must zero tasks AND streams, not the next spawn: {:?}",
        h.fairness_snapshot()
    );
    let _ = die.result.await;

    // Next generation, a is still the only live window: it fills all
    // four slots again and the refusal is the global cap.
    wait_state(&h, State::Ready).await;
    for _ in 0..4 {
        held.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("a lone window re-fills the cap"),
        );
    }
    let refused = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("the global cap is the only refusal left");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_tasks")
    );

    h.shutdown().await;
    for ct in held {
        let _ = ct.result.await;
    }
}

/// The host bucket gets the same rule for task slots: while `None`
/// holds work its calls share the cap (window: null in the error);
/// once it drains it stops being live and the real window reaches the
/// full cap.
#[tokio::test]
async fn host_bucket_shares_task_slots_only_while_busy() {
    let (ev, _rx) = mpsc::channel(64);
    let h = spawn_h(spec(FAKE_FAIRNESS_RUNTIME), fast_config(), ev);
    h.fairness_join_window("win-a");
    wait_state(&h, State::Ready).await;

    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(
            h.call_task("hold", json!({}), None, Some(false))
                .expect("the host bucket shares task slots like a window"),
        );
    }
    assert!(
        eventually(|| h.fairness_snapshot().usage(None) == (0, 3)).await,
        "host tasks in flight: {:?}",
        h.fairness_snapshot()
    );
    let refused = h
        .call("hold", json!({}), None, Some(false))
        .await
        .expect_err("the host bucket hits its ceiling too");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("window_share")
    );
    assert!(
        refused.data.as_ref().is_some_and(|d| d["window"].is_null()),
        "the host bucket is reported as window: null"
    );

    // While None is busy it is live - and the window still fits,
    // because the busy bucket is past its minimum and reserves
    // nothing further.
    let answered = h
        .call("ok", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect("the window keeps its own share");
    assert_eq!(answered, json!({"ok": "ok"}));

    // Cancel the host tasks (owner_allows: host tasks are cancellable
    // by anyone). Collect task ids BEFORE cancelling - awaiting a held
    // call's result now would block until the cancel resolves it.
    let mut task_ids = Vec::new();
    let mut results = Vec::new();
    for ct in held {
        task_ids.push(ct.task.await.expect("task id was delivered"));
        results.push(ct.result);
    }
    for t in task_ids {
        let _ = h.cancel(t, None).await;
    }
    for r in results {
        let _ = r.await;
    }
    assert!(
        eventually(|| !h.fairness_snapshot().is_live(None)).await,
        "an idle host bucket must stop reserving: {:?}",
        h.fairness_snapshot()
    );

    // Now lone-live win-a reaches the full cap.
    let mut held2 = Vec::new();
    for _ in 0..4 {
        held2.push(
            h.call_task("hold", json!({}), Some("win-a".into()), Some(false))
                .expect("with an idle host bucket, the window fills the cap"),
        );
    }
    let refused = h
        .call("hold", json!({}), Some("win-a".into()), Some(false))
        .await
        .expect_err("the global cap is the only refusal left");
    assert_eq!(
        refused.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some("max_concurrent_tasks")
    );

    h.shutdown().await;
    for ct in held2 {
        let _ = ct.result.await;
    }
}
