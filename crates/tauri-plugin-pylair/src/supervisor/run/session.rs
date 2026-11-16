//! The steady-state half of `run_child`: `Session` holds the per-generation
//! bookkeeping and the methods the `select!` arms call.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::RecvError;
use tokio::time::timeout;

use super::super::config::{RuntimeEvent, State};
use super::super::handle::{limit_err, Command, ShutdownOutcome};
use super::super::stream::ChunkFail;
use super::super::wire::{cancel_outcome, mtime, send_ctrl};
use super::{
    classify_pong, drain_exited_inbox, fail_all, kill_and_reap, ChildIo, ChildReadErr, ExitCause,
    PendingKind, PongVerdict, Supervisor, TaskMeta,
};
use crate::protocol::{self as proto, ErrorObj, InMsg};

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

    pub(super) async fn on_frame(
        &mut self,
        msg: Option<Result<InMsg, ChildReadErr>>,
    ) -> ControlFlow<ExitCause> {
        match msg {
            Some(Ok(InMsg::Response { id, result, error })) => {
                let out = match (result, error) {
                    (_, Some(e)) => Err(e),
                    (r, None) => Ok(r.unwrap_or(Value::Null)),
                };
                if let Some(kind) = self.pending.remove(&id) {
                    match kind {
                        PendingKind::Call(tx) => {
                            let _ = tx.send(out);
                        }
                        PendingKind::Stream(sink) => sink.finish(out),
                        PendingKind::Cancel { respond } => {
                            let _ = respond.send(cancel_outcome(&out));
                        }
                    }
                }
                if let Some(task) = self.task_of_call.remove(&id) {
                    self.tasks.remove(&task);
                    if self.done_tasks.len() >= 1024 {
                        self.done_tasks.clear();
                    }
                    self.done_tasks.insert(task);
                }
            }
            Some(Ok(InMsg::Notification { method, params })) => {
                let p = params.unwrap_or(Value::Null);
                match method.as_str() {
                    "pong" => {
                        self.pongs_received += 1;
                        // Transport is alive. Is the loop SERVING?
                        let stale = Duration::from_millis(p["loop_stale_ms"].as_u64().unwrap_or(0));
                        match classify_pong(stale, self.sup.config.loop_stall_kill) {
                            PongVerdict::StallKill => {
                                self.sup.set_state(
                                    State::Unresponsive,
                                    format!("event loop blocked for {stale:?}"),
                                );
                                fail_all(
                                    &mut self.pending,
                                    proto::RUNTIME_WEDGED,
                                    &format!(
                                        "the runtime's event loop has been blocked for {stale:?} \
                                                          (blocking code inside an async def?)"
                                    ),
                                )
                                .await;
                                kill_and_reap(&mut self.io).await;
                                return ControlFlow::Break(ExitCause::Crashed(format!(
                                    "event loop blocked for {stale:?}"
                                )));
                            }
                            // Diagnostic only (P1-7): the loop
                            // cannot serve calls right now, but a
                            // long stall is not proof of a wedge.
                            PongVerdict::StallWarn => self.sup.set_state(
                                State::Unresponsive,
                                format!("event loop blocked for {stale:?} (diagnostic)"),
                            ),
                            PongVerdict::Ready => self.sup.set_state(State::Ready, "pong received"),
                        }
                    }
                    "stream.chunk" => {
                        let task = p["task"].as_u64().unwrap_or(0);
                        let seq = p["seq"].as_u64().unwrap_or(0);
                        let value = p["value"].clone();
                        // A chunk with no outstanding credit is
                        // a protocol violation (protocol-v1 §3.1):
                        // a runtime that cannot count credit
                        // cannot be trusted to honour a per-task
                        // abort either — same treatment as a
                        // malformed frame. Chunks for a task the
                        // generation no longer knows are still
                        // ignored: they are a plausible race with
                        // the producer draining after a finish.
                        if self.tasks.get(&task).is_some_and(|m| m.credit == 0) {
                            fail_all(
                                &mut self.pending,
                                proto::PROTOCOL_VIOLATION,
                                "runtime sent stream.chunk with no outstanding credit",
                            )
                            .await;
                            kill_and_reap(&mut self.io).await;
                            return ControlFlow::Break(ExitCause::Crashed(format!(
                                "protocol violation: task {task} sent an uncredited stream.chunk"
                            )));
                        }
                        let mut abandoned = None;
                        if let Some(meta) = self.tasks.get_mut(&task) {
                            // Each delivered chunk consumed one
                            // credit (RC.8A accounting).
                            meta.credit -= 1;
                            if let Some(PendingKind::Stream(sink)) = self.pending.get(&meta.call_id)
                            {
                                if let Err(why) = sink.chunk(seq, value) {
                                    abandoned = Some((meta.call_id, why));
                                }
                            }
                        }
                        if let Some((call_id, why)) = abandoned {
                            // The consumer stalled or vanished, or a
                            // byte cap tripped. End the stream with a
                            // terminal that cannot be lost, and STOP
                            // the producer instead of leaving it
                            // running unobserved.
                            let (err, note) = match why {
                                ChunkFail::Stalled => (
                                    ErrorObj {
                                        code: proto::INTERNAL,
                                        message: "stream consumer stalled".into(),
                                        data: None,
                                    },
                                    "stalled",
                                ),
                                ChunkFail::Gone => (
                                    ErrorObj {
                                        code: proto::INTERNAL,
                                        message: "stream consumer went away".into(),
                                        data: None,
                                    },
                                    "went away",
                                ),
                                ChunkFail::StreamBytes => (
                                    limit_err(
                                        "stream_buffer_bytes",
                                        self.sup.config.limits.stream_buffer_bytes as u64,
                                        "stream consumer buffered past the per-stream byte cap",
                                    ),
                                    "over the byte cap",
                                ),
                                ChunkFail::TotalBytes => (
                                    limit_err(
                                        "stream_buffer_bytes_total",
                                        self.sup.config.limits.stream_buffer_bytes_total as u64,
                                        "all streams exceeded the shared byte cap",
                                    ),
                                    "over the shared byte cap",
                                ),
                            };
                            let _ = self.sup.events.try_send(RuntimeEvent::Stderr(format!(
                                "task {task}: stream consumer {note}; cancelling producer"
                            )));
                            if let Some(PendingKind::Stream(sink)) = self.pending.remove(&call_id) {
                                sink.finish(Err(err));
                            }
                            self.task_of_call.remove(&call_id);
                            self.tasks.remove(&task);
                            let cid = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
                            let (ack, _) = oneshot::channel();
                            if send_ctrl(
                                &self.io,
                                &proto::request(cid, "cancel", serde_json::json!({"task": task})),
                                self.sup.config.max_frame,
                            ) {
                                self.pending
                                    .insert(cid, PendingKind::Cancel { respond: ack });
                            }
                        }
                    }
                    _ => {} // unknown notifications are ignored (spec §8)
                }
            }
            Some(Err(e)) => {
                fail_all(
                    &mut self.pending,
                    proto::PROTOCOL_VIOLATION,
                    "runtime sent a malformed frame",
                )
                .await;
                kill_and_reap(&mut self.io).await;
                return ControlFlow::Break(ExitCause::Crashed(format!("protocol violation: {e}")));
            }
            None => {
                // reader hit EOF: runtime is gone; monitor should confirm
                let detail = match timeout(Duration::from_secs(5), &mut self.io.exited).await {
                    Ok(Ok(Ok(status))) => format!("exited {status}"),
                    _ => "runtime gone".into(),
                };
                fail_all(&mut self.pending, proto::RUNTIME_CRASHED, &detail).await;
                return ControlFlow::Break(ExitCause::Crashed(detail));
            }
        }
        ControlFlow::Continue(())
    }

    pub(super) async fn on_command(&mut self, cmd: Option<Command>) -> ControlFlow<ExitCause> {
        let Some(cmd) = cmd else {
            // all handles dropped — shut down
            let outcome = if kill_and_reap(&mut self.io).await {
                ShutdownOutcome::ForcedKill
            } else {
                ShutdownOutcome::Incomplete
            };
            self.sup.shutdown_outcome = Some(outcome);
            return ControlFlow::Break(ExitCause::CleanShutdown);
        };
        match cmd {
            Command::Call {
                fn_name,
                input,
                stream,
                respond,
                task_out,
                owner,
            } => self.call(fn_name, input, stream, respond, task_out, owner),
            Command::CallStream {
                fn_name,
                input,
                stream,
                items,
                task_out,
                owner,
            } => self.call_stream(fn_name, input, stream, items, task_out, owner),
            Command::Cancel {
                task,
                respond,
                caller,
            } => {
                self.cancel(task, respond, caller);
                ControlFlow::Continue(())
            }
            Command::GrantCredit {
                task,
                n,
                respond,
                caller,
            } => {
                self.grant_credit(task, n, respond, caller);
                ControlFlow::Continue(())
            }
            Command::CancelOwned { owner, before } => {
                self.cancel_owned(owner, before);
                ControlFlow::Continue(())
            }
            Command::Retry => {
                // Only meaningful from Failed — already serving.
                ControlFlow::Continue(())
            }
            Command::Shutdown { ack } => self.shutdown(ack).await,
        }
    }
}
