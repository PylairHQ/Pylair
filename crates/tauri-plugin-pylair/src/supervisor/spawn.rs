//! Spawning one runtime generation: the platform spawn path, the stdin
//! writer (control lane ahead of data, single ping slot), the stdout reader
//! and the stderr drain, wired into a `ChildIo` for the actor.

use std::io;
#[cfg(not(target_os = "linux"))]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(all(windows, debug_assertions))]
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
#[cfg(not(target_os = "linux"))]
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, oneshot};

#[cfg(windows)]
use super::config::RuntimeEvent;
use super::config::{SpawnSpec, SupervisorConfig};
#[cfg(not(windows))]
use super::platform_unix::monitor_child;
#[cfg(target_os = "linux")]
use super::platform_unix::{spawn_via_pdeathsig_anchor, PdeathAnchor};
#[cfg(windows)]
use super::platform_windows::{
    assign_to_job_object, child_creation_flags, monitor_child, resume_main_thread,
};
use super::run::{ChildIo, ChildReadErr};
use super::stderr::{StderrLines, StderrSink};
use super::wire::Ctrl;
use crate::protocol::InMsg;
use crate::transport::Transport;

// ---- process spawning & kill plumbing ----

/// Capacity of the outbound data lane (calls, the call frame of a stream).
/// It is also the `max` of a `LIMIT_EXCEEDED { limit: "frame_out_queue" }`
/// error, so the constructor and the error both read this one value.
pub(super) const FRAME_OUT_CAPACITY: usize = 64;
/// Capacity of the outbound control lane (cancel, credit, shutdown). Also
/// the `max` of `LIMIT_EXCEEDED { limit: "ctrl_queue" }`.
pub(super) const CTRL_OUT_CAPACITY: usize = 16;

pub(super) async fn spawn_child(
    spec: &SpawnSpec,
    config: &SupervisorConfig,
    events: &crate::eventbus::EventBus,
    // The dev-only inspector tap and the generation being spawned —
    // stderr lines are recorded generation-tagged before the event
    // channel ever sees them (docs/inspector.md). Inert in release.
    inspect: &crate::inspect::InspectRef,
    generation: u64,
    #[cfg(target_os = "linux")] pdeath_anchor: &mut Option<PdeathAnchor>,
) -> io::Result<ChildIo> {
    // Simplify a canonicalized (Windows \\?\-prefixed) program path so the
    // child sees a plain sys.executable — dev-mode callers may pass one too.
    // On Linux the spawn happens on a dedicated anchor thread instead —
    // see spawn_via_pdeathsig_anchor.
    #[cfg(not(target_os = "linux"))]
    let mut child = {
        let mut cmd = TokioCommand::new(dunce::simplified(&spec.program));
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        cmd.envs(spec.env.iter().cloned());
        #[cfg(windows)]
        {
            // The bundled python.exe is console-subsystem: launched from our
            // GUI-subsystem app it would pop an empty console window unless
            // CREATE_NO_WINDOW suppresses it. Orthogonal to containment —
            // the Job Object attaches by handle after spawn, and stdio
            // stays on the pipes either way.
            //
            // CREATE_SUSPENDED: the runtime must not execute one instruction
            // before it is inside the Job Object, or a process it spawns in
            // that gap is born outside the job and survives it (K2,
            // docs/design/windows-job-object-race.md). Resumed below, on
            // every path, right after assignment.
            const CREATE_SUSPENDED: u32 = 0x0000_0004;
            cmd.creation_flags(
                child_creation_flags(std::env::var_os("PYLAIR_CONSOLE").as_deref())
                    | CREATE_SUSPENDED,
            );
        }
        #[cfg(unix)]
        {
            // Own process group so killpg can reach grandchildren (§10).
            cmd.process_group(0);
        }
        cmd.spawn()?
    };
    #[cfg(target_os = "linux")]
    let mut child = spawn_via_pdeathsig_anchor(spec, pdeath_anchor)?;
    let pid = child.id();
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Test seam (debug builds only): widen the spawn-to-assignment gap so a
    // regression test can make the runtime act inside it (tests/job_race.rs).
    #[cfg(all(windows, debug_assertions))]
    if let Some(ms) = std::env::var("PYLAIR_TEST_ASSIGN_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    // Job Object handle must outlive the child — held by the monitor task.
    #[cfg(windows)]
    let job = assign_to_job_object(pid);
    #[cfg(windows)]
    if job.is_none() {
        events.emit(RuntimeEvent::Stderr(
            "pylair: could not place the runtime in a Job Object; falling back to \
             taskkill /T for cleanup (descendants may survive a hard host exit)"
                .into(),
        ));
    }
    // Let the runtime run, whether or not the job assignment worked (the
    // taskkill fallback needs a running child to be useful, and a child left
    // suspended would only end at the hello timeout). A thread that cannot
    // be resumed is a failed spawn: kill it so nothing is left suspended.
    #[cfg(windows)]
    if let Err(e) = pid
        .ok_or_else(|| io::Error::other("child exited before it could be resumed"))
        .and_then(resume_main_thread)
    {
        let _ = child.start_kill();
        return Err(io::Error::other(format!(
            "could not start the Python runtime (resuming its main thread failed: {e})"
        )));
    }
    #[cfg(windows)]
    let tree_pid = pid;

    // monitor: owns the child, awaits exit or kill signal
    let (exit_tx, exit_rx) = oneshot::channel();
    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
    #[cfg(windows)]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx, job, tree_pid));
    #[cfg(not(windows))]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx));

    // writer: frames -> child stdin. Two lanes: control (cancel/credit/
    // shutdown, small and always drained first) and data (calls, initial
    // credit). A stream flood can fill the data lane; it cannot starve or
    // silently drop a control frame (RC.6B).
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_OUT_CAPACITY);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(CTRL_OUT_CAPACITY);
    let ping_slot = Arc::new(Mutex::new(None::<Vec<u8>>));
    let ping_notify = Arc::new(tokio::sync::Notify::new());
    let pings_sent = Arc::new(AtomicU64::new(0));
    let writer_ping_slot = ping_slot.clone();
    let writer_ping_notify = ping_notify.clone();
    let writer_pings_sent = pings_sent.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut w = stdin;
        loop {
            // Latest ping first: take() so only the freshest frame the
            // actor committed goes on the wire - and count the write,
            // because only a ping the runtime could have answered may
            // count as a miss.
            let pending_ping = writer_ping_slot.lock().unwrap().take();
            if let Some(b) = pending_ping {
                if w.write_all(&b).await.is_err() || w.flush().await.is_err() {
                    return; // child stdin gone
                }
                writer_pings_sent.fetch_add(1, Ordering::Relaxed);
            }
            let body: Vec<u8> = tokio::select! {
                biased;
                // A ping arrived while both lanes were quiet - loop back
                // to the slot drain above instead of sleeping through it.
                _ = writer_ping_notify.notified() => continue,
                // Ctrl always wins when both lanes have a frame; a closed
                // ctrl channel falls through to the data lane.
                c = ctrl_rx.recv() => match c {
                    Some(Ctrl::Now(b)) => b,
                    Some(Ctrl::FlushDataThen(b)) => {
                        // Shutdown ordering: write every data frame that
                        // was committed before this control frame — a call
                        // accepted before shutdown must reach the runtime
                        // or its caller hears 'unavailable' instead of the
                        // drain's real terminal.
                        while let Ok(d) = frame_rx.try_recv() {
                            if w.write_all(&d).await.is_err() || w.flush().await.is_err() {
                                return; // child stdin gone
                            }
                        }
                        b
                    }
                    None => match frame_rx.recv().await {
                        Some(b) => b,
                        None => return,
                    },
                },
                d = frame_rx.recv() => match d {
                    Some(b) => b,
                    None => match ctrl_rx.recv().await {
                        Some(Ctrl::Now(b)) => b,
                        Some(Ctrl::FlushDataThen(b)) => b, // data lane already empty
                        None => return,
                    },
                },
            };
            // The frame arrived with its length prefix embedded, so one
            // write_all puts the whole frame on the pipe — the old
            // header+body pair cost an extra unbuffered write per frame
            // (docs/performance.md).
            if w.write_all(&body).await.is_err() || w.flush().await.is_err() {
                return; // child stdin gone
            }
        }
    });

    // reader: child stdout -> parsed messages
    let (msg_tx, msg_rx) = mpsc::channel::<Result<InMsg, ChildReadErr>>(64);
    let max_frame = config.max_frame;
    tokio::spawn(async move {
        let mut t = ReaderOnly {
            inner: stdout,
            max_frame,
        };
        loop {
            match t.read_frame().await {
                Ok(Some(body)) => {
                    let parsed = InMsg::parse(&body).map_err(ChildReadErr::Malformed);
                    if msg_tx.send(parsed).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return, // EOF
                Err(e) => {
                    let _ = msg_tx.send(Err(ChildReadErr::Io(e))).await;
                    return;
                }
            }
        }
    });

    // stderr: always drained, forwarded as events (protocol §9). Raw
    // bytes + lossy UTF-8: an invalid byte must never end the drain —
    // a full pipe deadlocks the child. The sink between drain and the
    // bounded event channel owns the documented drop-oldest policy.
    let ev = events.clone();
    let sink = Arc::new(StderrSink::new());
    {
        let sink = sink.clone();
        // The inspector tap records lines BEFORE the bounded channel
        // policy can drop them — the timeline's own ring owns its
        // lossiness instead of inheriting the channel's. One atomic
        // load per line when the flag is off, nothing in release.
        let inspect = inspect.clone();
        tokio::spawn(async move {
            let mut rdr = BufReader::with_capacity(16 * 1024, stderr);
            let mut lines = StderrLines::new();
            loop {
                match rdr.fill_buf().await {
                    Ok([]) => break, // EOF — child stderr closed
                    Ok(b) => {
                        let n = b.len();
                        for line in lines.feed(b) {
                            inspect.note_stderr(generation, &line);
                            sink.push(line);
                        }
                        rdr.consume(n);
                    }
                    // A real I/O error (not bad UTF-8, which can't get
                    // here — we never decode through lines()).
                    Err(_) => break,
                }
            }
            if let Some(last) = lines.finish() {
                inspect.note_stderr(generation, &last);
                sink.push(last);
            }
            sink.finish();
        });
    }
    tokio::spawn(async move { sink.forward(ev).await });

    Ok(ChildIo {
        frame_out: frame_tx,
        ctrl_out: ctrl_tx,
        ping_slot,
        ping_notify,
        pings_sent,
        msg_in: msg_rx,
        exited: exit_rx,
        kill: kill_tx,
        pid,
    })
}

/// stdout reader without a writer — the reader task only consumes.
struct ReaderOnly<R> {
    inner: R,
    max_frame: usize,
}

#[async_trait::async_trait]
impl<R: tokio::io::AsyncRead + Unpin + Send> Transport for ReaderOnly<R> {
    async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        use tokio::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_be_bytes(len_buf) as usize;
        if n > self.max_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                crate::transport::FrameTooLarge(n),
            ));
        }
        let mut body = vec![0u8; n];
        self.inner.read_exact(&mut body).await?;
        Ok(Some(body))
    }
    async fn write_frame(&mut self, _body: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "reader only"))
    }
}
