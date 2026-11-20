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
