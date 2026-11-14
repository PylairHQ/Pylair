//! RC.6 acceptance tests — originally the Step 0 reproduction suite for
//! three claims, now asserting the fixes hold. Do not weaken assertions.
//!
//!   1. non_owner_* — a webview that does not own a task cannot cancel it
//!      or grant credit on its id. Ownership comes from the caller identity
//!      the command layer supplies (the Tauri webview label), never from
//!      a task-id argument.
//!   2. cancel_not_dropped_when_frame_queue_full — control frames travel a
//!      dedicated lane that a data flood cannot starve, and every outcome
//!      is distinct: NotFound is not "cancel failed", a wedged runtime is
//!      a loud error, delivery is never silently dropped.
//!   3. host_sigkill_* — an abrupt host kill leaves no runtime and no
//!      isolated worker running. Linux: PR_SET_PDEATHSIG. All unix: stdin
//!      EOF drain + the worker's own parent-death watcher.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(15);

// ---- shared harness (same conventions as supervisor_faults.rs) ----

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

fn fast_config() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(2);
    c.ping_interval = Duration::from_millis(150);
    c.missed_pongs_to_kill = 2;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 2;
    c.breaker_window = Duration::from_secs(60);
    c.shutdown_grace = Duration::from_secs(2);
    c.initial_stream_credit = 8;
    c.stream_buffer = 256;
    c.loop_stall_kill = None;
    c.cancel_timeout = Duration::from_secs(5);
    c.max_frame = proto::max_frame();
    c.restart_trigger = None;
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
    .unwrap_or_else(|_| panic!("state never reached {want:?} (at {:?})", h.state()));
}

/// Well-behaved fake runtime: hello, echo, pong, exit on shutdown/EOF.
const FAKE_READY: &str = r#"
import sys, struct, json
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
    elif "id" in m:
        w({"jsonrpc":"2.0","id": m["id"],"result": m["params"].get("input")})
"#;

/// Hello, one pong, then wedges forever without ever reading stdin —
/// stands in for a runtime whose loop (or reader) is dead: nothing can
/// observe stdin EOF, so no drain ever runs.
const FAKE_WEDGED: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time
time.sleep(600)
"#;

// ---- claim 1: task ownership ----
// Two webviews are two different owner labels on the same Handle — exactly
// what the Tauri commands in lib.rs supply via window.label().

#[tokio::test]
async fn non_owner_cannot_cancel_another_windows_task() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    let ct = h
        .call_task(
            "sleep_forever",
            json!({}),
            Some("win-a".into()),
            Some(false),
        )
        .unwrap();
    let task = timeout(T, ct.task)
        .await
        .expect("task id timed out")
        .unwrap();

    // A different webview's cancel is rejected — nothing is delivered.
    let res = timeout(T, h.cancel(task, Some("win-b".into())))
        .await
        .expect("cancel timed out")
        .unwrap();
    assert_eq!(
        res,
        CancelOutcome::Rejected,
        "window B cancelled or touched window A's task — ownership check missing"
    );

    // The task must still be running (owner's work untouched).
    let still_running = timeout(Duration::from_secs(2), ct.result).await.is_err();
    assert!(still_running, "window B's cancel terminated A's task");

    // The real owner can still cancel — the check scopes, it doesn't block.
    let res = timeout(T, h.cancel(task, Some("win-a".into())))
        .await
        .expect("owner cancel timed out")
        .unwrap();
    assert_eq!(
        res,
        CancelOutcome::Cancelled,
        "owner's cancel was not honoured"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn non_owner_cannot_grant_credit_on_another_windows_stream() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.initial_stream_credit = 2; // stream blocks quickly without credit
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let (task, mut items) = timeout(
        T,
        h.call_stream(
            "counter",
            json!({"n": 1_000}),
            Some("win-a".into()),
            Some(true),
        ),
    )
    .await
    .expect("call_stream timed out")
    .unwrap();

    // Drain the initial credit window first so a leak is attributable.
    let mut seen = 0usize;
    while let Ok(Some(item)) = timeout(Duration::from_secs(2), items.recv()).await {
        if !matches!(
            item,
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { .. }
        ) {
            break;
        }
        seen += 1;
        if seen >= 2 {
            break;
        }
    }
    assert_eq!(seen, 2, "initial credit never produced its chunks");

    let res = timeout(T, h.grant_credit(task, 10_000, Some("win-b".into())))
        .await
        .expect("grant_credit timed out");
    assert!(
        res.is_err(),
        "window B granted credit on window A's stream — no ownership check on python_grant_credit"
    );

    // B's credit must not have advanced A's stream.
    let leaked = timeout(Duration::from_secs(2), items.recv())
        .await
        .map(|o| o.is_some())
        .unwrap_or(false);
    assert!(!leaked, "window B's credit produced chunks on A's stream");

    // The owner's own credit still flows.
    h.grant_credit(task, 4, Some("win-a".into()))
        .await
        .expect("owner credit errored");
    let mut flowed = 0usize;
    while let Ok(Some(item)) = timeout(Duration::from_secs(3), items.recv()).await {
        if matches!(
            item,
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { .. }
        ) {
            flowed += 1;
            if flowed >= 2 {
                break;
            }
        } else {
            break;
        }
    }
    assert!(flowed >= 2, "owner's credit failed to advance the stream");
    h.shutdown().await;
}

// ---- claim 2: reliable control delivery ----

/// The data lane is a bounded (64) mpsc; a wedged reader fills it. The
/// control lane must still deliver — and where nothing can be delivered or
/// acknowledged, the outcome must be loud and distinct, never ambiguous.
#[tokio::test]
async fn cancel_not_dropped_when_frame_queue_full() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    // Keep the wedged runtime alive: it can't pong, so the liveness
    // killer must not fire during the flood.
    cfg.missed_pongs_to_kill = 1_000_000;
    let h = spawn_h(spec(FAKE_WEDGED), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Register a real task while the data lane is empty — its frame sits
    // in the pipe buffer: delivered, never read by the wedged runtime.
    let ct = h.call_task("echo", json!({}), None, Some(false)).unwrap();
    let task = timeout(T, ct.task)
        .await
        .expect("task id timed out")
        .unwrap();

    // Flood the data lane until a send reports it full. Frames that DID
    // queue never get a response, so awaiting each result would burn the
    // whole timeout per call; instead a drainer task watches results for
    // the loud saturated-queue signal (RUNTIME_CRASHED or the typed
    // LIMIT_EXCEEDED on the frame-out queue / cmd lane).
    let big = "x".repeat(16 * 1024);
    let (full_tx, mut full_rx) = mpsc::channel::<()>(1);
    for _ in 0..400 {
        match h.call_task("echo", json!({"input": big}), None, Some(false)) {
            Ok(ct) => {
                let tx = full_tx.clone();
                tokio::spawn(async move {
                    if let Ok(Err(e)) = ct.result.await {
                        if e.code == proto::RUNTIME_CRASHED || e.code == proto::LIMIT_EXCEEDED {
                            let _ = tx.send(()).await;
                        }
                    }
                });
            }
            Err(_) => break,
        }
        tokio::task::yield_now().await;
    }
    let queue_full = timeout(Duration::from_secs(10), full_rx.recv())
        .await
        .map(|o| o.is_some())
        .unwrap_or(false);
    assert!(queue_full, "never managed to fill the frame queue");

    // The cancel still leaves on the control lane — the wedged runtime
    // just never acks it, so the outcome is a loud bounded error.
    let res = timeout(T, h.cancel(task, None))
        .await
        .expect("cancel timed out");
    assert!(
        res.is_err(),
        "cancel under a full data queue must be a loud outcome, not ambiguity"
    );

    // An unknown task gets its own distinct outcome — and the lookup
    // never touches a queue at all, so it cannot starve.
    let res = timeout(T, h.cancel(999_999, None))
        .await
        .expect("cancel timed out")
        .unwrap();
    assert_eq!(res, CancelOutcome::NotFound);

    // Credit rides the control lane: delivered even with the data lane full.
    timeout(T, h.grant_credit(task, 8, None))
        .await
        .expect("grant_credit timed out")
        .expect("credit on the control lane should deliver under a data flood");

    // Credit on an unknown task fails loudly, not silently.
    let res = timeout(T, h.grant_credit(999_999, 8, None))
        .await
        .expect("grant_credit timed out");
    assert_eq!(
        res.unwrap_err().code,
        proto::TASK_NOT_FOUND,
        "credit on an unknown task must fail with a real error"
    );
    h.shutdown().await;
}

// ---- claim 3: parent-death cleanup ----

/// Re-exec harness: the controller spawns this same test binary filtered
/// to this test with PYLAIR_ORPHAN_HOST set; that child plays "the app"
/// and is killed abruptly so no Rust Drop ever runs. The supervisor's
/// children are then surveyed via the platform's process tools.
#[cfg(unix)]
fn children_of(pid: u32) -> Vec<u32> {
    let out = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[cfg(windows)]
fn children_of(pid: u32) -> Vec<u32> {
    // CIM is the supported way to walk the parent->child edge on Windows.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}').ProcessId"),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// (state, starttime token) for a watched pid. Linux reads
/// /proc/<pid>/stat — parsed after the last ')' so a comm containing
/// spaces/parens can't shift the columns (starttime is field 22, index
/// 19 of the post-')' tokens). Other unix has no /proc — ps prints
/// state + start time instead.
#[cfg(unix)]
fn proc_state_start(pid: u32) -> Option<(char, String)> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.rsplit_once(')')?.1;
        let mut f = rest.split_whitespace();
        let state = f.next()?.chars().next()?;
        let start = f.nth(18)?.to_string();
        Some((state, start))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat=", "-o", "lstart="])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let state = line.chars().next()?;
        let start = line[1..].trim().to_string();
        Some((state, start))
    }
}

/// Identity token for a watched pid: captured at discovery so a recycled
/// pid later reads as not-ours instead of a survivor.
#[cfg(unix)]
fn token(pid: u32) -> String {
    proc_state_start(pid).map(|(_, s)| s).unwrap_or_default()
}

#[cfg(windows)]
fn token(pid: u32) -> String {
    // Process start time in ticks — unique enough to spot pid reuse.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).StartTime.Ticks"),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A watched pid survives only while the same process is genuinely
/// alive: kill -0 sees zombies as alive (a zombie is dead, just unreaped
/// — not a survivor), and a pid recycled by another process is not our
/// child (starttime won't match).
#[cfg(unix)]
fn alive_since(pid: u32, start: &str) -> bool {
    match proc_state_start(pid) {
        Some(('Z' | 'X', _)) | None => false,
        Some((_, s)) => s == start,
    }
}

/// Windows liveness via the process object's signaled state, not tasklist
/// presence. tasklist (and Get-CimInstance) still list a process that is
/// mid-termination or dead-but-referenced — on a loaded runner the object
/// can outlive TerminateProcess/job-close by seconds, which reads as a
/// survivor that isn't (the rc.15 CI flake). HasExited checks the signaled
/// state, which is what "alive" actually means. One powershell call also
/// returns the start-time token, so a recycled pid reads as not-ours.
#[cfg(windows)]
fn alive_since(pid: u32, start: &str) -> bool {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; \
                 if ($null -eq $p -or $p.HasExited) {{ 'gone' }} \
                 else {{ 'alive:' + $p.StartTime.Ticks }}"
            ),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim() == format!("alive:{start}")
}

/// Failure-path diagnostics: what each survivor actually is and who
/// spawned it, so the next occurrence names itself instead of a bare pid.
#[cfg(not(windows))]
fn describe_survivor(pid: u32, _start: &str) -> String {
    format!("pid {pid} still alive — check /proc/{pid} or ps")
}

#[cfg(windows)]
fn describe_survivor(pid: u32, start: &str) -> String {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p = Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; \
                 if ($null -eq $p) {{ 'pid {pid}: gone (was a lingering object, or exited since the poll)' }} \
                 else {{ $par = Get-CimInstance Win32_Process -Filter \"ProcessId=$($p.ParentProcessId)\"; \
                    \"pid $($p.ProcessId) name=$($p.Name) start_ticks={start} parent=$($p.ParentProcessId)($($par.Name)) cmd=$($p.CommandLine)\" }}"
            ),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Abrupt host kill — SIGKILL on unix, taskkill /F on Windows. Deliberately
/// NOT /T on Windows: if we killed the tree ourselves the Job Object test
/// would prove nothing — the kernel must clean up the descendants.
#[cfg(unix)]
fn kill_host(pid: u32) {
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .unwrap();
}

#[cfg(windows)]
fn kill_host(pid: u32) {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .unwrap();
}

/// Kill surveyed pids on drop so a failed assertion doesn't leak orphans.
struct Reap(Vec<u32>);
impl Drop for Reap {
    fn drop(&mut self) {
        for p in self.0.clone() {
            kill_host(p);
        }
    }
}

/// A wedged runtime can never observe stdin EOF — the EOF fallback cannot
/// reach it. On Linux, PR_SET_PDEATHSIG is the kernel-level answer: the
/// child gets SIGKILL the moment the host dies. On Windows the Job Object
/// does the same — the kernel kills the whole job when the host's last
/// handle closes. macOS has no pdeathsig and no jobs; the documented
/// boundary there is EOF-only (healthy runtimes die — proven by the test
/// below — wedged ones are the accepted gap).
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn host_sigkill_does_not_orphan_wedged_runtime() {
    if std::env::var("PYLAIR_ORPHAN_HOST").is_ok() {
        // Host role: spawn the wedged runtime, then live forever until
        // killed. The wedged fake can't pong — disable the liveness kill
        // so only PDEATHSIG (or its absence) decides.
        let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
        let mut cfg = fast_config();
        cfg.missed_pongs_to_kill = 1_000_000;
        let _h = spawn_h(spec(FAKE_WEDGED), cfg, ev);
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }

    let me = std::env::current_exe().unwrap();
    let mut host = std::process::Command::new(me)
        .args([
            "host_sigkill_does_not_orphan_wedged_runtime",
            "--exact",
            "--nocapture",
        ])
        .env("PYLAIR_ORPHAN_HOST", "1")
        .spawn()
        .expect("spawn host");
    let host_pid = host.id();

    // Wait for the runtime child to exist (poll; spawn takes a moment).
    let mut kids = Vec::new();
    for _ in 0..100 {
        kids = children_of(host_pid);
        if !kids.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _reap = Reap(kids.clone());
    assert!(!kids.is_empty(), "host never spawned a runtime child");
    let watched: Vec<(u32, String)> = kids.iter().map(|p| (*p, token(*p))).collect();

    // Abruptly kill the "app" — no Drop, no kill_on_drop, no graceful path.
    kill_host(host_pid);
    let _ = host.wait();

    // PDEATHSIG / Job Object cleanup is immediate — a tight bound is honest.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while watched.iter().any(|(p, s)| alive_since(*p, s)) {
        let survivors: Vec<String> = watched
            .iter()
            .filter(|(p, s)| alive_since(*p, s))
            .map(|(p, s)| describe_survivor(*p, s))
            .collect();
        assert!(
            std::time::Instant::now() < deadline,
            "runtime children survived the host's abrupt kill — \
             PDEATHSIG/Job Object absent: {survivors:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// All-platform invariant: a HEALTHY runtime dies on stdin EOF when the
/// host is killed — the EOF fallback is real coverage, not a fiction.
/// On Linux/Windows the kernel mechanisms may beat EOF to the kill;
/// either way it must be dead.
#[tokio::test]
async fn host_sigkill_healthy_runtime_exits_on_eof() {
    if std::env::var("PYLAIR_ORPHAN_HOST").is_ok() {
        let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
        let _h = spawn_h(spec(FAKE_READY), fast_config(), ev);
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }

    let me = std::env::current_exe().unwrap();
    let mut host = std::process::Command::new(me)
        .args([
            "host_sigkill_healthy_runtime_exits_on_eof",
            "--exact",
            "--nocapture",
        ])
        .env("PYLAIR_ORPHAN_HOST", "1")
        .spawn()
        .expect("spawn host");
    let host_pid = host.id();

    let mut kids = Vec::new();
    for _ in 0..100 {
        kids = children_of(host_pid);
        if !kids.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _reap = Reap(kids.clone());
    assert!(!kids.is_empty(), "host never spawned a runtime child");
    let watched: Vec<(u32, String)> = kids.iter().map(|p| (*p, token(*p))).collect();

    kill_host(host_pid);
    let _ = host.wait();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while watched.iter().any(|(p, s)| alive_since(*p, s)) {
        let survivors: Vec<String> = watched
            .iter()
            .filter(|(p, s)| alive_since(*p, s))
            .map(|(p, s)| describe_survivor(*p, s))
            .collect();
        assert!(
            std::time::Instant::now() < deadline,
            "healthy runtime children survived the host's abrupt kill: {survivors:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The real grandchild case: a REAL runtime with a REAL isolated worker
/// in flight (isolated_sleep). Real workers protect themselves — the
/// ppid watcher in _worker.py killpgs the worker group on reparent, the
/// worker also sets PR_SET_PDEATHSIG on Linux, and on Windows the Job
/// Object nests every descendant by default.
#[tokio::test]
async fn host_sigkill_does_not_orphan_isolated_worker() {
    if std::env::var("PYLAIR_ORPHAN_HOST").is_ok() {
        let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
        let h = spawn_h(real_runtime_spec(), fast_config(), ev);
        wait_state(&h, State::Ready).await;
        // Start the long-running isolated task — the worker it spawns is
        // the grandchild under test.
        let ct = h
            .call_task("isolated_sleep", json!({"seconds": 600}), None, Some(false))
            .expect("isolated call rejected");
        let _ = timeout(T, ct.task).await.expect("task id timed out");
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }

    let me = std::env::current_exe().unwrap();
    let mut host = std::process::Command::new(me)
        .args([
            "host_sigkill_does_not_orphan_isolated_worker",
            "--exact",
            "--nocapture",
        ])
        .env("PYLAIR_ORPHAN_HOST", "1")
        .spawn()
        .expect("spawn host");
    let host_pid = host.id();

    // Find the runtime = the child of the host that itself has a child
    // (the isolated worker). Polling for "a child of host" can catch the
    // transient python() version probe instead.
    let (mut runtime_pid, mut worker_pid) = (0u32, 0u32);
    for _ in 0..100 {
        for cand in children_of(host_pid) {
            if let Some(w) = children_of(cand).first() {
                runtime_pid = cand;
                worker_pid = *w;
                break;
            }
        }
        if worker_pid != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(runtime_pid != 0, "host never spawned a runtime child");
    assert!(worker_pid != 0, "runtime never spawned a worker child");

    let _reap = Reap(vec![runtime_pid, worker_pid]);
    let rt = (runtime_pid, token(runtime_pid));
    let wk = (worker_pid, token(worker_pid));
    kill_host(host_pid);
    let _ = host.wait();

    // EOF drain (macOS), PDEATHSIG + the worker's own watcher (Linux), or
    // the Job Object (Windows): whatever the path, NOTHING may survive.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while alive_since(rt.0, &rt.1) || alive_since(wk.0, &wk.1) {
        assert!(
            std::time::Instant::now() < deadline,
            "worker {worker_pid} survived after runtime {runtime_pid} exited — \
             an unreaped isolated worker is an orphan"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---- the pdeathsig anchor must not fire early ----

/// PR_SET_PDEATHSIG is bound to the THREAD that forked the child. If the
/// runtime were spawned on a transient executor thread (a spawn_blocking
/// worker exits at ~10s idle), a healthy runtime would be SIGKILLed
/// shortly after spawn. The spawn happens on a dedicated parked thread
/// instead — this test proves a runtime survives well past the thread-
/// lifetime cliff the mechanism was protecting against.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn pdeathsig_does_not_kill_a_healthy_runtime_early() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    // spawn_blocking keepalive is ~10s; 35s proves the anchor, not a
    // borrowed thread, owns the parent-death binding.
    sleep(Duration::from_secs(35)).await;
    assert_eq!(
        h.state(),
        State::Ready,
        "runtime died within 35s of spawn — pdeathsig fired on a healthy runtime"
    );

    // And it must still serve a call — "alive" means functioning.
    let out = timeout(T, h.call("add", json!({"a": 1, "b": 2}), None, Some(false)))
        .await
        .expect("call timed out")
        .expect("call failed after the wait");
    assert_eq!(out, json!(3));
    h.shutdown().await;
}
