//! The steady-state half of `run_child`: `Session` holds the per-generation
//! bookkeeping and the methods the `select!` arms call.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use tokio::sync::oneshot::error::RecvError;

use super::super::config::{RuntimeEvent, State};
use super::super::wire::mtime;
use super::{
    drain_exited_inbox, fail_all, kill_and_reap, ChildIo, ExitCause, PendingKind, Supervisor,
    TaskMeta,
};
use crate::protocol as proto;

/// The steady-state bookkeeping of one runtime generation (everything
/// `run_child` used to keep in locals), plus the supervisor and child IO it
/// operates on. `run_child` owns the `select!` loop; every arm hands its
/// result to a method here.
///
/// Field order is the drop order the locals had: `pending` before `io`.
pub(super) struct Session<'a> {
    pub(super) sup: &'a mut Supervisor,
    /// Recently-finished task ids — lets cancel report Completed
    /// instead of NotFound. Bounded; cleared on overflow.
    pub(super) done_tasks: HashSet<u64>,
    /// Task registry {task -> call_id, owner webview} — the ownership
    /// check for cancel/credit happens here, and the map is per-session
    /// so a stale id from a previous generation resolves to NotFound.
    pub(super) tasks: HashMap<u64, TaskMeta>,
    /// call_id -> task
    pub(super) task_of_call: HashMap<u64, u64>,
    pub(super) pending: HashMap<u64, PendingKind>,
    pub(super) io: ChildIo,
    pub(super) pongs_received: u64,
    pub(super) ping_seq: u64,
}

impl<'a> Session<'a> {
    pub(super) fn new(sup: &'a mut Supervisor, io: ChildIo) -> Self {
        Session {
            sup,
            done_tasks: HashSet::new(),
            tasks: HashMap::new(),
            task_of_call: HashMap::new(),
            pending: HashMap::new(),
            io,
            pongs_received: 0,
            ping_seq: 0,
        }
    }
}

impl Session<'_> {
    pub(super) async fn on_ping_tick(&mut self) -> ControlFlow<ExitCause> {
        // Misses count only pings the writer actually put on
        // the wire: a ping still in the slot behind a full
        // pipe is backpressure, not evidence the runtime died.
        let pongs_outstanding = self
            .io
            .pings_sent
            .load(Ordering::Relaxed)
            .saturating_sub(self.pongs_received) as u32;
        if pongs_outstanding >= self.sup.config.missed_pongs_to_kill {
            self.sup.set_state(
                State::Unresponsive,
                format!("{} missed pongs — runtime wedged", pongs_outstanding),
            );
            fail_all(
                &mut self.pending,
                proto::RUNTIME_WEDGED,
                "runtime stopped responding (possible native-code wedge)",
            )
            .await;
            kill_and_reap(&mut self.io).await;
            return ControlFlow::Break(ExitCause::Crashed(format!(
                "wedged: {} missed pongs",
                pongs_outstanding
            )));
        }
        if pongs_outstanding > 0 {
            // A ping went a full interval unanswered — a real miss.
            self.sup.set_state(
                State::Unresponsive,
                format!("missed {} pong(s)", pongs_outstanding),
            );
        }
        self.ping_seq += 1;
        let body = proto::request(
            self.sup.next_id.fetch_add(1, Ordering::Relaxed),
            "ping",
            serde_json::json!({"seq": self.ping_seq}),
        );
        // Liveness gets its own slot the writer drains ahead
        // of data, never the shared control lane - a stalled
        // writer holds at most one pending ping, so liveness
        // cannot starve cancel/credit either.
        if let Ok(bytes) = serde_json::to_vec(&body) {
            if bytes.len() <= self.sup.config.max_frame {
                *self.io.ping_slot.lock().unwrap() = Some(bytes);
                self.io.ping_notify.notify_one();
            }
        }
        ControlFlow::Continue(())
    }

    pub(super) async fn on_trigger_tick(
        &mut self,
        restart_trigger: &Option<PathBuf>,
        trigger_mtime: Option<SystemTime>,
    ) -> ControlFlow<ExitCause> {
        let m = restart_trigger.as_deref().and_then(mtime);
        if m != trigger_mtime {
            let _ = self.sup.events.try_send(RuntimeEvent::Stderr(
                "pylair: source changed — restarting python runtime".into(),
            ));
            fail_all(
                &mut self.pending,
                proto::RUNTIME_RESTARTED,
                "python restarted (source change)",
            )
            .await;
            kill_and_reap(&mut self.io).await;
            return ControlFlow::Break(ExitCause::DevRestart);
        }
        ControlFlow::Continue(())
    }

    pub(super) async fn on_child_exit(
        &mut self,
        status: Result<io::Result<ExitStatus>, RecvError>,
    ) -> ControlFlow<ExitCause> {
        let detail = match status {
            Ok(Ok(s)) => format!("exited {s}"),
            Ok(Err(e)) => format!("wait error: {e}"),
            Err(_) => "monitor dropped".into(),
        };
        // Same last-words delivery as the shutdown path: a dying
        // runtime's queued responses resolve their own calls.
        drain_exited_inbox(
            &mut self.io,
            &mut self.pending,
            &mut self.tasks,
            &mut self.task_of_call,
            &mut self.done_tasks,
        )
        .await;
        fail_all(&mut self.pending, proto::RUNTIME_CRASHED, &detail).await;
        ControlFlow::Break(ExitCause::Crashed(detail))
    }
}
