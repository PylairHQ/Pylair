//! Characterisation tests for `run_child` (supervisor/run.rs): one test per
//! guarantee the session loop gives, written before the loop is carved into
//! phases so the carve can be proven behaviour-preserving. Every assertion
//! goes through the public surface only: the `Handle`, the event channel,
//! shutdown outcomes and (unix) process state.
//!
//! The fake runtime below is steered by ordinary calls (`hold`, `finish`,
//! `chunks`, `emit`, `set`, `stop_reading`, `eof_then_exit`, `die`) and by a
//! JSON plan in `FAKE_PLAN` for the handshake and shutdown behaviour. With
//! `"log": true` it reports every frame it reads on stderr as `RX {json}`,
//! which lets a test check what reached the wire and in which order.
//!
//! Process state: the fake prints its pid first. The event recorder checks,
//! at the moment each state event arrives, whether that pid still exists
//! (unix: alive or not yet reaped; Windows: still running). "Killed and
//! reaped before the state changed" is therefore `present == Some(false)` on
//! the event. "Callers were failed before the kill" is observed by a spawned
//! watcher task; see `Watch` for why that observation is deterministic.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, CancelOutcome, Handle, RuntimeEvent, ShutdownOutcome, SpawnSpec, State, StreamItem,
    SupervisorConfig,
};
use tauri_plugin_pylair::ErrorObj;
use tokio::sync::{mpsc, Notify};
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(30);

const FAKE: &str = r#"
import sys, os, json, struct, time, threading
plan = json.loads(os.environ.get("FAKE_PLAN") or "{}")
out = sys.stdout.buffer
wlock = threading.Lock()
def raw(b):
    with wlock:
        out.write(struct.pack(">I", len(b)) + b)
        out.flush()
def w(o):
    raw(json.dumps(o).encode())
def log(s):
    sys.stderr.write(s + "\n")
    sys.stderr.flush()
log("FAKE_PID=%d" % os.getpid())
time.sleep(0.3 + plan.get("hello_delay", 0))
hk = plan.get("hello", "ok")
if hk == "not_hello":
    w({"jsonrpc": "2.0", "method": "pong", "params": {"seq": 0}})
    time.sleep(600)
elif hk == "malformed":
    raw(b"nope")
    time.sleep(600)
elif hk == "eof":
    sys.exit(0)
elif hk == "none":
    time.sleep(600)
w({"jsonrpc": "2.0", "method": "hello", "params": {"protocol": plan.get("protocol", 1),
   "exposes": [{"name": "echo", "kind": "sync"}]}})
st = {"stale": 0, "pong": True}
held = {}
inp = sys.stdin.buffer
def read():
    h = inp.read(4)
    if len(h) < 4:
        return None
    n, = struct.unpack(">I", h)
    return json.loads(inp.read(n))
def reply(m, result):
    w({"jsonrpc": "2.0", "id": m["id"], "result": result})
def finish(task, result):
    rid = held.pop(task, None)
    if rid is not None:
        w({"jsonrpc": "2.0", "id": rid, "result": result})
while True:
    m = read()
    if m is None:
        break
    meth = m.get("method")
    p = m.get("params") or {}
    if plan.get("log"):
        log("RX " + json.dumps({"method": meth, "task": p.get("task"), "fn": p.get("fn"),
            "n": p.get("n"), "seq": p.get("seq"), "grace_ms": p.get("grace_ms"),
            "stream": p.get("stream", "absent")}))
    if meth == "ping":
        if st["pong"]:
            w({"jsonrpc": "2.0", "method": "pong",
               "params": {"seq": p["seq"], "loop_stale_ms": st["stale"]}})
    elif meth == "shutdown":
        mode = plan.get("shutdown", "exit")
        if mode == "ignore":
            continue
        if mode == "finish_first" and held:
            t = next(iter(held))
            finish(t, {"finished": t})
        reply(m, {})
        break
    elif meth == "cancel":
        t = p.get("task")
        rid = held.pop(t, None)
        if rid is not None:
            w({"jsonrpc": "2.0", "id": rid, "error": {"code": -32010, "message": "cancelled"}})
        reply(m, plan.get("cancel_reply", {"cancelled": True}))
    elif meth == "call":
        fn = p.get("fn")
        a = p.get("input")
        if fn == "echo" or fn == "sync":
            reply(m, a)
        elif fn == "hold":
            held[p.get("task")] = m["id"]
        elif fn == "finish":
            finish(a["task"], a.get("result"))
            reply(m, True)
        elif fn == "chunks":
            for i in range(a["n"]):
                w({"jsonrpc": "2.0", "method": "stream.chunk",
                   "params": {"task": a["task"], "seq": i, "value": i}})
            reply(m, True)
        elif fn == "emit":
            for f in a["frames"]:
                raw(f.encode())
        elif fn == "set":
            st.update(a)
            reply(m, True)
        elif fn == "stop_reading":
            reply(m, True)
            time.sleep(a)
        elif fn == "eof_then_exit":
            out.flush()
            os.close(1)
            time.sleep(a["after"])
            os._exit(a["code"])
        elif fn == "die":
            for i in range(a.get("junk", 0)):
                w({"jsonrpc": "2.0", "id": 1000000000 + i, "result": None})
            for t in a["finish"]:
                finish(t, {"finished": t})
            os._exit(a["code"])
        else:
            w({"jsonrpc": "2.0", "id": m["id"],
               "error": {"code": -32601, "message": "no fn " + str(fn)}})
"#;

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        // Check exit status, not just spawn: on Windows `python3` can be the
        // Microsoft Store alias, which launches but does not run Python.
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

fn fake(plan: Value) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), FAKE.into()],
        env: vec![("FAKE_PLAN".into(), plan.to_string())],
        cwd: None,
        ..Default::default()
    }
}

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(10);
    c.ping_interval = Duration::from_millis(150);
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 1000;
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

// ---- process state ----

/// Whether `pid` still exists: alive, or exited but not yet reaped.
#[cfg(unix)]
fn pid_present(pid: u32) -> Option<bool> {
    Some(unsafe { libc::kill(pid as i32, 0) } == 0)
}

/// Whether `pid` is still running (an exited process reports its exit
/// code instead of STILL_ACTIVE, 259).
#[cfg(windows)]
fn pid_present(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return Some(false);
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        Some(ok != 0 && code == 259)
    }
}

#[cfg(not(any(unix, windows)))]
fn pid_present(_pid: u32) -> Option<bool> {
    None
}

// ---- event recorder ----

#[derive(Debug, Clone)]
enum Ev {
    State {
        #[allow(dead_code)]
        from: State,
        to: State,
        detail: String,
        /// Whether the latest fake's pid still existed when this event was
        /// received (unix only).
        present: Option<bool>,
    },
    Stderr(String),
    Exposes(Value),
}

#[derive(Clone)]
struct Rec {
    log: Arc<Mutex<Vec<Ev>>>,
    pid: Arc<Mutex<Option<u32>>>,
    bump: Arc<Notify>,
}

impl Rec {
    fn start(mut rx: mpsc::Receiver<RuntimeEvent>) -> Rec {
        let rec = Rec {
            log: Arc::new(Mutex::new(Vec::new())),
            pid: Arc::new(Mutex::new(None)),
            bump: Arc::new(Notify::new()),
        };
        let r = rec.clone();
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                let ev = match e {
                    RuntimeEvent::Stderr(l) => {
                        if let Some(p) = l.trim().strip_prefix("FAKE_PID=") {
                            *r.pid.lock().unwrap() = p.parse().ok();
                        }
                        Ev::Stderr(l)
                    }
                    RuntimeEvent::StateChanged { from, to, detail } => {
                        let pid = *r.pid.lock().unwrap();
                        Ev::State {
                            from,
                            to,
                            detail,
                            present: pid.and_then(pid_present),
                        }
                    }
                    RuntimeEvent::Exposes(x) => Ev::Exposes(x),
                };
                r.log.lock().unwrap().push(ev);
                r.bump.notify_waiters();
            }
        });
        rec
    }

    fn snapshot(&self) -> Vec<Ev> {
        self.log.lock().unwrap().clone()
    }

    async fn wait<R>(&self, what: &str, f: impl Fn(&[Ev]) -> Option<R>) -> R {
        let res = timeout(T, async {
            loop {
                let n = self.bump.notified();
                tokio::pin!(n);
                n.as_mut().enable();
                if let Some(r) = f(&self.log.lock().unwrap()) {
                    return r;
                }
                n.await;
            }
        })
        .await;
        match res {
            Ok(r) => r,
            Err(_) => panic!(
                "timed out waiting for {what}; events: {:#?}",
                self.snapshot()
            ),
        }
    }

    /// The first state event at index >= `from` going to `to` whose detail
    /// contains `needle`.
    async fn state(&self, from: usize, to: State, needle: &str) -> (usize, Ev) {
        self.wait(&format!("state {to:?} containing {needle:?}"), |log| {
            log.iter()
                .enumerate()
                .skip(from)
                .find_map(|(i, e)| match e {
                    Ev::State { to: t, detail, .. } if *t == to && detail.contains(needle) => {
                        Some((i, e.clone()))
                    }
                    _ => None,
                })
        })
        .await
    }

    async fn stderr(&self, needle: &str) -> usize {
        self.wait(&format!("stderr containing {needle:?}"), |log| {
            log.iter()
                .position(|e| matches!(e, Ev::Stderr(l) if l.contains(needle)))
        })
        .await
    }

    async fn pid(&self) -> u32 {
        self.wait("the fake's pid", |_| *self.pid.lock().unwrap())
            .await
    }

    fn pid_lines(&self) -> usize {
        self.snapshot()
            .iter()
            .filter(|e| matches!(e, Ev::Stderr(l) if l.contains("FAKE_PID=")))
            .count()
    }

    /// Every frame the fake logged reading, in order.
    fn rx(&self) -> Vec<Value> {
        self.snapshot()
            .iter()
            .filter_map(|e| match e {
                Ev::Stderr(l) => l
                    .trim()
                    .strip_prefix("RX ")
                    .and_then(|j| serde_json::from_str(j).ok()),
                _ => None,
            })
            .collect()
    }

    async fn rx_until(&self, what: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + T;
        loop {
            let rx = self.rx();
            if f(&rx) {
                return rx;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; frames read: {rx:#?}"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    fn states_after(&self, from: usize) -> Vec<(State, String)> {
        self.snapshot()
            .iter()
            .skip(from)
            .filter_map(|e| match e {
                Ev::State { to, detail, .. } => Some((*to, detail.clone())),
                _ => None,
            })
            .collect()
    }
}

fn start(spec: SpawnSpec, config: SupervisorConfig) -> (Handle, Rec) {
    let (ev, rx) = mpsc::channel::<RuntimeEvent>(4096);
    let rec = Rec::start(rx);
    let (h, actor) = spawn(spec, config, ev);
    tokio::spawn(actor);
    (h, rec)
}

/// Spawn and wait for the first Ready. Returns the index of that event.
async fn ready(plan: Value, config: SupervisorConfig) -> (Handle, Rec, usize) {
    let (h, rec) = start(fake(plan), config);
    let (i, _) = rec.state(0, State::Ready, "handshake complete").await;
    (h, rec, i)
}

/// `shutdown()` with a deadline: a supervisor that never acknowledges
/// must fail the test, not hang it.
async fn stop(h: &Handle) -> ShutdownOutcome {
    timeout(T, h.shutdown())
        .await
        .expect("shutdown() was never acknowledged")
}

fn assert_reaped(ev: &Ev) {
    if let Ev::State {
        present: Some(true),
        detail,
        to,
        ..
    } = ev
    {
        panic!("the child still existed when {to:?} ({detail}) was reported");
    }
}

/// Held exclusively by the five ordering tests (the ones using `Watch`) and
/// shared by every other test in this file, so while an ordering test runs
/// no other test's child process exists. Every test reaps its children
/// before it returns: it ends with `stop()`, which resolves only after
/// cleanup, or (`dropping_every_handle_...`) waits for Stopped, which follows
/// the reap.
static CHILDREN: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

type Reply = Result<Value, ErrorObj>;

/// Observes "callers were failed before the kill" from a SPAWNED task, not
/// from the test body. Why this is deterministic (tokio 1.54,
/// runtime/scheduler/current_thread/mod.rs; these tests run on the
/// current_thread runtime, also #[tokio::test]'s default):
///
/// - A task woken on the runtime thread while another task is being polled
///   goes to the back of one local FIFO queue (`schedule` -> `push_task`,
///   `push_back`); tasks run from the front (`next_local_task`, `pop_front`).
/// - The test body is the `block_on` future. It is polled only between
///   batches of up to `event_interval` (61) queued tasks, so a check in the
///   body can run after the monitor task has already sent the kill. That is
///   the race behind the macOS failure on f3f17eb.
/// - In one poll of the actor, `fail_all` sends the caller's error (waking
///   this watcher, pushed to the back), then `kill_and_reap` does
///   `kill.try_send` (waking the monitor, pushed behind the watcher). Both
///   are synchronous; neither yields. So the watcher runs before the monitor:
///   it checks the child before any kill was sent and sees it alive, on
///   every OS.
/// - In the reversed order `kill_and_reap` awaits the reap (the monitor's
///   `child.wait()`) before the error is sent, so the watcher sees the child
///   gone.
/// - The monitor's only other waker is a child exit. On Linux tokio reaps
///   through a pidfd (its own child only); on Windows through a wait on its
///   own process handle. On macOS the reaper is SIGCHLD-driven, so ANY child
///   exit in the test process wakes every monitor and could queue it ahead of
///   the watcher; `CHILDREN` held exclusively rules that out (the watched
///   test's own child is the only one, and it is alive until the kill).
struct Watch(
    tokio::task::JoinHandle<(
        Result<Reply, tokio::sync::oneshot::error::RecvError>,
        Option<bool>,
    )>,
);

impl Watch {
    /// Spawn the watcher and return once it is waiting on `rx`.
    async fn arm(pid: u32, rx: tokio::sync::oneshot::Receiver<Reply>) -> Watch {
        let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            // One poll: signal armed, then register on rx.
            let _ = armed_tx.send(());
            let r = rx.await;
            (r, pid_present(pid))
        });
        armed_rx.await.expect("watcher never ran");
        Watch(handle)
    }

    /// The caller's result. Panics if the child was already gone when the
    /// watcher woke, i.e. the kill came before the error.
    async fn result(self, what: &str) -> Reply {
        let (r, present) = timeout(T, self.0)
            .await
            .expect("call never resolved")
            .expect("watcher panicked");
        if let Some(false) = present {
            panic!("the child was already gone when {what}");
        }
        r.expect("call channel closed")
    }
}

fn assert_limit(e: &ErrorObj, limit: &str) {
    assert_eq!(e.code, proto::LIMIT_EXCEEDED, "{e:?}");
    assert_eq!(
        e.data.as_ref().and_then(|d| d["limit"].as_str()),
        Some(limit),
        "{e:?}"
    );
}

async fn hold(
    h: &Handle,
    owner: Option<&str>,
) -> (u64, tokio::sync::oneshot::Receiver<Result<Value, ErrorObj>>) {
    let ct = h
        .call_task("hold", json!({}), owner.map(String::from), Some(false))
        .unwrap();
    let task = match timeout(T, ct.task).await.unwrap() {
        Ok(t) => t,
        Err(_) => panic!("hold was rejected: {:?}", ct.result.await),
    };
    // Barrier: `sync` rides the data lane behind the hold, so once it is
    // answered the runtime has registered the hold. Without it a control
    // frame (cancel) could overtake the hold on the wire.
    h.call("sync", json!(task), None, Some(false))
        .await
        .unwrap();
    (task, ct.result)
}

async fn result_of(
    rx: tokio::sync::oneshot::Receiver<Result<Value, ErrorObj>>,
) -> Result<Value, ErrorObj> {
    timeout(T, rx).await.expect("call never resolved").unwrap()
}

/// Every item until the terminal: (chunk seqs, terminal).
async fn drain_stream(
    mut items: tauri_plugin_pylair::supervisor::StreamRx,
) -> (Vec<u64>, Result<Value, ErrorObj>) {
    let mut seqs = Vec::new();
    loop {
        match timeout(T, items.recv()).await.expect("stream stalled") {
            Some(StreamItem::Chunk { seq, .. }) => seqs.push(seq),
            Some(StreamItem::End(r)) => return (seqs, r),
            None => panic!("stream closed without a terminal"),
        }
    }
}

// ---- a. hello ----

/// a: the still-starting timer dies with the handshake; nothing fires
/// after hello even once the half-mark has long passed.
#[tokio::test]
async fn slow_start_event_never_fires_after_hello() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.hello_timeout = Duration::from_secs(6);
    let (h, rec, ready_at) = ready(json!({}), c).await;
    sleep(Duration::from_millis(3500)).await;
    let slow: Vec<_> = rec
        .states_after(ready_at)
        .into_iter()
        .filter(|(to, d)| *to == State::Starting && d.contains("still starting"))
        .collect();
    assert!(slow.is_empty(), "still-starting after hello: {slow:?}");
    stop(&h).await;
}

/// a: a first frame that is not hello is a crash; the child is reaped
/// before Restarting is reported.
#[tokio::test]
async fn hello_first_frame_not_hello_crashes_and_reaps() {
    let _children = CHILDREN.read().await;
    let (h, rec) = start(fake(json!({"hello": "not_hello"})), cfg());
    let (_, ev) = rec
        .state(0, State::Restarting, "first frame was not hello")
        .await;
    assert_reaped(&ev);
    stop(&h).await;
}

/// a: a malformed frame during hello is a crash with the child reaped.
#[tokio::test]
async fn hello_malformed_frame_crashes_and_reaps() {
    let _children = CHILDREN.read().await;
    let (h, rec) = start(fake(json!({"hello": "malformed"})), cfg());
    let (_, ev) = rec
        .state(0, State::Restarting, "protocol violation during hello")
        .await;
    assert_reaped(&ev);
    stop(&h).await;
}

/// a: EOF during hello is reported at once (not at the timeout) as a crash.
#[tokio::test]
async fn hello_eof_crashes_and_reaps() {
    let _children = CHILDREN.read().await;
    let t0 = Instant::now();
    let (h, rec) = start(fake(json!({"hello": "eof"})), cfg());
    let (_, ev) = rec
        .state(0, State::Restarting, "runtime produced no hello within 10s")
        .await;
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "EOF waited for the hello timeout: {:?}",
        t0.elapsed()
    );
    assert_reaped(&ev);
    stop(&h).await;
}

/// a: no hello within the deadline is a crash with the child killed and reaped.
#[tokio::test]
async fn hello_timeout_crashes_kills_and_reaps() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.hello_timeout = Duration::from_secs(1);
    let t0 = Instant::now();
    let (h, rec) = start(fake(json!({"hello": "none"})), c);
    let (_, ev) = rec
        .state(0, State::Restarting, "runtime produced no hello within 1s")
        .await;
    assert!(t0.elapsed() >= Duration::from_secs(1));
    assert_reaped(&ev);
    stop(&h).await;
}

/// a + run_loop: a protocol version mismatch is Fatal: Failed straight
/// away, child reaped, no restart and no second spawn.
#[tokio::test]
async fn version_mismatch_is_fatal_reaped_and_never_restarted() {
    let _children = CHILDREN.read().await;
    let (h, rec) = start(fake(json!({"protocol": 99})), cfg());
    let (_, ev) = rec
        .state(0, State::Failed, "protocol version mismatch")
        .await;
    assert_reaped(&ev);
    sleep(Duration::from_millis(600)).await;
    let restarts: Vec<_> = rec
        .states_after(0)
        .into_iter()
        .filter(|(to, _)| *to == State::Restarting)
        .collect();
    assert!(
        restarts.is_empty(),
        "Fatal entered the restart loop: {restarts:?}"
    );
    assert_eq!(rec.pid_lines(), 1, "a Fatal exit spawned again");
    assert_eq!(h.state(), State::Failed);
    stop(&h).await;
}

// ---- b. exposes ----

/// b: the exposes manifest is emitted before the Ready transition.
#[tokio::test]
async fn exposes_event_precedes_ready() {
    let _children = CHILDREN.read().await;
    let (h, rec, ready_at) = ready(json!({}), cfg()).await;
    let log = rec.snapshot();
    let exposes_at = log
        .iter()
        .position(|e| matches!(e, Ev::Exposes(_)))
        .expect("no exposes event");
    assert!(
        exposes_at < ready_at,
        "exposes (#{exposes_at}) came after Ready (#{ready_at})"
    );
    if let Ev::Exposes(x) = &log[exposes_at] {
        assert_eq!(x[0]["name"], "echo");
    }
    stop(&h).await;
}

// ---- c. response routing and tombstones ----

/// c: responses route by pending kind: a call gets its value, a stream its
/// terminal, a cancel its mapped outcome.
#[tokio::test]
async fn responses_route_by_pending_kind() {
    let _children = CHILDREN.read().await;
    let plan = json!({"cancel_reply": {"cancelled": true, "status": "terminated"}});
    let (h, _rec, _) = ready(plan, cfg()).await;
    let out = h
        .call("echo", json!({"v": 1}), None, Some(false))
        .await
        .unwrap();
    assert_eq!(out, json!({"v": 1}));

    let (task, items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    h.call(
        "finish",
        json!({"task": task, "result": {"r": 2}}),
        None,
        Some(false),
    )
    .await
    .unwrap();
    let (seqs, end) = drain_stream(items).await;
    assert!(seqs.is_empty());
    assert_eq!(end.unwrap(), json!({"r": 2}));

    let (task, result) = hold(&h, None).await;
    assert_eq!(
        h.cancel(task, None).await.unwrap(),
        CancelOutcome::Terminated
    );
    assert_eq!(
        result_of(result).await.unwrap_err().code,
        proto::TASK_CANCELLED
    );
    stop(&h).await;
}

/// c: a finished task is tombstoned (cancel says Completed, not NotFound)
/// and the tombstone set is cleared when it reaches 1024 entries.
#[tokio::test]
async fn finished_tasks_are_tombstoned_and_the_set_clears_at_1024() {
    let _children = CHILDREN.read().await;
    let (h, _rec, _) = ready(json!({}), cfg()).await;
    let first = h.call_task("echo", json!(0), None, Some(false)).unwrap();
    let t1 = first.task.await.unwrap();
    first.result.await.unwrap().unwrap();
    assert_eq!(h.cancel(t1, None).await.unwrap(), CancelOutcome::Completed);
    assert_eq!(
        h.cancel(t1 + 1_000_000, None).await.unwrap(),
        CancelOutcome::NotFound
    );

    // 1023 more: 1024 tombstones, no clear yet.
    let mut left = 1023;
    while left > 0 {
        let n = left.min(32);
        let cts: Vec<_> = (0..n)
            .map(|i| h.call_task("echo", json!(i), None, Some(false)).unwrap())
            .collect();
        for ct in cts {
            ct.result.await.unwrap().unwrap();
        }
        left -= n;
    }
    assert_eq!(
        h.cancel(t1, None).await.unwrap(),
        CancelOutcome::Completed,
        "1024 tombstones must all still be held"
    );

    // The 1025th clears the set before inserting itself.
    let last = h
        .call_task("echo", json!("last"), None, Some(false))
        .unwrap();
    let t_last = last.task.await.unwrap();
    last.result.await.unwrap().unwrap();
    assert_eq!(h.cancel(t1, None).await.unwrap(), CancelOutcome::NotFound);
    assert_eq!(
        h.cancel(t_last, None).await.unwrap(),
        CancelOutcome::Completed
    );
    stop(&h).await;
}

// ---- d. pongs ----

/// d: a pong reporting a stall past loop_stall_kill: Unresponsive, then
/// pending callers get RUNTIME_WEDGED (child still alive), then the kill,
/// then Crashed (Restarting) with the child reaped.
#[tokio::test(flavor = "current_thread")]
async fn pong_stall_past_kill_limit_wedges_callers_then_kills() {
    let _children = CHILDREN.write().await;
    let mut c = cfg();
    c.loop_stall_kill = Some(Duration::from_millis(500));
    let (h, rec, ready_at) = ready(json!({}), c).await;
    let pid = rec.pid().await;
    let (_task, result) = hold(&h, None).await;
    let watch = Watch::arm(pid, result).await;
    h.call("set", json!({"stale": 2000}), None, Some(false))
        .await
        .unwrap();
    let err = watch
        .result("callers heard RUNTIME_WEDGED")
        .await
        .unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_WEDGED, "{err:?}");
    assert!(
        err.message.contains("event loop has been blocked for 2s"),
        "{err:?}"
    );
    let (u, _) = rec
        .state(ready_at, State::Unresponsive, "event loop blocked for 2s")
        .await;
    let (_, ev) = rec
        .state(u, State::Restarting, "event loop blocked for 2s")
        .await;
    assert_reaped(&ev);
    stop(&h).await;
}

/// d: a stall below the kill limit is a diagnostic only (Unresponsive, no
/// restart, calls keep pending), and a healthy pong goes back to Ready.
#[tokio::test]
async fn pong_stall_below_kill_limit_is_diagnostic_and_recovers() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.loop_stall_kill = Some(Duration::from_secs(30));
    let (h, rec, ready_at) = ready(json!({}), c).await;
    let (task, mut result) = hold(&h, None).await;
    h.call("set", json!({"stale": 1500}), None, Some(false))
        .await
        .unwrap();
    let (u, _) = rec
        .state(
            ready_at,
            State::Unresponsive,
            "event loop blocked for 1.5s (diagnostic)",
        )
        .await;
    assert!(
        timeout(Duration::from_millis(400), &mut result)
            .await
            .is_err(),
        "a diagnostic stall must not fail pending calls"
    );
    h.call("set", json!({"stale": 0}), None, Some(false))
        .await
        .unwrap();
    rec.state(u, State::Ready, "pong received").await;
    let restarts: Vec<_> = rec
        .states_after(ready_at)
        .into_iter()
        .filter(|(to, _)| matches!(to, State::Restarting | State::Starting | State::Failed))
        .collect();
    assert!(
        restarts.is_empty(),
        "a diagnostic stall restarted: {restarts:?}"
    );
    h.call(
        "finish",
        json!({"task": task, "result": "done"}),
        None,
        Some(false),
    )
    .await
    .unwrap();
    assert_eq!(result_of(result).await.unwrap(), json!("done"));
    stop(&h).await;
}

// ---- e. stream.chunk ----

/// e: each delivered chunk consumes one credit, and a chunk with zero
/// credit left is PROTOCOL_VIOLATION for every pending caller (child still
/// alive), then kill, then Crashed with the child reaped.
#[tokio::test(flavor = "current_thread")]
async fn uncredited_chunk_fails_all_then_kills_and_crashes() {
    let _children = CHILDREN.write().await;
    let mut c = cfg();
    c.initial_stream_credit = 2;
    let (h, rec, _) = ready(json!({}), c).await;
    let pid = rec.pid().await;
    let (task, items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    let ct = h
        .call_task("chunks", json!({"task": task, "n": 3}), None, Some(false))
        .unwrap();
    let watch = Watch::arm(pid, ct.result).await;
    let err = watch
        .result("callers heard PROTOCOL_VIOLATION")
        .await
        .unwrap_err();
    assert_eq!(err.code, proto::PROTOCOL_VIOLATION, "{err:?}");
    assert_eq!(
        err.message,
        "runtime sent stream.chunk with no outstanding credit"
    );
    let (seqs, end) = drain_stream(items).await;
    assert_eq!(
        seqs,
        vec![0, 1],
        "exactly the two credited chunks are delivered"
    );
    assert_eq!(end.unwrap_err().code, proto::PROTOCOL_VIOLATION);
    let (_, ev) = rec
        .state(
            0,
            State::Restarting,
            &format!("protocol violation: task {task} sent an uncredited stream.chunk"),
        )
        .await;
    assert_reaped(&ev);
    stop(&h).await;
}

/// e: a chunk for a task this generation does not know is ignored.
#[tokio::test]
async fn chunk_for_unknown_task_is_ignored() {
    let _children = CHILDREN.read().await;
    let (h, rec, ready_at) = ready(json!({}), cfg()).await;
    let ok = h
        .call(
            "chunks",
            json!({"task": 987_654_321u64, "n": 3}),
            None,
            Some(false),
        )
        .await
        .unwrap();
    assert_eq!(ok, json!(true));
    assert_eq!(
        h.call("echo", json!("still"), None, Some(false))
            .await
            .unwrap(),
        json!("still")
    );
    let changes: Vec<_> = rec
        .states_after(ready_at + 1)
        .into_iter()
        .filter(|(to, _)| *to != State::Ready)
        .collect();
    assert!(
        changes.is_empty(),
        "an unknown-task chunk changed state: {changes:?}"
    );
    stop(&h).await;
}

/// e: a stalled consumer: the stream ends with a terminal that cannot be
/// lost, the stderr note names the task, the task is removed (its slot is
/// free, cancel no longer finds it) and a best-effort cancel reaches the
/// runtime.
#[tokio::test]
async fn stalled_consumer_ends_stream_notes_removes_and_cancels() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.stream_buffer = 2;
    c.initial_stream_credit = 50;
    c.limits.max_concurrent_streams = 1;
    let (h, rec, _) = ready(json!({"log": true}), c).await;
    let (task, items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    h.call("chunks", json!({"task": task, "n": 10}), None, Some(false))
        .await
        .unwrap();
    rec.stderr(&format!(
        "task {task}: stream consumer stalled; cancelling producer"
    ))
    .await;
    rec.rx_until("the producer cancel", |rx| {
        rx.iter()
            .any(|f| f["method"] == "cancel" && f["task"] == task)
    })
    .await;
    // Pins CURRENT behaviour: the abandoned task is removed from the
    // registry without a tombstone, so a later cancel says NotFound.
    // docs/cancellation.md says a finished task reports `completed`;
    // the mismatch is listed in the PR, not changed here.
    assert_eq!(h.cancel(task, None).await.unwrap(), CancelOutcome::NotFound);
    let (_t2, _items2) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .expect("the abandoned stream still holds its slot");
    let (seqs, end) = drain_stream(items).await;
    assert_eq!(seqs, vec![0, 1]);
    let err = end.unwrap_err();
    assert_eq!(err.code, proto::INTERNAL);
    assert_eq!(err.message, "stream consumer stalled");
    stop(&h).await;
}

/// e: a vanished consumer: the stderr note, the task removed, the cancel sent.
#[tokio::test]
async fn vanished_consumer_ends_stream_notes_and_cancels() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.initial_stream_credit = 50;
    let (h, rec, _) = ready(json!({"log": true}), c).await;
    let (task, items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    drop(items);
    h.call("chunks", json!({"task": task, "n": 1}), None, Some(false))
        .await
        .unwrap();
    rec.stderr(&format!(
        "task {task}: stream consumer went away; cancelling producer"
    ))
    .await;
    rec.rx_until("the producer cancel", |rx| {
        rx.iter()
            .any(|f| f["method"] == "cancel" && f["task"] == task)
    })
    .await;
    // Pins CURRENT behaviour: the abandoned task is removed from the
    // registry without a tombstone, so a later cancel says NotFound.
    // docs/cancellation.md says a finished task reports `completed`;
    // the mismatch is listed in the PR, not changed here.
    assert_eq!(h.cancel(task, None).await.unwrap(), CancelOutcome::NotFound);
    stop(&h).await;
}

// ---- f. malformed frame, reader EOF ----

/// f: a malformed frame while serving: PROTOCOL_VIOLATION to pending
/// callers (child alive), then kill, then Crashed with the child reaped.
#[tokio::test(flavor = "current_thread")]
async fn malformed_frame_while_serving_fails_all_then_kills() {
    let _children = CHILDREN.write().await;
    let (h, rec, _) = ready(json!({}), cfg()).await;
    let pid = rec.pid().await;
    let (_task, result) = hold(&h, None).await;
    let watch = Watch::arm(pid, result).await;
    let _emit = h
        .call_task("emit", json!({"frames": ["nope"]}), None, Some(false))
        .unwrap();
    let err = watch
        .result("callers heard PROTOCOL_VIOLATION")
        .await
        .unwrap_err();
    assert_eq!(err.code, proto::PROTOCOL_VIOLATION);
    assert_eq!(err.message, "runtime sent a malformed frame");
    let (_, ev) = rec
        .state(0, State::Restarting, "protocol violation: malformed frame")
        .await;
    assert_reaped(&ev);
    stop(&h).await;
}

/// f: reader EOF: pending callers get RUNTIME_CRASHED carrying the exit
/// status, which the supervisor waits for (here 1.5s after stdout closed).
#[tokio::test]
async fn reader_eof_fails_pending_with_the_exit_status() {
    let _children = CHILDREN.read().await;
    let (h, rec, _) = ready(json!({}), cfg()).await;
    let t0 = Instant::now();
    let ct = h
        .call_task(
            "eof_then_exit",
            json!({"after": 1.5, "code": 3}),
            None,
            Some(false),
        )
        .unwrap();
    let err = result_of(ct.result).await.unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_CRASHED);
    assert!(
        err.message.starts_with("exited") && err.message.contains('3'),
        "{err:?}"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(1300),
        "{:?}",
        t0.elapsed()
    );
    rec.state(0, State::Restarting, &err.message).await;
    stop(&h).await;
}

/// f: reader EOF with no exit status within 5s: "runtime gone".
#[tokio::test]
async fn reader_eof_without_exit_status_reports_runtime_gone_after_5s() {
    let _children = CHILDREN.read().await;
    let (h, rec, _) = ready(json!({}), cfg()).await;
    let pid = rec.pid().await;
    let t0 = Instant::now();
    let ct = h
        .call_task(
            "eof_then_exit",
            json!({"after": 8, "code": 3}),
            None,
            Some(false),
        )
        .unwrap();
    let err = result_of(ct.result).await.unwrap_err();
    let waited = t0.elapsed();
    assert_eq!(err.code, proto::RUNTIME_CRASHED);
    assert_eq!(err.message, "runtime gone");
    assert!(
        waited >= Duration::from_millis(4800) && waited < Duration::from_secs(8),
        "{waited:?}"
    );
    let (_, ev) = rec.state(0, State::Restarting, "runtime gone").await;
    // Trace for the PR's "Noticed, not changed": this path returns
    // without kill_and_reap, so the child may still exist here.
    if let Ev::State { present, .. } = &ev {
        eprintln!("trace: reader-EOF Crashed reported with child pid {pid} present={present:?}");
    }
    stop(&h).await;
}

// ---- g. all handles dropped ----

/// g: dropping every handle kills and reaps the child and stops cleanly
/// (Stopped, never a restart).
#[tokio::test]
async fn dropping_every_handle_kills_the_child_and_stops_clean() {
    let _children = CHILDREN.read().await;
    let (h, rec, ready_at) = ready(json!({}), cfg()).await;
    let _pid = rec.pid().await;
    let mut srx = h.subscribe_state();
    drop(h);
    timeout(T, async {
        while *srx.borrow() != State::Stopped {
            if srx.changed().await.is_err() {
                break;
            }
        }
    })
    .await
    .expect("never stopped");
    let (_, ev) = rec.state(ready_at, State::Stopped, "shutdown").await;
    assert_reaped(&ev);
    let restarts: Vec<_> = rec
        .states_after(ready_at)
        .into_iter()
        .filter(|(to, _)| *to == State::Restarting)
        .collect();
    assert!(restarts.is_empty(), "{restarts:?}");
}

// ---- h. commands ----

/// h: a stream's initial credit reaches the runtime before its call.
#[tokio::test]
async fn stream_credit_reaches_the_wire_before_its_call() {
    let _children = CHILDREN.read().await;
    let (h, rec, _) = ready(json!({"log": true}), cfg()).await;
    let (task, _items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    let rx = rec
        .rx_until("the call", |rx| {
            rx.iter()
                .any(|f| f["method"] == "call" && f["task"] == task)
        })
        .await;
    let credit = rx
        .iter()
        .position(|f| f["method"] == "stream.credit" && f["task"] == task)
        .expect("no initial credit");
    let call = rx
        .iter()
        .position(|f| f["method"] == "call" && f["task"] == task)
        .unwrap();
    assert!(
        credit < call,
        "credit #{credit} after call #{call}: {rx:#?}"
    );
    assert_eq!(rx[credit]["n"], 8);
    stop(&h).await;
}

/// h: a stream call that fails after its credit was delivered sends the
/// cleanup cancel for that task (and the call never reaches the runtime).
#[tokio::test]
async fn failed_stream_call_after_its_credit_sends_a_cleanup_cancel() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.max_frame = 4096;
    let (h, rec, _) = ready(json!({"log": true}), c).await;
    let err = match h
        .call_stream("hold", json!({"pad": "x".repeat(8000)}), None, Some(true))
        .await
    {
        Ok(_) => panic!("an oversize stream call was accepted"),
        Err(e) => e,
    };
    assert_limit(&err, "max_frame");
    let rx = rec
        .rx_until("the cleanup cancel", |rx| {
            rx.iter().any(|f| f["method"] == "cancel")
        })
        .await;
    let credit = rx
        .iter()
        .find(|f| f["method"] == "stream.credit")
        .expect("no credit was sent")
        .clone();
    let cancel = rx.iter().find(|f| f["method"] == "cancel").unwrap();
    assert_eq!(cancel["task"], credit["task"], "{rx:#?}");
    assert!(
        !rx.iter()
            .any(|f| f["method"] == "call" && f["task"] == credit["task"]),
        "the oversize call reached the runtime: {rx:#?}"
    );
    stop(&h).await;
}

/// h: cancel answers Rejected for another webview, NotFound for an
/// unknown id, the runtime's outcome for the owner, and Completed after.
#[tokio::test]
async fn cancel_answers_rejected_notfound_cancelled_completed() {
    let _children = CHILDREN.read().await;
    let (h, _rec, _) = ready(json!({}), cfg()).await;
    let (task, result) = hold(&h, Some("main")).await;
    assert_eq!(
        h.cancel(task, Some("other".into())).await.unwrap(),
        CancelOutcome::Rejected
    );
    assert_eq!(h.cancel(task, None).await.unwrap(), CancelOutcome::Rejected);
    assert_eq!(
        h.cancel(424_242, None).await.unwrap(),
        CancelOutcome::NotFound
    );
    assert_eq!(
        h.cancel(task, Some("main".into())).await.unwrap(),
        CancelOutcome::Cancelled
    );
    assert_eq!(
        result_of(result).await.unwrap_err().code,
        proto::TASK_CANCELLED
    );
    assert_eq!(
        h.cancel(task, Some("main".into())).await.unwrap(),
        CancelOutcome::Completed
    );
    stop(&h).await;
}

/// h: GrantCredit answers: another webview, over the cap, success, an
/// unknown task, and a finished task each get their own answer.
#[tokio::test]
async fn grant_credit_answers_name_the_case() {
    let _children = CHILDREN.read().await;
    let (h, _rec, _) = ready(json!({}), cfg()).await;
    let main = || Some("main".to_string());
    let (task, items) = h
        .call_stream("hold", json!({}), main(), Some(true))
        .await
        .unwrap();

    let e = h
        .grant_credit(task, 1, Some("other".into()))
        .await
        .unwrap_err();
    assert_eq!(e.code, proto::INVALID_REQUEST);
    assert_eq!(
        e.message,
        format!("task {task} belongs to a different webview")
    );

    let e = h.grant_credit(task, 4096, main()).await.unwrap_err();
    assert_limit(&e, "max_stream_credit");
    assert!(e.message.contains("grant of 4096 exceeds"), "{e:?}");

    h.grant_credit(task, 1, main()).await.unwrap();

    let e = h.grant_credit(999_999, 1, main()).await.unwrap_err();
    assert_eq!(e.code, proto::TASK_NOT_FOUND);
    assert_eq!(e.message, "no such task in this generation: 999999");

    h.call(
        "finish",
        json!({"task": task, "result": 7}),
        None,
        Some(false),
    )
    .await
    .unwrap();
    assert_eq!(drain_stream(items).await.1.unwrap(), json!(7));
    let e = h.grant_credit(task, 1, main()).await.unwrap_err();
    assert_eq!(e.code, proto::TASK_NOT_FOUND);
    assert_eq!(e.message, format!("task {task} already finished"));
    stop(&h).await;
}

/// h: a grant whose control send fails is NOT counted: with the control
/// lane full, a grant that exactly reaches the cap from the delivered
/// credit is still a ctrl_queue error, never a max_stream_credit one.
#[tokio::test]
async fn grant_credit_counts_only_when_the_control_send_succeeds() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.initial_stream_credit = 1;
    c.limits.max_stream_credit = 200;
    let (h, _rec, _) = ready(json!({}), c).await;
    let (task, _items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    // The runtime stops reading; big data frames fill the pipe so the
    // writer blocks mid-frame and the control lane can no longer drain.
    h.call("stop_reading", json!(5.0), None, Some(false))
        .await
        .unwrap();
    let big = json!({"pad": "x".repeat(1 << 20)});
    let _flood: Vec<_> = (0..3)
        .map(|_| h.call_task("echo", big.clone(), None, Some(false)).unwrap())
        .collect();
    sleep(Duration::from_millis(300)).await;
    let mut delivered = 0u64;
    let full = loop {
        match h.grant_credit(task, 1, None).await {
            Ok(()) => {
                delivered += 1;
                assert!(delivered <= 16, "the control lane never filled");
            }
            Err(e) => break e,
        }
    };
    assert_limit(&full, "ctrl_queue");
    assert_limit(
        &h.grant_credit(task, 50, None).await.unwrap_err(),
        "ctrl_queue",
    );
    let credited = 1 + delivered;
    let probe = h
        .grant_credit(task, 200 - credited, None)
        .await
        .unwrap_err();
    assert_limit(&probe, "ctrl_queue");
    stop(&h).await;
}

/// h: an initial stream credit that cannot be delivered rejects the
/// stream BEFORE its call is committed: the call never reaches the runtime.
#[tokio::test]
async fn undeliverable_initial_credit_rejects_before_the_call_is_sent() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.initial_stream_credit = 1;
    let (h, rec, _) = ready(json!({"log": true}), c).await;
    let (task, _items) = h
        .call_stream("hold", json!({}), None, Some(true))
        .await
        .unwrap();
    h.call("stop_reading", json!(3.0), None, Some(false))
        .await
        .unwrap();
    let big = json!({"pad": "x".repeat(1 << 20)});
    let _flood: Vec<_> = (0..3)
        .map(|_| h.call_task("echo", big.clone(), None, Some(false)).unwrap())
        .collect();
    sleep(Duration::from_millis(300)).await;
    // Fill the control lane.
    let mut n = 0;
    while h.grant_credit(task, 1, None).await.is_ok() {
        n += 1;
        assert!(n <= 16, "the control lane never filled");
    }
    let err = match h
        .call_stream("probe_stream", json!({}), None, Some(true))
        .await
    {
        Ok(_) => panic!("a stream with an undeliverable credit was accepted"),
        Err(e) => e,
    };
    assert_limit(&err, "ctrl_queue");
    // Barrier behind the flood on the data lane: once it is read, anything
    // committed to the data lane before it has been read too.
    h.call("sync", json!("after"), None, Some(false))
        .await
        .unwrap();
    let rx = rec
        .rx_until("the barrier", |rx| {
            rx.iter()
                .any(|f| f["fn"] == "sync" && f["task"].as_u64() > Some(task))
        })
        .await;
    assert!(
        !rx.iter().any(|f| f["fn"] == "probe_stream"),
        "the rejected stream's call reached the runtime: {rx:#?}"
    );
    stop(&h).await;
}

/// h: a call's `stream` field is sent only when the caller declared an
/// intent; `None` (a pre-flag client) omits it.
#[tokio::test]
async fn call_stream_field_is_sent_only_when_declared() {
    let _children = CHILDREN.read().await;
    let (h, rec, _) = ready(json!({"log": true}), cfg()).await;
    h.call("echo", json!("none"), None, None).await.unwrap();
    h.call("echo", json!("false"), None, Some(false))
        .await
        .unwrap();
    let rx = rec
        .rx_until("both calls", |rx| {
            rx.iter().filter(|f| f["fn"] == "echo").count() >= 2
        })
        .await;
    let calls: Vec<&Value> = rx.iter().filter(|f| f["fn"] == "echo").collect();
    assert_eq!(calls[0]["stream"], "absent", "{rx:#?}");
    assert_eq!(calls[1]["stream"], false, "{rx:#?}");
    stop(&h).await;
}

/// h: a stream call whose credit went out but whose call frame finds the
/// data lane full: a frame_out_queue limit error, and the cleanup cancel
/// for the stranded credit.
#[tokio::test]
async fn stream_call_on_a_full_data_lane_is_a_limit_error_and_cleans_up() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.limits.max_concurrent_tasks = 512;
    c.limits.max_queued_commands = 512;
    let (h, rec, _) = ready(json!({"log": true}), c).await;
    h.call("stop_reading", json!(3.0), None, Some(false))
        .await
        .unwrap();
    let _blocker = h
        .call_task(
            "echo",
            json!({"pad": "x".repeat(1 << 20)}),
            None,
            Some(false),
        )
        .unwrap();
    sleep(Duration::from_millis(300)).await;
    // Fill the 64-frame data lane behind the blocked writer.
    let mut filler = Vec::new();
    let mut lane_full = false;
    for i in 0..100 {
        let ct = h.call_task("echo", json!(i), None, Some(false)).unwrap();
        filler.push(ct);
        if i >= 64 {
            if let Ok(Ok(Err(e))) = timeout(
                Duration::from_millis(200),
                &mut filler.last_mut().unwrap().result,
            )
            .await
            {
                assert_limit(&e, "frame_out_queue");
                lane_full = true;
                break;
            }
        }
    }
    assert!(lane_full, "the data lane never filled");
    let err = match h.call_stream("hold", json!({}), None, Some(true)).await {
        Ok(_) => panic!("a stream call on a full data lane was accepted"),
        Err(e) => e,
    };
    assert_limit(&err, "frame_out_queue");
    assert_eq!(err.message, "runtime stdin queue full — not draining");
    let rx = rec
        .rx_until("the cleanup cancel", |rx| {
            rx.iter().any(|f| f["method"] == "cancel")
        })
        .await;
    let credit = rx
        .iter()
        .find(|f| f["method"] == "stream.credit")
        .expect("no credit was sent")
        .clone();
    let cancel = rx.iter().find(|f| f["method"] == "cancel").unwrap();
    assert_eq!(cancel["task"], credit["task"], "{rx:#?}");
    stop(&h).await;
}

/// h: the task cap applies to stream calls too.
#[tokio::test]
async fn stream_call_over_the_task_cap_is_rejected() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.limits.max_concurrent_tasks = 1;
    let (h, _rec, _) = ready(json!({}), c).await;
    let ct = h.call_task("hold", json!({}), None, Some(false)).unwrap();
    let _t = ct.task.await.unwrap();
    let err = match h.call_stream("hold", json!({}), None, Some(true)).await {
        Ok(_) => panic!("a stream over the task cap was accepted"),
        Err(e) => e,
    };
    assert_limit(&err, "max_concurrent_tasks");
    assert_eq!(err.message, "too many in-flight tasks");
    stop(&h).await;
}

/// h: CancelOwned without a watermark cancels every task of that owner;
/// with a watermark only the ones allocated before it. Other owners keep
/// theirs.
#[tokio::test]
async fn cancel_owned_with_and_without_watermark() {
    let _children = CHILDREN.read().await;
    let (h, rec, _) = ready(json!({"log": true}), cfg()).await;
    let (a1, r_a1) = hold(&h, Some("A")).await;
    let (a2, r_a2) = hold(&h, Some("A")).await;
    let (b1, mut r_b1) = hold(&h, Some("B")).await;
    h.cancel_owner("A".into()).await;
    let barrier = h.call("echo", json!("barrier-1"), None, Some(false)).await;
    assert!(barrier.is_ok());
    let cancelled = |rx: &[Value]| -> Vec<u64> {
        rx.iter()
            .filter(|f| f["method"] == "cancel")
            .filter_map(|f| f["task"].as_u64())
            .collect()
    };
    let rx = rec
        .rx_until("the barrier", |rx| {
            rx.iter().filter(|f| f["fn"] == "echo").count() >= 1
        })
        .await;
    let mut got = cancelled(&rx);
    got.sort();
    assert_eq!(got, vec![a1, a2]);
    assert_eq!(
        result_of(r_a1).await.unwrap_err().code,
        proto::TASK_CANCELLED
    );
    assert_eq!(
        result_of(r_a2).await.unwrap_err().code,
        proto::TASK_CANCELLED
    );
    assert!(timeout(Duration::from_millis(200), &mut r_b1)
        .await
        .is_err());

    let (a3, r_a3) = hold(&h, Some("A")).await;
    let mark = h.task_watermark();
    let (a4, mut r_a4) = hold(&h, Some("A")).await;
    assert!(a3 < mark && a4 >= mark);
    h.cancel_owner_before("A".into(), mark).await;
    h.call("echo", json!("barrier-2"), None, Some(false))
        .await
        .unwrap();
    let rx = rec
        .rx_until("the second barrier", |rx| {
            rx.iter().filter(|f| f["fn"] == "echo").count() >= 2
        })
        .await;
    let mut got = cancelled(&rx);
    got.sort();
    assert_eq!(got, vec![a1, a2, a3], "only a3 is before the watermark");
    assert_eq!(
        result_of(r_a3).await.unwrap_err().code,
        proto::TASK_CANCELLED
    );
    assert!(timeout(Duration::from_millis(200), &mut r_a4)
        .await
        .is_err());
    assert!(!got.contains(&b1));
    stop(&h).await;
}

/// h: Retry while serving does nothing: no state change, no respawn.
#[tokio::test]
async fn retry_while_serving_is_a_no_op() {
    let _children = CHILDREN.read().await;
    let (h, rec, ready_at) = ready(json!({}), cfg()).await;
    h.retry().await.unwrap();
    sleep(Duration::from_millis(500)).await;
    let changes: Vec<_> = rec
        .states_after(ready_at + 1)
        .into_iter()
        .filter(|(to, _)| *to != State::Ready)
        .collect();
    assert!(changes.is_empty(), "{changes:?}");
    assert_eq!(rec.pid_lines(), 1);
    assert_eq!(
        h.call("echo", json!(5), None, Some(false)).await.unwrap(),
        json!(5)
    );
    stop(&h).await;
}

// ---- i. shutdown ----

/// i: grace_ms is shutdown_grace minus 1s, floored at 500ms.
#[tokio::test]
async fn shutdown_grace_ms_is_grace_minus_one_second_floored_at_500() {
    let _children = CHILDREN.read().await;
    for (grace_ms, want) in [(1000u64, 500u64), (1600, 600)] {
        let mut c = cfg();
        c.shutdown_grace = Duration::from_millis(grace_ms);
        let (h, rec, _) = ready(json!({"log": true}), c).await;
        assert_eq!(stop(&h).await, ShutdownOutcome::Terminated);
        let rx = rec
            .rx_until("the shutdown frame", |rx| {
                rx.iter().any(|f| f["method"] == "shutdown")
            })
            .await;
        let sd = rx.iter().find(|f| f["method"] == "shutdown").unwrap();
        assert_eq!(sd["grace_ms"], want, "shutdown_grace {grace_ms}ms");
    }
}

/// i: the shutdown frame is written after data committed before it; the
/// runtime's late reply is delivered before the sweep; the rest get
/// RUNTIME_UNAVAILABLE; the outcome is Terminated; the supervisor stops.
#[tokio::test]
async fn shutdown_follows_committed_calls_and_late_replies_win() {
    let _children = CHILDREN.read().await;
    let (h, rec, ready_at) = ready(json!({"log": true, "shutdown": "finish_first"}), cfg()).await;
    let first = h.call_task("hold", json!({}), None, Some(false)).unwrap();
    let second = h.call_task("hold", json!({}), None, Some(false)).unwrap();
    let outcome = stop(&h).await;
    assert_eq!(outcome, ShutdownOutcome::Terminated);
    let t1 = first.task.await.unwrap();
    assert_eq!(
        result_of(first.result).await.unwrap(),
        json!({"finished": t1})
    );
    let e = result_of(second.result).await.unwrap_err();
    assert_eq!(e.code, proto::RUNTIME_UNAVAILABLE);
    assert_eq!(e.message, "shutdown");
    let rx = rec
        .rx_until("the shutdown frame", |rx| {
            rx.iter().any(|f| f["method"] == "shutdown")
        })
        .await;
    let sd = rx.iter().position(|f| f["method"] == "shutdown").unwrap();
    let calls: Vec<usize> = rx
        .iter()
        .enumerate()
        .filter(|(_, f)| f["fn"] == "hold")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(calls.len(), 2, "{rx:#?}");
    assert!(
        calls.iter().all(|&i| i < sd),
        "shutdown overtook a committed call: {rx:#?}"
    );
    let (_, ev) = rec.state(ready_at, State::Stopped, "shutdown").await;
    assert_reaped(&ev);
}

/// i: a runtime that ignores shutdown is force-killed after the grace:
/// ForcedKill, pending callers get RUNTIME_UNAVAILABLE, Stopped.
#[tokio::test]
async fn ignored_shutdown_is_force_killed_and_pending_get_unavailable() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.shutdown_grace = Duration::from_secs(1);
    let (h, rec, ready_at) = ready(json!({"shutdown": "ignore"}), c).await;
    let (_task, result) = hold(&h, None).await;
    let t0 = Instant::now();
    assert_eq!(stop(&h).await, ShutdownOutcome::ForcedKill);
    assert!(t0.elapsed() >= Duration::from_millis(900));
    let e = result_of(result).await.unwrap_err();
    assert_eq!(e.code, proto::RUNTIME_UNAVAILABLE);
    assert_eq!(e.message, "shutdown");
    let (_, ev) = rec.state(ready_at, State::Stopped, "shutdown").await;
    assert_reaped(&ev);
}

// ---- j. liveness ----

/// j: missed_pongs_to_kill written-but-unanswered pings: Unresponsive,
/// RUNTIME_WEDGED to pending callers (child alive), kill, Crashed.
#[tokio::test(flavor = "current_thread")]
async fn missed_pongs_wedge_callers_then_kill() {
    let _children = CHILDREN.write().await;
    let mut c = cfg();
    c.missed_pongs_to_kill = 3;
    let (h, rec, ready_at) = ready(json!({}), c).await;
    let pid = rec.pid().await;
    let (_task, result) = hold(&h, None).await;
    let watch = Watch::arm(pid, result).await;
    h.call("set", json!({"pong": false}), None, Some(false))
        .await
        .unwrap();
    let e = watch
        .result("callers heard RUNTIME_WEDGED")
        .await
        .unwrap_err();
    assert_eq!(e.code, proto::RUNTIME_WEDGED);
    assert_eq!(
        e.message,
        "runtime stopped responding (possible native-code wedge)"
    );
    // The first unanswered ping already set Unresponsive ("missed 1
    // pong(s)"); set_state dedupes, so the wedge write emits no new event.
    let (u, _) = rec.state(ready_at, State::Unresponsive, "missed").await;
    let (_, ev) = rec
        .state(u, State::Restarting, "wedged: 3 missed pongs")
        .await;
    assert_reaped(&ev);
    stop(&h).await;
}

/// j: pings that never reached the wire (writer blocked on a full pipe)
/// are not misses, and the slot holds only the latest ping: after the
/// stall the runtime reads one ping, not a backlog.
#[tokio::test]
async fn pings_behind_a_full_pipe_are_not_misses_and_only_the_latest_is_sent() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.ping_interval = Duration::from_millis(100);
    c.missed_pongs_to_kill = 3;
    let (h, rec, ready_at) = ready(json!({"log": true}), c).await;
    h.call("stop_reading", json!(2.5), None, Some(false))
        .await
        .unwrap();
    let big = json!({"pad": "x".repeat(1 << 20)});
    let flood: Vec<_> = (0..3)
        .map(|_| h.call_task("echo", big.clone(), None, Some(false)).unwrap())
        .collect();
    for ct in flood {
        result_of(ct.result).await.unwrap();
    }
    let killed: Vec<_> = rec
        .states_after(ready_at)
        .into_iter()
        .filter(|(to, d)| matches!(to, State::Restarting | State::Failed) || d.contains("wedged"))
        .collect();
    assert!(
        killed.is_empty(),
        "blocked pings counted as misses: {killed:?}"
    );
    let rx = rec.rx();
    let stop_at = rx.iter().position(|f| f["fn"] == "stop_reading").unwrap();
    let echoes: Vec<usize> = rx
        .iter()
        .enumerate()
        .skip(stop_at)
        .filter(|(_, f)| f["fn"] == "echo")
        .map(|(i, _)| i)
        .collect();
    assert!(echoes.len() >= 2, "{rx:#?}");
    let pings_between = rx[echoes[0]..echoes[1]]
        .iter()
        .filter(|f| f["method"] == "ping")
        .count();
    assert!(
        pings_between <= 2,
        "{pings_between} pings queued behind the stall (slot must hold one)"
    );
    stop(&h).await;
}

// ---- k. dev restart ----

fn touch(p: &std::path::Path, k: u64) {
    std::fs::write(p, k.to_string()).unwrap();
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(std::time::SystemTime::now() + Duration::from_secs(10 * k))
        .unwrap();
}

/// k + run_loop: a changed trigger file fails pending callers with
/// RUNTIME_RESTARTED, kills, and respawns with no crash accounting (with
/// breaker_crashes = 1 a single crash would go straight to Failed).
#[tokio::test(flavor = "current_thread")]
async fn dev_restart_fails_pending_restarted_and_skips_crash_accounting() {
    let _children = CHILDREN.write().await;
    let dir = std::env::temp_dir().join(format!("pylair-rcg-trig-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let trigger = dir.join("trigger");
    touch(&trigger, 0);
    let mut c = cfg();
    c.breaker_crashes = 1;
    c.restart_trigger = Some(trigger.clone());
    let (h, rec, ready_at) = ready(json!({}), c).await;
    let pid = rec.pid().await;
    let (_task, result) = hold(&h, None).await;
    let watch = Watch::arm(pid, result).await;
    touch(&trigger, 1);
    let e = watch
        .result("callers heard RUNTIME_RESTARTED")
        .await
        .unwrap_err();
    assert_eq!(e.code, proto::RUNTIME_RESTARTED);
    assert_eq!(e.message, "python restarted (source change)");
    rec.stderr("source changed").await;
    let (r1, ev) = rec
        .state(ready_at, State::Restarting, "source changed")
        .await;
    assert_reaped(&ev);
    let (g2, _) = rec.state(r1, State::Ready, "handshake complete").await;
    touch(&trigger, 2);
    let (r2, _) = rec.state(g2, State::Restarting, "source changed").await;
    rec.state(r2, State::Ready, "handshake complete").await;
    assert!(
        !rec.states_after(0)
            .iter()
            .any(|(to, _)| *to == State::Failed),
        "a dev restart was counted as a crash"
    );
    stop(&h).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- l. child exit ----

/// l: the runtime exits on its own: responses it wrote before dying are
/// delivered to their callers, every other pending caller gets
/// RUNTIME_CRASHED, and the generation ends Crashed.
#[tokio::test]
async fn child_exit_delivers_last_words_then_fails_the_rest() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.limits.max_concurrent_tasks = 512;
    c.limits.max_queued_commands = 512;
    let (h, rec, ready_at) = ready(json!({}), c).await;
    let mut held = Vec::new();
    for _ in 0..200 {
        held.push(hold(&h, None).await);
    }
    let (_extra, extra) = hold(&h, None).await;
    let tasks: Vec<u64> = held.iter().map(|(t, _)| *t).collect();
    let die = h
        .call_task(
            "die",
            json!({"junk": 3000, "finish": tasks, "code": 3}),
            None,
            Some(false),
        )
        .unwrap();
    for (t, r) in held {
        assert_eq!(
            result_of(r).await.unwrap(),
            json!({"finished": t}),
            "task {t}"
        );
    }
    let e = result_of(extra).await.unwrap_err();
    assert_eq!(e.code, proto::RUNTIME_CRASHED);
    assert!(e.message.starts_with("exited"), "{e:?}");
    assert_eq!(
        result_of(die.result).await.unwrap_err().code,
        proto::RUNTIME_CRASHED
    );
    rec.state(ready_at, State::Restarting, "exited").await;
    stop(&h).await;
}

// ---- run_loop: how each ExitCause is treated ----

/// run_loop: Crashed waits out the restart backoff before respawning.
/// (Crash accounting: crash_loop_breaker_trips; Fatal: the version
/// mismatch test above; DevRestart: the dev restart test; CleanShutdown:
/// the shutdown tests.)
#[tokio::test]
async fn crash_waits_out_the_backoff_before_respawning() {
    let _children = CHILDREN.read().await;
    let mut c = cfg();
    c.backoff_base = Duration::from_secs(1);
    c.backoff_cap = Duration::from_secs(1);
    let (h, rec, ready_at) = ready(json!({}), c).await;
    let _die = h
        .call_task("die", json!({"finish": [], "code": 3}), None, Some(false))
        .unwrap();
    let (r, _) = rec.state(ready_at, State::Restarting, "exited").await;
    let t0 = Instant::now();
    rec.state(r, State::Starting, "spawning (gen 1)").await;
    // jitter is +/-25% of the 1s backoff
    assert!(
        t0.elapsed() >= Duration::from_millis(650),
        "respawned after {:?}, inside the backoff",
        t0.elapsed()
    );
    stop(&h).await;
}
