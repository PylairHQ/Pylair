//! F5 (rc.15): configurable hello_timeout and the "still starting" signal.
//!
//!   * A runtime that takes longer than half the hello timeout produces
//!     exactly one repeated `starting → starting` state event carrying a
//!     human-readable detail, then still reaches Ready — the signal is a
//!     hint for UIs, not a state change.
//!   * A runtime that never hellos fails with a message that names both
//!     escape hatches (env knob and Builder method).
//!   * A fast hello emits no same-state event at all — normal boots are
//!     silent.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(30);

fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: tauri_plugin_pylair::supervisor::EventBus,
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

fn cfg(hello_timeout: Duration) -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = hello_timeout;
    c.ping_interval = Duration::from_secs(60); // no liveness interference
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 50;
    c.shutdown_grace = Duration::from_secs(2);
    c.loop_stall_kill = None;
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
    .unwrap_or_else(|_| panic!("never reached {want:?} (state {:?})", *rx.borrow()));
}

/// Echo-capable runtime that sleeps `delay_s` seconds before hello.
fn delayed_hello(delay_s: f64) -> String {
    format!(
        r#"
import sys, struct, json, time
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
time.sleep({delay_s})
w({{"jsonrpc":"2.0","method":"hello","params":{{"protocol":1,"exposes":[{{"name":"echo","kind":"sync"}}]}}}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m.get("method") == "ping":
        w({{"jsonrpc":"2.0","method":"pong","params":{{"seq": m["params"]["seq"]}}}})
    elif m.get("method") == "shutdown":
        w({{"jsonrpc":"2.0","id": m["id"],"result": None}}); break
    elif "id" in m:
        w({{"jsonrpc":"2.0","id": m["id"],"result": m["params"].get("input")}})
"#
    )
}

/// StateChanged events collected CONTINUOUSLY, the way the production
/// pump drains: state slots coalesce intermediate transitions by design
/// (protocol §5.1), so a drain-at-end probe only ever sees the newest
/// one. The signal being probed here fires a full second before Ready,
/// so a live drain always observes it — the probe must too.
type StateLog = Arc<Mutex<Vec<(State, State, String)>>>;

fn collect_state(mut rx: tauri_plugin_pylair::supervisor::EventDrain) -> StateLog {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            if let RuntimeEvent::StateChanged { from, to, detail } = e {
                sink.lock().unwrap().push((from, to, detail));
            }
        }
    });
    log
}

/// Every StateChanged event where from == to == "starting" (the
/// still-starting signal) collected so far.
fn slow_start_events(log: &StateLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(f, t, _)| matches!(f, State::Starting) && matches!(t, State::Starting))
        .map(|(_, _, d)| d.clone())
        .collect()
}

/// Hello delayed past half the timeout: one slow-start event, then Ready.
#[tokio::test]
async fn delayed_hello_emits_one_slow_start_event_then_ready() {
    let (ev, rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let log = collect_state(rx);
    // hello at ~3s, half-mark at 2s — the signal must fire exactly once.
    let h = spawn_h(spec(&delayed_hello(3.0)), cfg(Duration::from_secs(4)), ev);
    wait_state(&h, State::Ready).await;
    // The signal precedes hello, so by Ready it is already queued.
    let events = slow_start_events(&log);
    assert_eq!(
        events.len(),
        1,
        "expected exactly one slow-start event, got {events:?}"
    );
    assert!(
        events[0].contains("still starting") && events[0].contains("PYLAIR_HELLO_TIMEOUT_MS"),
        "slow-start detail should name the delay and the knob, got {:?}",
        events[0]
    );
    h.shutdown().await;
}

/// A hello that beats the half-mark: no same-state event at all.
#[tokio::test]
async fn fast_hello_emits_no_slow_start_event() {
    let (ev, rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let log = collect_state(rx);
    let h = spawn_h(spec(&delayed_hello(0.05)), cfg(Duration::from_secs(4)), ev);
    wait_state(&h, State::Ready).await;
    // Give a stray timer a chance to fire (it must not exist).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        slow_start_events(&log).is_empty(),
        "fast hello must not emit a slow-start event"
    );
    h.shutdown().await;
}

/// No hello at all: the timeout message must name both escape hatches.
#[tokio::test]
async fn hello_timeout_message_names_env_and_builder_knobs() {
    const NEVER_HELLO: &str = "import time; time.sleep(999)";
    let (ev, mut rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let h = spawn_h(spec(NEVER_HELLO), cfg(Duration::from_secs(2)), ev);
    let found = timeout(Duration::from_secs(15), async {
        loop {
            match rx.recv().await {
                Some(RuntimeEvent::StateChanged { detail, .. })
                    if detail.contains("PYLAIR_HELLO_TIMEOUT_MS")
                        && detail.contains("Builder::hello_timeout") =>
                {
                    break detail;
                }
                Some(_) => continue,
                None => panic!("events channel closed before timeout fired"),
            }
        }
    })
    .await
    .expect("no event named the hello-timeout knobs within 15s");
    assert!(
        found.contains("no hello"),
        "timeout detail should still explain what happened: {found}"
    );
    h.shutdown().await;
}
