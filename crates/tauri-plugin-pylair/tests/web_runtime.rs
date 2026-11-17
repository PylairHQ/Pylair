//! W1: the `Web` runtime kind (docs/protocol-v1.md section 4.1).
//!
//! Two kinds of child are used. Hand-written fakes (`python -c`) speak just
//! enough protocol to pin the supervisor's own rules: ready on `ready` and
//! not on `hello`, `ready_timeout`, url validation, shutdown while waiting.
//! The real launcher (`python -m pylair.wrap`) with `wrap_server.py` as the
//! web server covers the end-to-end faults: a server that never becomes
//! ready, crash after ready and recovery on a new url, a hung server, stray
//! stdout, a lost port race, and grandchild leaks (POSIX group kill and the
//! Windows Job Object).
//!
//! Timing note: these tests start many Python processes, so each wait is
//! generous. If one fails while another build is saturating the machine,
//! rerun it before treating it as real.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, RuntimeKind, ShutdownOutcome, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(45);

// ---- harness ----

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
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

fn server_script() -> String {
    root()
        .join("python/pylair/tests/wrap_server.py")
        .to_string_lossy()
        .into_owned()
}

/// The real launcher in front of `wrap_server.py PORT <server_args>`.
fn launcher_spec(launcher_opts: &[&str], server_args: &[String]) -> SpawnSpec {
    let mut args: Vec<String> = vec!["-m".into(), "pylair.wrap".into()];
    args.extend(launcher_opts.iter().map(|s| s.to_string()));
    args.extend([
        "--cmd".into(),
        "python".into(),
        server_script(),
        "{port}".into(),
    ]);
    args.extend(server_args.iter().cloned());
    SpawnSpec {
        program: python(),
        args,
        env: vec![(
            "PYTHONPATH".into(),
            root()
                .join("python/pylair/src")
                .to_string_lossy()
                .into_owned(),
        )],
        cwd: Some(root().join("python/pylair/tests")),
        ..Default::default()
    }
}

/// A fake runtime: `body` runs after `hello` has been written. `w(obj)`
/// writes one frame, `ready(url)` writes a `ready` notification.
fn fake_spec(body: &str) -> SpawnSpec {
    let script = format!(
        r#"
import sys, struct, json, time, threading
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
def ready(url):
    w({{"jsonrpc":"2.0","method":"ready","params":{{"url":url}}}})
w({{"jsonrpc":"2.0","method":"hello","params":{{"protocol":1,"exposes":[],"kind":"web"}}}})
def pump():
    while True:
        h = sys.stdin.buffer.read(4)
        if len(h) < 4: break
        n, = struct.unpack(">I", h)
        m = json.loads(sys.stdin.buffer.read(n))
        if m.get("method") == "ping":
            w({{"jsonrpc":"2.0","method":"pong","params":{{"seq": m["params"]["seq"]}}}})
        elif m.get("method") == "shutdown":
            w({{"jsonrpc":"2.0","id": m["id"],"result": {{}}}}); import os; os._exit(0)
threading.Thread(target=pump, daemon=True).start()
{body}
"#
    );
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script],
        ..Default::default()
    }
}

fn cfg(kind: RuntimeKind) -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.kind = kind;
    c.hello_timeout = Duration::from_secs(30);
    c.ready_timeout = Duration::from_secs(60);
    c.ping_interval = Duration::from_millis(200);
    c.missed_pongs_to_kill = 1000; // liveness is not what these tests probe
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 1000;
    c.breaker_window = Duration::from_secs(60);
    c.shutdown_grace = Duration::from_secs(3);
    c.loop_stall_kill = None;
    c
}

/// A running supervisor plus a recorder of every event it emits.
struct Rig {
    h: Handle,
    events: Arc<Mutex<Vec<RuntimeEvent>>>,
}

fn start(spec: SpawnSpec, config: SupervisorConfig) -> Rig {
    let (ev_tx, mut ev_rx) = mpsc::channel::<RuntimeEvent>(4096);
    let (h, actor) = spawn(spec, config, ev_tx);
    tokio::spawn(actor);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    tokio::spawn(async move {
        while let Some(e) = ev_rx.recv().await {
            sink.lock().unwrap().push(e);
        }
    });
    Rig { h, events }
}

impl Rig {
    fn states(&self) -> Vec<(State, State, String)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::StateChanged { from, to, detail } => {
                    Some((*from, *to, detail.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn stderr(&self) -> String {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::Stderr(l) => Some(l.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn saw_hello(&self) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Exposes(_)))
    }

    /// The detail of the first transition into `to`, once it has happened.
    async fn wait_for_transition(&self, to: State) -> String {
        let t0 = Instant::now();
        loop {
            if let Some((_, _, d)) = self.states().into_iter().find(|(_, t, _)| *t == to) {
                return d;
            }
            assert!(
                t0.elapsed() < T,
                "never saw a transition to {to:?}; states: {:?}\nstderr:\n{}",
                self.states(),
                self.stderr()
            );
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_state(&self, want: State) {
        let mut rx = self.h.subscribe_state();
        timeout(T, async {
            while *rx.borrow() != want {
                rx.changed().await.expect("state channel closed");
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "never reached {want:?} (now {:?}); states: {:?}\nstderr:\n{}",
                self.h.state(),
                self.states(),
                self.stderr()
            )
        });
    }

    /// Ready with a url: the first Ready after `previous` (if given).
    async fn wait_ready_url(&self, previous: Option<&str>) -> String {
        let t0 = Instant::now();
        loop {
            if self.h.state() == State::Ready {
                if let Some(u) = self.h.ready_url() {
                    if Some(u.as_str()) != previous {
                        return u;
                    }
                }
            }
            assert!(
                t0.elapsed() < T,
                "no new ready url (state {:?}, url {:?}, previous {previous:?}); states: {:?}\nstderr:\n{}",
                self.h.state(),
                self.h.ready_url(),
                self.states(),
                self.stderr()
            );
            sleep(Duration::from_millis(25)).await;
        }
    }
}

/// A scratch directory under the system temp dir, removed on drop.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new() -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "pylair-web-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

async fn read_pid(p: &Path) -> u32 {
    let t0 = Instant::now();
    loop {
        if let Some(n) = std::fs::read_to_string(p)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return n;
        }
        assert!(t0.elapsed() < T, "no pid in {}", p.display());
        sleep(Duration::from_millis(25)).await;
    }
}

// ---- process liveness and a targeted kill (only pids a test started) ----

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    // A zombie is dead, only waiting to be reaped.
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    !s.is_empty() && !s.starts_with('Z')
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
}

/// Kill exactly this process, not its tree.
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }
}

async fn wait_dead(pid: u32, what: &str) {
    let t0 = Instant::now();
    while alive(pid) {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "{what} (pid {pid}) is still alive: the process tree leaked"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

// ---- supervisor rules, with fake runtimes ----

#[tokio::test]
async fn ready_is_reached_on_ready_not_on_hello() {
    // hello now, ready 3s later.
    let rig = start(
        fake_spec(r#"time.sleep(3); ready("http://127.0.0.1:4242/"); time.sleep(600)"#),
        cfg(RuntimeKind::Web),
    );
    let t0 = Instant::now();
    while !rig.saw_hello() {
        assert!(t0.elapsed() < T, "no hello");
        sleep(Duration::from_millis(10)).await;
    }
    // hello is in; the host must NOT be ready and has no url yet.
    assert_eq!(rig.h.state(), State::Starting);
    assert_eq!(rig.h.ready_url(), None);
    assert!(rig.states().iter().all(|(_, to, _)| *to != State::Ready));

    rig.wait_state(State::Ready).await;
    // The url is published before the Ready transition is visible.
    assert_eq!(rig.h.ready_url().as_deref(), Some("http://127.0.0.1:4242/"));
    assert_eq!(rig.h.kind(), RuntimeKind::Web);

    assert_eq!(rig.h.shutdown().await, ShutdownOutcome::Terminated);
    assert_eq!(
        rig.h.ready_url(),
        None,
        "url must be cleared with the generation"
    );
}

#[tokio::test]
async fn hello_but_no_ready_hits_ready_timeout_not_hello_timeout() {
    // A bridge-style runtime under a web supervisor: hello, then silence.
    let mut c = cfg(RuntimeKind::Web);
    c.hello_timeout = Duration::from_secs(30);
    c.ready_timeout = Duration::from_secs(1);
    let rig = start(fake_spec("time.sleep(600)"), c);
    let t0 = Instant::now();
    let detail = rig.wait_for_transition(State::Restarting).await;
    assert!(
        t0.elapsed() < Duration::from_secs(25),
        "the 30s hello_timeout must not be what fired"
    );
    assert!(
        detail.contains("no `ready`") && detail.contains("ready_timeout"),
        "detail should name ready and the knob: {detail}"
    );
    assert!(
        rig.states().iter().all(|(_, to, _)| *to != State::Ready),
        "must never have been Ready: {:?}",
        rig.states()
    );
    assert_eq!(rig.h.ready_url(), None);
    rig.h.shutdown().await;
}

#[tokio::test]
async fn non_loopback_ready_urls_are_a_protocol_violation() {
    for bad in [
        "http://evil.example/",
        "http://localhost:5000/",
        "https://127.0.0.1:5000/",
        "http://127.0.0.1/",
        "http://127.0.0.1@evil.example:5000/",
        "http://[::1]:5000/",
    ] {
        let mut c = cfg(RuntimeKind::Web);
        c.breaker_crashes = 1; // first violation -> Failed, no backoff loop
        let rig = start(fake_spec(&format!(r#"ready({bad:?}); time.sleep(600)"#)), c);
        let detail = rig.wait_for_transition(State::Failed).await;
        assert!(
            detail.contains("protocol violation") && detail.contains("ready url"),
            "{bad}: {detail}"
        );
        assert!(
            rig.states().iter().all(|(_, to, _)| *to != State::Ready),
            "{bad} must never reach Ready"
        );
        assert_eq!(rig.h.ready_url(), None, "{bad}");
        rig.h.shutdown().await;
    }
}

#[tokio::test]
async fn ready_without_a_url_is_a_protocol_violation() {
    let mut c = cfg(RuntimeKind::Web);
    c.breaker_crashes = 1;
    let rig = start(
        fake_spec(r#"w({"jsonrpc":"2.0","method":"ready","params":{}}); time.sleep(600)"#),
        c,
    );
    let detail = rig.wait_for_transition(State::Failed).await;
    assert!(detail.contains("protocol violation"), "{detail}");
    rig.h.shutdown().await;
}

#[tokio::test]
async fn shutdown_while_waiting_for_ready_is_prompt() {
    let mut c = cfg(RuntimeKind::Web);
    c.ready_timeout = Duration::from_secs(600);
    let rig = start(fake_spec("time.sleep(600)"), c);
    let t0 = Instant::now();
    while !rig.saw_hello() {
        assert!(t0.elapsed() < T, "no hello");
        sleep(Duration::from_millis(10)).await;
    }
    let t1 = Instant::now();
    let outcome = rig.h.shutdown().await;
    assert!(
        t1.elapsed() < Duration::from_secs(10),
        "shutdown waited out the ready window: {:?}",
        t1.elapsed()
    );
    assert_eq!(outcome, ShutdownOutcome::ForcedKill);
    assert_eq!(rig.h.state(), State::Stopped);
}

#[tokio::test]
async fn calls_fail_fast_while_waiting_and_after_ready() {
    let rig = start(
        fake_spec(r#"time.sleep(3); ready("http://127.0.0.1:4242/"); time.sleep(600)"#),
        cfg(RuntimeKind::Web),
    );
    let t0 = Instant::now();
    while !rig.saw_hello() {
        assert!(t0.elapsed() < T, "no hello");
        sleep(Duration::from_millis(10)).await;
    }
    // Waiting for ready: nothing to queue the call for.
    let err = rig.h.call("anything", json!({}), None, Some(false)).await;
    assert_eq!(
        err.unwrap_err().code,
        tauri_plugin_pylair::protocol::RUNTIME_UNAVAILABLE
    );
    assert_eq!(
        rig.h.state(),
        State::Starting,
        "the call must not disturb the wait"
    );
    rig.wait_state(State::Ready).await;
    rig.h.shutdown().await;
}

#[tokio::test]
async fn bridge_kind_is_unchanged_by_ready_and_has_no_url() {
    // Default kind: Ready on hello. A `ready` notification (bad url and all)
    // is just an unknown notification and is ignored.
    let c = cfg(RuntimeKind::Bridge);
    assert_eq!(SupervisorConfig::default().kind, RuntimeKind::Bridge);
    let rig = start(
        fake_spec(r#"ready("http://evil.example/"); time.sleep(600)"#),
        c,
    );
    rig.wait_state(State::Ready).await;
    sleep(Duration::from_millis(800)).await;
    assert_eq!(
        rig.h.state(),
        State::Ready,
        "a stray ready must not crash a bridge runtime"
    );
    assert_eq!(rig.h.ready_url(), None);
    assert_eq!(rig.h.kind(), RuntimeKind::Bridge);
    assert!(rig
        .states()
        .iter()
        .all(|(_, to, _)| *to != State::Restarting));
    rig.h.shutdown().await;
}

#[tokio::test]
async fn ready_timeout_is_clamped_and_reported() {
    let mut c = cfg(RuntimeKind::Web);
    c.ready_timeout = Duration::from_millis(10);
    let rig = start(fake_spec("time.sleep(600)"), c);
    let t0 = Instant::now();
    while !rig.stderr().contains("ready_timeout=10ms") {
        assert!(t0.elapsed() < T, "no clamp warning: {}", rig.stderr());
        sleep(Duration::from_millis(10)).await;
    }
    // Clamped up to 1s, so it fires at ~1s, not instantly.
    let detail = rig.wait_for_transition(State::Restarting).await;
    assert!(detail.contains("1s"), "{detail}");
    rig.h.shutdown().await;
}

// ---- end to end, with the real launcher ----

#[tokio::test]
async fn real_launcher_reaches_ready_with_a_loopback_url() {
    let rig = start(launcher_spec(&[], &[]), cfg(RuntimeKind::Web));
    let url = rig.wait_ready_url(None).await;
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with('/'),
        "{url}"
    );
    // The state event carries a url on the states where a server is up.
    let (_, _, d) = rig
        .states()
        .into_iter()
        .find(|(_, to, _)| *to == State::Ready)
        .unwrap();
    assert!(d.contains("ready"), "{d}");
    // A call reaches the launcher and gets a typed answer, not a hang.
    let err = rig
        .h
        .call("x", json!({}), None, Some(false))
        .await
        .unwrap_err();
    assert_eq!(err.code, tauri_plugin_pylair::protocol::FUNCTION_NOT_FOUND);
    rig.h.shutdown().await;
}

#[tokio::test]
async fn server_that_never_becomes_ready_times_out_and_leaves_no_tree() {
    let dir = TmpDir::new();
    let spid = dir.path().join("s.pid");
    let gpid = dir.path().join("g.pid");
    let mut c = cfg(RuntimeKind::Web);
    c.ready_timeout = Duration::from_secs(2);
    let rig = start(
        launcher_spec(
            &[],
            &args(&[
                "--never-listen",
                "--pidfile",
                spid.to_str().unwrap(),
                "--grandchild",
                gpid.to_str().unwrap(),
            ]),
        ),
        c,
    );
    let (server, grandchild) = (read_pid(&spid).await, read_pid(&gpid).await);
    let detail = rig.wait_for_transition(State::Restarting).await;
    assert!(detail.contains("no `ready`"), "{detail}");
    assert!(
        rig.states().iter().all(|(_, to, _)| *to != State::Ready),
        "never ready, never Ready"
    );
    // The supervisor's kill reaped the launcher's whole tree.
    wait_dead(server, "server").await;
    wait_dead(grandchild, "grandchild").await;
    rig.h.shutdown().await;
}

#[tokio::test]
async fn crash_after_ready_recovers_on_a_new_url() {
    let dir = TmpDir::new();
    let spid = dir.path().join("s.pid");
    let rig = start(
        launcher_spec(&[], &args(&["--pidfile", spid.to_str().unwrap()])),
        cfg(RuntimeKind::Web),
    );
    let url1 = rig.wait_ready_url(None).await;
    kill_pid(read_pid(&spid).await); // the server only, not the launcher
    let detail = rig.wait_for_transition(State::Restarting).await;
    assert!(
        detail.contains('4'),
        "launcher exit code 4 expected: {detail}"
    );
    let url2 = rig.wait_ready_url(Some(&url1)).await;
    assert_ne!(url1, url2, "a restart is a new port and a new url");
    // The url was cleared between the two generations.
    assert!(rig
        .states()
        .iter()
        .any(|(_, to, _)| *to == State::Restarting));
    rig.h.shutdown().await;
    assert_eq!(rig.h.ready_url(), None);
}

#[tokio::test]
async fn lost_port_race_exits_and_the_restart_gets_a_new_port() {
    // The server's first start exits 98 at once, as a server that lost the
    // port between the launcher's probe-bind and its own bind would.
    let dir = TmpDir::new();
    let state = dir.path().join("starts");
    let rig = start(
        launcher_spec(
            &[],
            &args(&[
                "--fail-starts",
                "1",
                "--state-file",
                state.to_str().unwrap(),
            ]),
        ),
        cfg(RuntimeKind::Web),
    );
    let detail = rig.wait_for_transition(State::Restarting).await;
    assert!(detail.contains('4'), "{detail}");
    let url = rig.wait_ready_url(None).await;
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(rig.stderr().contains("address already in use"));
    rig.h.shutdown().await;
}

#[tokio::test]
async fn hung_server_probe_misses_lead_to_exit_and_restart() {
    let rig = start(
        launcher_spec(
            &[
                "--probe-interval",
                "0.3",
                "--probe-timeout",
                "0.3",
                "--max-misses",
                "3",
            ],
            &args(&["--hang-after", "1"]),
        ),
        cfg(RuntimeKind::Web),
    );
    let url1 = rig.wait_ready_url(None).await;
    // Pings are answered by the launcher while the server hangs: the host
    // sees a healthy process until the launcher itself gives up.
    let detail = rig.wait_for_transition(State::Restarting).await;
    assert!(
        detail.contains('5'),
        "launcher exit code 5 expected: {detail}"
    );
    assert!(
        rig.stderr().contains("health probe missed (3/3)"),
        "{}",
        rig.stderr()
    );
    assert!(!rig.stderr().contains("(4/3)"));
    // The restarted pair serves again, on its own url.
    let url2 = rig.wait_ready_url(Some(&url1)).await;
    assert_ne!(url1, url2);
    rig.h.shutdown().await;
}

#[tokio::test]
async fn stray_stdout_from_the_app_never_corrupts_the_frame_channel() {
    let mut c = cfg(RuntimeKind::Web);
    c.ping_interval = Duration::from_millis(100);
    c.missed_pongs_to_kill = 3; // a corrupted channel would trip this fast
    let rig = start(launcher_spec(&[], &args(&["--noise"])), c);
    rig.wait_ready_url(None).await;
    sleep(Duration::from_millis(2500)).await; // ~25 pings through the noise
    assert_eq!(rig.h.state(), State::Ready);
    assert!(
        rig.states()
            .iter()
            .all(|(_, to, _)| *to != State::Restarting && *to != State::Failed),
        "frame channel was disturbed: {:?}",
        rig.states()
    );
    assert!(
        rig.stderr().contains("server: stdout noise at startup"),
        "app stdout should be on the log stream: {}",
        rig.stderr()
    );
    rig.h.shutdown().await;
}

#[tokio::test]
async fn no_grandchild_survives_shutdown() {
    let dir = TmpDir::new();
    let (spid, gpid) = (dir.path().join("s.pid"), dir.path().join("g.pid"));
    let rig = start(
        launcher_spec(
            &[],
            &args(&[
                "--grandchild",
                gpid.to_str().unwrap(),
                "--pidfile",
                spid.to_str().unwrap(),
            ]),
        ),
        cfg(RuntimeKind::Web),
    );
    rig.wait_ready_url(None).await;
    let (server, grandchild) = (read_pid(&spid).await, read_pid(&gpid).await);
    assert!(alive(server) && alive(grandchild));
    rig.h.shutdown().await;
    wait_dead(server, "server").await;
    wait_dead(grandchild, "grandchild (ignores SIGTERM on POSIX)").await;
}

#[tokio::test]
async fn orphaned_grandchild_dies_when_the_server_is_killed() {
    // The server dies on its own and leaves its grandchild reparented: the
    // launcher exits, and the supervisor's group reap (POSIX) or Job Object
    // (Windows) must still take the grandchild with it.
    let dir = TmpDir::new();
    let (spid, gpid) = (dir.path().join("s.pid"), dir.path().join("g.pid"));
    let rig = start(
        launcher_spec(
            &[],
            &args(&[
                "--grandchild",
                gpid.to_str().unwrap(),
                "--pidfile",
                spid.to_str().unwrap(),
            ]),
        ),
        cfg(RuntimeKind::Web),
    );
    let url1 = rig.wait_ready_url(None).await;
    let (server, grandchild) = (read_pid(&spid).await, read_pid(&gpid).await);
    kill_pid(server);
    rig.wait_for_transition(State::Restarting).await;
    wait_dead(grandchild, "orphaned grandchild").await;
    // ...and the restart is healthy again with a fresh tree.
    rig.wait_ready_url(Some(&url1)).await;
    rig.h.shutdown().await;
}

#[tokio::test]
async fn launcher_killed_hard_takes_the_whole_tree_with_it() {
    // The launcher itself is SIGKILLed / TerminateProcess'd, so its own
    // cleanup never runs. Only the supervisor's containment (process group
    // reap / Job Object) can reach the server and grandchild now.
    let dir = TmpDir::new();
    let (spid, gpid) = (dir.path().join("s.pid"), dir.path().join("g.pid"));
    let rig = start(
        launcher_spec(
            &[],
            &args(&[
                "--grandchild",
                gpid.to_str().unwrap(),
                "--pidfile",
                spid.to_str().unwrap(),
            ]),
        ),
        cfg(RuntimeKind::Web),
    );
    rig.wait_ready_url(None).await;
    let (server, grandchild) = (read_pid(&spid).await, read_pid(&gpid).await);
    let launcher = parent_of(server).expect("the server's parent is the launcher");
    kill_pid(launcher);
    rig.wait_for_transition(State::Restarting).await;
    wait_dead(server, "server").await;
    wait_dead(grandchild, "grandchild").await;
    rig.h.shutdown().await;
}

/// The parent pid of `pid` (the launcher, for the server we started).
fn parent_of(pid: u32) -> Option<u32> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').ParentProcessId"
                ),
            ])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}
