//! K2: the Windows spawn-to-job-assignment gap.
//!
//! The runtime must be inside the Job Object before it executes a single
//! instruction, otherwise a process it spawns early is born outside the job
//! and survives the job closing. The gap is microseconds wide in practice,
//! so this test widens it on purpose with the debug-only
//! `PYLAIR_TEST_ASSIGN_DELAY_MS` seam in `spawn_child`: the fake runtime
//! spawns a long-lived grandchild as its very first act, then the host
//! tears the runtime down and asserts the grandchild is gone.
//!
//! Design and trade-offs: docs/design/windows-job-object-race.md.
#![cfg(windows)]

use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri_plugin_pylair::supervisor::{
    spawn, RuntimeEvent, ShutdownOutcome, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// First act: spawn a grandchild that outlives any polite exit, report its
/// pid on stderr, only then say hello and serve like a normal runtime.
const FAKE_SPAWNS_FIRST: &str = r#"
import sys, struct, json, subprocess
g = subprocess.Popen(
    [sys.executable, "-c", "import time; time.sleep(600)"],
    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
sys.stderr.write("GRANDCHILD %d\n" % g.pid); sys.stderr.flush()
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}})
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

/// True while `pid` names a process that has not exited. A process that is
/// already gone cannot be opened (null handle); one mid-termination or
/// dead-but-referenced is reported by its signaled state, not by presence.
fn is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const SYNCHRONIZE: u32 = 0x0010_0000;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return false;
        }
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        r == WAIT_TIMEOUT
    }
}

#[tokio::test]
async fn grandchild_spawned_during_the_assign_gap_dies_with_the_job() {
    // 400 ms is enormous next to the real 33 us gap: it guarantees the
    // runtime has run (and spawned) before assignment unless the child is
    // held back until it is in the job.
    std::env::set_var("PYLAIR_TEST_ASSIGN_DELAY_MS", "400");

    let (ev_tx, mut ev_rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut cfg = SupervisorConfig::default();
    cfg.hello_timeout = Duration::from_secs(10);
    cfg.shutdown_grace = Duration::from_secs(3);
    cfg.restart_trigger = None;
    let spec = SpawnSpec {
        program: python(),
        args: vec!["-c".into(), FAKE_SPAWNS_FIRST.into()],
        ..Default::default()
    };
    let (h, actor) = spawn(spec, cfg, ev_tx);
    tokio::spawn(actor);

    // The pid arrives on the stderr event stream.
    let grandchild: u32 = timeout(Duration::from_secs(15), async {
        loop {
            if let RuntimeEvent::Stderr(l) = ev_rx.recv().await.expect("event channel closed") {
                if let Some(p) = l.trim().strip_prefix("GRANDCHILD ") {
                    break p.parse().expect("pid");
                }
            }
        }
    })
    .await
    .expect("fake runtime never reported its grandchild");
    assert!(is_running(grandchild), "grandchild should be alive here");

    // The runtime must still come up: resuming after assignment is part of
    // the contract, not just containment.
    let mut st = h.subscribe_state();
    timeout(Duration::from_secs(15), async {
        while *st.borrow() != State::Ready {
            st.changed().await.expect("state channel closed");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("runtime never became ready (at {:?})", h.state()));

    // Tear down: the monitor task ends, the last job handle closes, the
    // kernel kills everything that was in the job.
    let outcome = h.shutdown().await;
    assert!(
        matches!(
            outcome,
            ShutdownOutcome::Terminated | ShutdownOutcome::ForcedKill
        ),
        "unexpected shutdown outcome {outcome:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while is_running(grandchild) {
        assert!(
            Instant::now() < deadline,
            "grandchild {grandchild} survived the job closing: it was spawned \
             before the runtime was assigned to the Job Object"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
