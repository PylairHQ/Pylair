//! Non-Windows process containment: the Linux parent-death anchor thread,
//! process-group kill and reaping, and the non-Windows process monitor.
//! The module is compiled for every non-Windows target; each item keeps
//! its own narrower cfg (unix, or target_os = "linux").

use std::io;
#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Child;
#[cfg(target_os = "linux")]
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, oneshot};

#[cfg(target_os = "linux")]
use super::config::SpawnSpec;

/// Linux: spawn the child FROM a dedicated long-lived thread that parks
/// for the host process's lifetime. PR_SET_PDEATHSIG is bound to the
/// *thread* that forked the child — not the process — so spawning on a
/// transient executor thread (e.g. a spawn_blocking worker at its ~10s
/// keepalive) would SIGKILL a perfectly healthy runtime. The anchor can
/// only die with the whole process, so the signal means exactly "the
/// host is gone".
///
/// One anchor serves every spawn a supervisor ever makes: the pre-rc.15
/// shape parked a fresh thread per spawn, so crash/retry respawns stacked
/// anchor threads (~8 MiB of reserved stack each) that never unwound.
/// The fork itself still happens on the anchor thread — the runtime
/// handle and the fully-configured `Command` are carried over a job
/// channel and executed there.
#[cfg(target_os = "linux")]
type AnchorJob = (
    tokio::runtime::Handle,
    tokio::process::Command,
    std::sync::mpsc::Sender<io::Result<Child>>,
);

#[cfg(target_os = "linux")]
pub(super) struct PdeathAnchor(std::sync::mpsc::Sender<AnchorJob>);

/// Live anchor threads — test-only visibility that respawns reuse one
/// anchor instead of stacking them.
#[cfg(target_os = "linux")]
static ANCHOR_THREADS: AtomicU64 = AtomicU64::new(0);

/// Live `pylair-pdeath-anchor` threads in this process (test hook).
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn pdeath_anchor_threads() -> usize {
    ANCHOR_THREADS.load(Ordering::SeqCst) as usize
}

#[cfg(target_os = "linux")]
impl PdeathAnchor {
    fn new() -> io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<AnchorJob>();
        std::thread::Builder::new()
            .name("pylair-pdeath-anchor".into())
            .spawn(move || {
                ANCHOR_THREADS.fetch_add(1, Ordering::SeqCst);
                // Jobs until every sender is gone — a sender lives in the
                // supervisor actor, so the anchor outlives every spawn it
                // served and exits when the supervisor does.
                while let Ok((rt, mut cmd, reply)) = rx.recv() {
                    let res = {
                        let _guard = rt.enter();
                        cmd.spawn()
                    };
                    let _ = reply.send(res);
                }
                ANCHOR_THREADS.fetch_sub(1, Ordering::SeqCst);
            })
            .map(|_| Self(tx))
    }
}

#[cfg(target_os = "linux")]
pub(super) fn spawn_via_pdeathsig_anchor(
    spec: &SpawnSpec,
    anchor: &mut Option<PdeathAnchor>,
) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|e| io::Error::other(format!("no runtime for spawn: {e}")))?;
    let mut cmd = TokioCommand::new(dunce::simplified(&spec.program));
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    cmd.envs(spec.env.iter().cloned());
    // The classic fork/prctl race: if the whole process dies between the
    // fork and the child's PR_SET_PDEATHSIG, the child is reparented before
    // the flag exists and the signal would never fire — a permanent orphan.
    // Mirror the isolated worker's check: verify getppid() still names the
    // process we forked from; if it changed, abort the exec and fail spawn.
    let expected_ppid = std::process::id() as libc::pid_t;
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != expected_ppid {
                return Err(io::Error::other("reparented before pdeathsig — aborting"));
            }
            Ok(())
        });
    }
    let anchor = match anchor {
        Some(a) => a,
        None => anchor.insert(PdeathAnchor::new()?),
    };
    let (tx, rx) = std::sync::mpsc::channel::<io::Result<Child>>();
    anchor
        .0
        .send((rt, cmd, tx))
        .map_err(|_| io::Error::other("pdeathsig anchor is gone"))?;
    rx.recv()
        .map_err(|_| io::Error::other("pdeathsig anchor died before spawn"))?
}

#[cfg(not(windows))]
pub(super) async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
) {
    #[cfg(unix)]
    let pgid = child.id();
    tokio::select! {
        status = child.wait() => {
            // The leader exiting on its own must not leave group members
            // (workers, user subprocesses) running with no supervisor.
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                reap_group(pgid);
            }
            let _ = exit_tx.send(status);
        }
        _ = kill_rx.recv() => {
            kill_tree(&mut child).await;
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// SIGKILL every remaining member of the process group `pgid` we created.
/// ESRCH (already empty) and EPERM (group id recycled by an unrelated
/// process) are both "nothing of ours left to kill".
#[cfg(unix)]
fn reap_group(pgid: u32) {
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

/// Kill the child process tree: killpg on unix (own process group, protocol
/// §10); other targets fall back to plain child kill. On Windows the Job
/// Object reaps the tree when its handle closes (see monitor_child).
#[cfg(not(windows))]
async fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            // negative pid = process group we created via process_group(0)
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}
