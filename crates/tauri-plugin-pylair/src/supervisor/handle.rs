//! The public face of the supervisor: `Handle` and the commands it sends
//! to the actor, plus the typed outcomes of cancel and shutdown
//! (docs/cancellation.md; protocol-v1.md section 3.2).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

use super::config::{Limits, RuntimeKind, State};
use super::fairness::{release_queued, FairnessRef, FairnessSnapshot};
use super::stream::{stream_pair, StreamRx, StreamSink};
use crate::protocol::{self as proto, ErrorObj};

pub(super) enum Command {
    Call {
        fn_name: String,
        input: Value,
        /// Client intent to declare on the wire (`"stream": <bool>`).
        /// `None` sends no field — the legacy shape a pre-flag host used,
        /// which the runtime answers without mismatch validation.
        stream: Option<bool>,
        respond: oneshot::Sender<Result<Value, ErrorObj>>,
        task_out: oneshot::Sender<u64>,
        owner: Option<String>,
    },
    CallStream {
        fn_name: String,
        input: Value,
        /// Declared client intent: `Some(false)` is a plain `call()` —
        /// no stream slot, no initial credit, `"stream": false` on the
        /// wire. `Some(true)` is `stream()` — slot + credit, `"stream":
        /// true`. `None` is a pre-flag client: accounted as a stream
        /// (slot + credit) but the field is OMITTED from the request so
        /// the runtime applies no mismatch validation.
        stream: Option<bool>,
        items: StreamSink,
        /// Ok(task id) or the typed rejection — `call_stream` must surface
        /// the REAL error (e.g. LIMIT_EXCEEDED), not a bare disconnect.
        task_out: oneshot::Sender<Result<u64, ErrorObj>>,
        owner: Option<String>,
    },
    Cancel {
        task: u64,
        respond: oneshot::Sender<CancelOutcome>,
        caller: Option<String>,
    },
    GrantCredit {
        task: u64,
        n: u64,
        respond: oneshot::Sender<Result<(), ErrorObj>>,
        caller: Option<String>,
    },
    /// Cancel every task owned by this webview label — sent when the
    /// window is destroyed or its document reloads (supervisor-initiated,
    /// not a JS caller). `before` bounds the reap to tasks allocated
    /// before a watermark: on a page-load the new document's calls may
    /// register before this command lands, and they must survive.
    CancelOwned { owner: String, before: Option<u64> },
    /// Operator-initiated restart out of the Failed state.
    Retry,
    /// Stop the runtime. `ack` fires only after the process tree is reaped,
    /// carrying what cleanup actually achieved.
    Shutdown {
        ack: oneshot::Sender<ShutdownOutcome>,
    },
}

/// What happened to a cancel request — distinct outcomes so a caller never
/// has to guess whether its frame landed or its task stopped (RC.6B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// Runtime confirmed the task stopped.
    Cancelled,
    /// Delivered, but the work can't be interrupted (inline sync): the
    /// caller sees TASK_CANCELLED while the Python may run to completion.
    RequestedMayContinue,
    /// Isolated worker was terminated and its process reaped.
    Terminated,
    /// Termination was requested but couldn't be confirmed — the worker
    /// may still be running.
    TerminationUncertain,
    /// The task already finished before the cancel landed.
    Completed,
    /// No such task in this runtime generation (stale or unknown id).
    NotFound,
    /// The caller does not own this task.
    Rejected,
    /// The control frame could not be queued — nothing was delivered.
    DeliveryFailed,
}

/// What shutdown actually achieved — reported, never assumed (RC.6C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownOutcome {
    /// Graceful exit inside the grace window.
    Terminated,
    /// Had to SIGKILL/taskkill — the tree is confirmed dead.
    ForcedKill,
    /// Kill sent but the exit wasn't confirmed in the wait window.
    Incomplete,
    /// The runtime was already gone or its state unknown.
    Unknown,
}

/// An eagerly-dispatched call: the request is already on its way when this
/// returns (unlike `call`, an async fn which is lazy until polled). `task`
/// resolves to the runtime task id — needed for `cancel`.
pub struct CallTask {
    pub task: oneshot::Receiver<u64>,
    pub result: oneshot::Receiver<Result<Value, ErrorObj>>,
}

/// Handle the app holds; clones freely.
#[derive(Clone)]
pub struct Handle {
    pub(super) cmd: mpsc::Sender<Command>,
    pub(super) state: watch::Receiver<State>,
    pub(super) ready_url: watch::Receiver<Option<String>>,
    pub(super) kind: RuntimeKind,
    pub(super) stream_buffer: usize,
    pub(super) cancel_timeout: Duration,
    /// Shared with the actor: total stream bytes buffered for ALL
    /// consumers, metered by each chunk's `StreamLease` (RC.8A).
    pub(super) stream_bytes_total: Arc<AtomicU64>,
    /// Shared with the actor: the next task id it will allocate. A page
    /// load reads this as a watermark — tasks with id < watermark existed
    /// before the navigation committed and belong to the dead document.
    pub(super) next_task: Arc<AtomicU64>,
    pub(super) limits: Arc<Limits>,
    /// Per-window accounting shared with the actor
    /// (docs/decisions/0005). Counting only - nothing is refused.
    pub(super) fair: FairnessRef,
}

impl Handle {
    fn queue_err(&self) -> ErrorObj {
        limit_err(
            "max_queued_commands",
            self.limits.max_queued_commands as u64,
            "command queue full — the supervisor is saturated",
        )
    }

    pub fn state(&self) -> State {
        *self.state.borrow()
    }

    /// What the supervised child is (`Bridge` unless configured `Web`).
    pub fn kind(&self) -> RuntimeKind {
        self.kind
    }

    /// The validated url the wrapped web server answers at, while the
    /// current generation is `Ready` or `Unresponsive` (`Web` kind only;
    /// always `None` for the bridge kind). Cleared the moment the
    /// generation ends: a restart is a new port, so never cache it.
    pub fn ready_url(&self) -> Option<String> {
        self.ready_url.borrow().clone()
    }

    /// Subscribe to changes of [`Handle::ready_url`].
    pub fn subscribe_ready_url(&self) -> watch::Receiver<Option<String>> {
        self.ready_url.clone()
    }

    /// Subscribe to state changes (UI lifecycle events).
    pub fn subscribe_state(&self) -> watch::Receiver<State> {
        self.state.clone()
    }

    /// Dispatch a call immediately (non-async, try_send) and get its task id.
    /// `owner` is the controlling caller's identity (a webview label from
    /// Tauri — never a JS-supplied value); `None` marks the task unowned
    /// (test/dev paths only — plugin commands always pass the label).
    /// `stream` is the wire `stream` field: `Some(false)` for `call()`
    /// (the only sane value here — `Some(true)` asks the runtime to
    /// stream into a call that cannot receive chunks), `None` omits the
    /// field entirely (no runtime mismatch validation).
    pub fn call_task(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
        stream: Option<bool>,
    ) -> Result<CallTask, ErrorObj> {
        let (rtx, rrx) = oneshot::channel();
        let (ttx, trx) = oneshot::channel();
        // Fairness admission bookkeeping: a queue slot is charged here
        // and released when the actor dequeues the command (decisions
        // 0005, PR 1: counting only - never refused).
        self.fair.lock().unwrap().admit_queue(&owner);
        self.cmd
            .try_send(Command::Call {
                fn_name: fn_name.into(),
                input,
                stream,
                respond: rtx,
                task_out: ttx,
                owner,
            })
            .map_err(|e| {
                // Unsent: the command comes back, so does its charge.
                let full = matches!(e, mpsc::error::TrySendError::Full(_));
                release_queued(&self.fair, &e.into_inner());
                if full {
                    self.queue_err()
                } else {
                    stopped_err()
                }
            })?;
        Ok(CallTask {
            task: trx,
            result: rrx,
        })
    }

    /// Call a function and await its result. For cancellation, prefer
    /// `call_task` — a pending `call` future carries no task id.
    pub async fn call(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
        stream: Option<bool>,
    ) -> Result<Value, ErrorObj> {
        let ct = self.call_task(fn_name, input, owner, stream)?;
        ct.result.await.map_err(|_| stopped_err())?
    }

    /// Start a call delivered over a per-call stream channel — chunks
    /// until `StreamItem::End`. `stream` is the declared client intent
    /// (`Some(false)` = plain `call()`, `Some(true)` = `stream()`,
    /// `None` = pre-flag client, see `Command::CallStream`).
    pub async fn call_stream(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
        stream: Option<bool>,
    ) -> Result<(u64, StreamRx), ErrorObj> {
        let (items_tx, items_rx) = stream_pair(
            self.stream_buffer,
            self.limits.stream_buffer_bytes as u64,
            self.stream_bytes_total.clone(),
            self.limits.stream_buffer_bytes_total as u64,
        );
        let (task_tx, task_rx) = oneshot::channel();
        self.fair.lock().unwrap().admit_queue(&owner);
        self.cmd
            .try_send(Command::CallStream {
                fn_name: fn_name.into(),
                input,
                stream,
                items: items_tx,
                task_out: task_tx,
                owner,
            })
            .map_err(|e| {
                let full = matches!(e, mpsc::error::TrySendError::Full(_));
                release_queued(&self.fair, &e.into_inner());
                if full {
                    self.queue_err()
                } else {
                    stopped_err()
                }
            })?;
        let task = task_rx.await.map_err(|_| stopped_err())??;
        Ok((task, items_rx))
    }

    /// Retry after a Failed state — respawns the runtime fresh.
    pub async fn retry(&self) -> Result<(), ErrorObj> {
        self.cmd.try_send(Command::Retry).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => self.queue_err(),
            mpsc::error::TrySendError::Closed(_) => stopped_err(),
        })
    }

    /// Ask the supervisor to cancel `task` as `caller`. Resolves to a
    /// `CancelOutcome` — never an ambiguous bool (RC.6B). A full command
    /// queue is a `LIMIT_EXCEEDED` error, not a wait.
    pub async fn cancel(
        &self,
        task: u64,
        caller: Option<String>,
    ) -> Result<CancelOutcome, ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .try_send(Command::Cancel {
                task,
                respond: tx,
                caller,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => self.queue_err(),
                mpsc::error::TrySendError::Closed(_) => stopped_err(),
            })?;
        match timeout(self.cancel_timeout, rx).await {
            Ok(r) => r.map_err(|_| stopped_err()),
            Err(_) => Err(ErrorObj {
                code: proto::RUNTIME_WEDGED,
                message: format!(
                    "cancel not acknowledged within {:?}: the runtime's event loop may be blocked",
                    self.cancel_timeout
                ),
                data: None,
            }),
        }
    }

    /// Cancel every task owned by `owner` — used when a window is destroyed.
    /// Async so the CALLER picks the executor — the same rule as `spawn`
    /// below. Window events run on the main thread where no reactor is
    /// entered; a `tokio::spawn` inside this method panicked there. The
    /// awaited send keeps the non-dropping guarantee: a momentarily-full
    /// command queue cannot silently lose it (the window's tasks would
    /// run on orphaned) — and nobody is left to report a loss to anyway.
    pub async fn cancel_owner(&self, owner: String) {
        let _ = self
            .cmd
            .send(Command::CancelOwned {
                owner,
                before: None,
            })
            .await;
    }

    /// The next task id the actor will allocate — a watermark for
    /// `cancel_owner_before`. Read cheap: a shared atomic.
    pub fn task_watermark(&self) -> u64 {
        self.next_task.load(Ordering::Relaxed)
    }

    /// The shared fairness map, for `WindowRegistry` to maintain the
    /// live set (builder wiring).
    pub(crate) fn fairness(&self) -> FairnessRef {
        self.fair.clone()
    }

    /// A copy of the fairness counters - the only observability
    /// integration tests get into the map (decisions 0005, PR 1).
    #[doc(hidden)]
    pub fn fairness_snapshot(&self) -> FairnessSnapshot {
        self.fair.lock().unwrap().snapshot()
    }

    /// Cancel every task `owner` allocated BEFORE `before` — used when a
    /// webview starts loading a new document. The label survives reloads,
    /// so unlike `cancel_owner` this must not sweep tasks the NEW
    /// document managed to register before this command lands: their ids
    /// are >= the watermark and they stay. Same awaited-send,
    /// never-dropped guarantee as `cancel_owner`.
    pub async fn cancel_owner_before(&self, owner: String, before: u64) {
        let _ = self
            .cmd
            .send(Command::CancelOwned {
                owner,
                before: Some(before),
            })
            .await;
    }

    /// Cancel exactly `task` as its owner — used by the stream forwarder
    /// when the call's channel dies. Unlike `cancel` this awaits the
    /// command send, so a momentarily-full queue cannot silently drop the
    /// reap the way a `try_send` could; the outcome is discarded — the
    /// dead consumer could never observe it. Cancelling one task leaves
    /// same-owner siblings alone (a window closed and reopened under the
    /// same label may hold newer live calls).
    pub async fn cancel_task(&self, task: u64, owner: String) {
        let (tx, _rx) = oneshot::channel();
        let _ = self
            .cmd
            .send(Command::Cancel {
                task,
                respond: tx,
                caller: Some(owner),
            })
            .await;
    }

    /// Grant more stream pull-credit to `task` as `caller`.
    pub async fn grant_credit(
        &self,
        task: u64,
        n: u64,
        caller: Option<String>,
    ) -> Result<(), ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .try_send(Command::GrantCredit {
                task,
                n,
                respond: tx,
                caller,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => self.queue_err(),
                mpsc::error::TrySendError::Closed(_) => stopped_err(),
            })?;
        rx.await.map_err(|_| stopped_err())?
    }

    /// Stop the runtime and wait until its process tree is gone (or the
    /// supervisor has already stopped). Returns what cleanup achieved.
    /// The awaited send is deliberate: a shutdown must never be dropped.
    pub async fn shutdown(&self) -> ShutdownOutcome {
        let (ack, done) = oneshot::channel();
        if self.cmd.send(Command::Shutdown { ack }).await.is_ok() {
            done.await.unwrap_or(ShutdownOutcome::Unknown)
        } else {
            ShutdownOutcome::Unknown
        }
    }
}

pub(super) fn stopped_err() -> ErrorObj {
    ErrorObj {
        code: proto::RUNTIME_UNAVAILABLE,
        message: "runtime is stopped".into(),
        data: None,
    }
}

/// Overload always answers with an explicit, typed error naming the limit
/// (RC.8A) — never a silent wait, a drop, or a mislabeled "stopped".
pub(super) fn limit_err(limit: &str, max: u64, msg: impl Into<String>) -> ErrorObj {
    ErrorObj {
        code: proto::LIMIT_EXCEEDED,
        message: msg.into(),
        data: Some(serde_json::json!({"limit": limit, "max": max})),
    }
}
