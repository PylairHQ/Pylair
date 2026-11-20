//! The `Command` handlers of `Session::on_command`: one method per command
//! (call, call_stream, cancel, grant_credit, cancel_owned, shutdown).

use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::super::handle::{limit_err, CancelOutcome, ShutdownOutcome};
use super::super::spawn::{CTRL_OUT_CAPACITY, FRAME_OUT_CAPACITY};
use super::super::stream::StreamSink;
use super::super::wire::{owner_allows, send_ctrl, send_ctrl_flushed, send_frame, SendFail};
use super::session::Session;
use super::{
    drain_exited_inbox, fail_all, kill_and_reap, reject_stream, ExitCause, PendingKind, TaskMeta,
};
use crate::protocol::{self as proto, ErrorObj};

impl Session<'_> {
    pub(super) fn call(
        &mut self,
        fn_name: String,
        input: Value,
        stream: Option<bool>,
        respond: oneshot::Sender<Result<Value, ErrorObj>>,
        task_out: oneshot::Sender<u64>,
        owner: Option<String>,
    ) -> ControlFlow<ExitCause> {
        let lim = &self.sup.config.limits;
        if self.tasks.len() >= lim.max_concurrent_tasks {
            let _ = respond.send(Err(limit_err(
                "max_concurrent_tasks",
                lim.max_concurrent_tasks as u64,
                "too many in-flight tasks",
            )));
            return ControlFlow::Continue(());
        }
        let id = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
        let task = self.sup.next_task.fetch_add(1, Ordering::Relaxed);
        let mut params = serde_json::json!({"fn": fn_name, "task": task,
                                "input": input});
        if let Some(s) = stream {
            params["stream"] = serde_json::json!(s);
        }
        let body = proto::request(id, "call", params);
        match send_frame(&self.io, &body, self.sup.config.max_frame) {
            Ok(()) => {
                let _ = task_out.send(task);
                self.pending.insert(id, PendingKind::Call(respond));
                self.task_of_call.insert(id, task);
                self.sup.fair.lock().unwrap().note_started(&owner);
                self.tasks.insert(
                    task,
                    TaskMeta {
                        call_id: id,
                        owner,
                        is_stream: false,
                        credit: 0,
                    },
                );
            }
            Err(SendFail::Oversize(n)) => {
                let _ = respond.send(Err(limit_err(
                    "max_frame",
                    self.sup.config.max_frame as u64,
                    format!("call frame is {n} bytes — the runtime would reject it"),
                )));
            }
            Err(SendFail::LaneFull) => {
                let _ = respond.send(Err(limit_err(
                    "frame_out_queue",
                    FRAME_OUT_CAPACITY as u64,
                    "runtime stdin queue full — not draining",
                )));
            }
        }
        ControlFlow::Continue(())
    }

    pub(super) fn call_stream(
        &mut self,
        fn_name: String,
        input: Value,
        stream: Option<bool>,
        items: StreamSink,
        task_out: oneshot::Sender<Result<u64, ErrorObj>>,
        owner: Option<String>,
    ) -> ControlFlow<ExitCause> {
        // `None` = a pre-flag client: it could be
        // either API, so the conservative accounting
        // is "stream" — a slot is taken and the
        // initial credit is granted exactly as before,
        // but the wire field stays absent so the
        // runtime applies no mismatch validation.
        let is_stream = stream.unwrap_or(true);
        let lim = &self.sup.config.limits;
        if self.tasks.len() >= lim.max_concurrent_tasks {
            reject_stream(
                items,
                task_out,
                limit_err(
                    "max_concurrent_tasks",
                    lim.max_concurrent_tasks as u64,
                    "too many in-flight tasks",
                ),
            );
            return ControlFlow::Continue(());
        }
        if is_stream {
            let streams = self.tasks.values().filter(|m| m.is_stream).count();
            if streams >= lim.max_concurrent_streams {
                reject_stream(
                    items,
                    task_out,
                    limit_err(
                        "max_concurrent_streams",
                        lim.max_concurrent_streams as u64,
                        "too many in-flight streams",
                    ),
                );
                return ControlFlow::Continue(());
            }
        }
        let id = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
        let task = self.sup.next_task.fetch_add(1, Ordering::Relaxed);
        let mut params = serde_json::json!({"fn": fn_name, "task": task,
                                "input": input});
        if let Some(s) = stream {
            params["stream"] = serde_json::json!(s);
        }
        let body = proto::request(id, "call", params);
        if is_stream {
            let credit = proto::notification(
                "stream.credit",
                serde_json::json!({"task": task, "n": self.sup.config.initial_stream_credit}),
            );
            // The initial grant is control traffic and goes
            // FIRST: the runtime buffers credit for a task it
            // has not registered yet (pending credit), so the
            // grant cannot be starved or reordered behind
            // data-lane traffic — and a failed send fails the
            // stream BEFORE the call is committed instead of
            // leaving it uncredited forever.
            if !send_ctrl(&self.io, &credit, self.sup.config.max_frame) {
                reject_stream(
                    items,
                    task_out,
                    limit_err(
                        "ctrl_queue",
                        CTRL_OUT_CAPACITY as u64,
                        "control queue full — credit not delivered",
                    ),
                );
                return ControlFlow::Continue(());
            }
        }
        match send_frame(&self.io, &body, self.sup.config.max_frame) {
            Ok(()) => {
                let _ = task_out.send(Ok(task));
                self.pending.insert(id, PendingKind::Stream(items));
                self.task_of_call.insert(id, task);
                self.sup.fair.lock().unwrap().note_started(&owner);
                self.tasks.insert(
                    task,
                    TaskMeta {
                        call_id: id,
                        owner,
                        is_stream,
                        credit: if is_stream {
                            self.sup.config.initial_stream_credit
                        } else {
                            0
                        },
                    },
                );
            }
            Err(e) => {
                // The grant was delivered but the call was
                // not — best-effort cancel drops the
                // stranded pending credit (bounded anyway).
                let cid = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
                let _ = send_ctrl(
                    &self.io,
                    &proto::request(cid, "cancel", serde_json::json!({"task": task})),
                    self.sup.config.max_frame,
                );
                let max = self.sup.config.max_frame as u64;
                reject_stream(
                    items,
                    task_out,
                    match e {
                        SendFail::Oversize(n) => limit_err(
                            "max_frame",
                            max,
                            format!("call frame is {n} bytes — the runtime would reject it"),
                        ),
                        SendFail::LaneFull => limit_err(
                            "frame_out_queue",
                            FRAME_OUT_CAPACITY as u64,
                            "runtime stdin queue full — not draining",
                        ),
                    },
                );
            }
        }
        ControlFlow::Continue(())
    }

    pub(super) fn cancel(
        &mut self,
        task: u64,
        respond: oneshot::Sender<CancelOutcome>,
        caller: Option<String>,
    ) {
        match self.tasks.get(&task) {
            // Unknown to this generation: a completed
            // tombstone means the cancel raced a finish;
            // anything else is a stale or foreign id.
            None => {
                let _ = respond.send(if self.done_tasks.contains(&task) {
                    CancelOutcome::Completed
                } else {
                    CancelOutcome::NotFound
                });
            }
            Some(meta) if !owner_allows(&meta.owner, &caller) => {
                let _ = respond.send(CancelOutcome::Rejected);
            }
            Some(_) => {
                let id = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
                let body = proto::request(id, "cancel", serde_json::json!({"task": task}));
                // Control lane: a data flood cannot starve
                // this, and a failed try_send is a loud
                // outcome — never a silent drop.
                if send_ctrl(&self.io, &body, self.sup.config.max_frame) {
                    self.pending.insert(id, PendingKind::Cancel { respond });
                } else {
                    let _ = respond.send(CancelOutcome::DeliveryFailed);
                }
            }
        }
    }

    pub(super) fn grant_credit(
        &mut self,
        task: u64,
        n: u64,
        respond: oneshot::Sender<Result<(), ErrorObj>>,
        caller: Option<String>,
    ) {
        match self.tasks.get_mut(&task) {
            None => {
                let _ = respond.send(Err(ErrorObj {
                    code: proto::TASK_NOT_FOUND,
                    message: if self.done_tasks.contains(&task) {
                        format!("task {task} already finished")
                    } else {
                        format!("no such task in this generation: {task}")
                    },
                    data: None,
                }));
            }
            Some(meta) if !owner_allows(&meta.owner, &caller) => {
                let _ = respond.send(Err(ErrorObj {
                    code: proto::INVALID_REQUEST,
                    message: format!("task {task} belongs to a different webview"),
                    data: None,
                }));
            }
            Some(meta) => {
                let lim = &self.sup.config.limits;
                let would = meta.credit.checked_add(n);
                match would {
                    Some(total) if total <= lim.max_stream_credit => {
                        let body = proto::notification(
                            "stream.credit",
                            serde_json::json!({"task": task, "n": n}),
                        );
                        let _ = respond.send(
                            if send_ctrl(&self.io, &body, self.sup.config.max_frame) {
                                meta.credit = total;
                                Ok(())
                            } else {
                                Err(limit_err(
                                    "ctrl_queue",
                                    CTRL_OUT_CAPACITY as u64,
                                    "control queue full — credit not delivered",
                                ))
                            },
                        );
                    }
                    _ => {
                        let _ = respond.send(Err(limit_err(
                            "max_stream_credit",
                            lim.max_stream_credit,
                            format!("grant of {n} exceeds the outstanding-credit cap"),
                        )));
                    }
                }
            }
        }
    }

    pub(super) fn cancel_owned(&mut self, owner: String, before: Option<u64>) {
        // Window destroyed (before: None) or new
        // document loading (Some watermark): cancel
        // every task it owned / owned before the mark.
        // Supervisor-initiated, so the owner check does
        // not apply; delivery failures only lose a
        // cancel the dead context could never observe.
        for (&task, meta) in self.tasks.iter() {
            if meta.owner.as_deref() == Some(owner.as_str()) && before.is_none_or(|b| task < b) {
                let id = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
                let body = proto::request(id, "cancel", serde_json::json!({"task": task}));
                let (ack, _) = oneshot::channel();
                if send_ctrl(&self.io, &body, self.sup.config.max_frame) {
                    self.pending
                        .insert(id, PendingKind::Cancel { respond: ack });
                }
            }
        }
    }

    pub(super) async fn shutdown(
        &mut self,
        ack: oneshot::Sender<ShutdownOutcome>,
    ) -> ControlFlow<ExitCause> {
        self.sup.shutdown_acks.push(ack);
        let id = self.sup.next_id.fetch_add(1, Ordering::Relaxed);
        // The runtime's drain budget must finish inside
        // our kill timer: give it the grace minus ~1s
        // for the ack, exit and reap, floored so a tiny
        // configured grace still buys real drain time.
        // grace_ms is ONE budget the runtime shares
        // across every shutdown phase (protocol §7).
        let grace_ms = self
            .sup
            .config
            .shutdown_grace
            .saturating_sub(Duration::from_millis(1000))
            .max(Duration::from_millis(500))
            .as_millis() as u64;
        let body = proto::request(id, "shutdown", serde_json::json!({"grace_ms": grace_ms}));
        // Flush ordering: calls committed to the data
        // lane before this shutdown must be written
        // first, so the drain can return their real
        // terminal instead of a generic unavailable.
        let _ = send_ctrl_flushed(&self.io, &body, self.sup.config.max_frame);
        // Graceful exit within the grace window consumes
        // `exited`; on timeout it is still pollable, so
        // kill the tree and wait for the reap. Report
        // what actually happened (RC.6C).
        let outcome = if timeout(self.sup.config.shutdown_grace, &mut self.io.exited)
            .await
            .is_ok()
        {
            ShutdownOutcome::Terminated
        } else if kill_and_reap(&mut self.io).await {
            ShutdownOutcome::ForcedKill
        } else {
            ShutdownOutcome::Incomplete
        };
        self.sup.shutdown_outcome = Some(outcome);
        // The runtime may have written final responses
        // (TASK_CANCELLED, results) before exiting —
        // deliver them so callers get what actually
        // happened, then sweep the rest.
        drain_exited_inbox(
            &mut self.io,
            &mut self.pending,
            &mut self.tasks,
            &mut self.task_of_call,
            &mut self.done_tasks,
            &self.sup.fair,
        )
        .await;
        fail_all(&mut self.pending, proto::RUNTIME_UNAVAILABLE, "shutdown").await;
        ControlFlow::Break(ExitCause::CleanShutdown)
    }
}
