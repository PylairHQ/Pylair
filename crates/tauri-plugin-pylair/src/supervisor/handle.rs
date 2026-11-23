//! The public face of the supervisor: `Handle` and the commands it sends
//! to the actor, plus the typed outcomes of cancel and shutdown
//! (docs/cancellation.md; protocol-v1.md section 3.2).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

use super::config::{Limits, RuntimeKind, State};
use super::fairness::{release_queued, FairnessRef, FairnessSnapshot, Resource};
use super::health::{
    AppComponentSnapshot, AppSnapshot, BreakerSnapshot, HealthRef, HealthSnapshot,
    LastExitSnapshot, LivenessSnapshot, LoadSnapshot, RestartsSnapshot, SelfResource, SelfSnapshot,
};
use super::stream::{stream_pair, StreamRx, StreamSink};
use crate::blobs::BlobRegistryRef;
use crate::protocol::{self as proto, ErrorObj};
use crate::util::fmt_state;

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
    /// Live blob handles shared with the actor and the `pylair-blob`
    /// scheme handler (docs/decisions/0010): `blob.register` fills it,
    /// window destroy drops an owner's entries, generation end clears it.
    pub(super) blobs: BlobRegistryRef,
    /// The `Handle::health` reads of the shared health block
    /// (docs/decisions/0006 §1) plus the three config constants its
    /// schema publishes.
    pub(super) health: HealthRef,
    pub(super) missed_pongs_to_kill: u32,
    pub(super) breaker_crashes: usize,
    pub(super) breaker_window: Duration,
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

    /// A point-in-time observability snapshot (docs/decisions/0006 §4):
    /// state and time-in-state, generation uptime, restart accounting,
    /// the transport/loop liveness verdicts, global load totals, and
    /// the CALLER's own usage/ceilings — `self` is `None` for
    /// `owner: None` and when `per_window_fairness` is off, and never
    /// exposes another window's row. `#[non_exhaustive]` makes additive
    /// v1 fields semver-safe; serde serializes it for the command
    /// boundary.
    pub fn health(&self, owner: Option<&str>) -> HealthSnapshot {
        let now = Instant::now();
        let state = self.state();
        // "live generation" = a Ready-or-Unresponsive child: liveness
        // fields and verdicts are per-generation, so outside it they
        // report null/unknown regardless of what the shared block holds.
        let live = matches!(state, State::Ready | State::Unresponsive);
        let (
            in_state_ms,
            generation,
            uptime_ms,
            last_exit,
            crashes_in_window,
            backoff_ms,
            last_pong_age_ms,
            rtt_ms,
            missed_pongs,
            loop_stale_ms,
            app_map,
        ) = {
            let h = self.health.lock().unwrap();
            (
                h.state_since.elapsed().as_millis() as u64,
                h.generation,
                h.ready_at
                    .map(|t| now.saturating_duration_since(t).as_millis() as u64),
                h.last_exit.clone(),
                h.crashes_in_window as u64,
                h.backoff.as_millis() as u64,
                if live {
                    h.last_pong_at
                        .map(|t| now.saturating_duration_since(t).as_millis() as u64)
                } else {
                    None
                },
                if live {
                    h.last_rtt.map(|d| d.as_millis() as u64)
                } else {
                    None
                },
                if live { h.missed_pongs } else { 0 },
                // Outer Some/None is the bridge/web absent rule; the
                // inner Option is "null until the first pong".
                if self.kind == RuntimeKind::Web {
                    None
                } else {
                    Some(if live {
                        h.last_loop_stale.map(|d| d.as_millis() as u64)
                    } else {
                        None
                    })
                },
                h.app.clone(),
            )
        };
        // Same rule the pylair://health event emits on
        // (health.rs::verdicts) — one source, snapshot and event
        // cannot drift apart.
        let (transport, loop_verdict) = self.health.lock().unwrap().verdicts(self.kind, live);
        let f = self.fair.lock().unwrap();
        let totals = f.snapshot().totals;
        let self_usage = match (self.limits.per_window_fairness, owner) {
            (true, Some(label)) => {
                let o = Some(label.to_string());
                let c = f.counts_of(&o);
                let lim = &self.limits;
                let cell = |in_use: u64, resource: Resource, cap: usize| SelfResource {
                    in_use,
                    ceiling: f.ceiling_of(&o, resource, cap) as u64,
                    cap: cap as u64,
                };
                Some(SelfSnapshot {
                    window: label.to_string(),
                    tasks: cell(c.in_flight, Resource::Tasks, lim.max_concurrent_tasks),
                    streams: cell(
                        c.in_flight_streams,
                        Resource::Streams,
                        lim.max_concurrent_streams,
                    ),
                    queued: cell(c.in_queue, Resource::Queue, lim.max_queued_commands),
                    isolated: cell(
                        c.in_flight_isolated,
                        Resource::Isolated,
                        lim.max_isolated_workers,
                    ),
                })
            }
            // Host-side reads and fairness-off deployments get no
            // caller row (decisions 0006 §4).
            _ => None,
        };
        drop(f);
        HealthSnapshot {
            version: 1,
            kind: match self.kind {
                RuntimeKind::Bridge => "bridge",
                RuntimeKind::Web => "web",
            },
            state: fmt_state(state),
            in_state_ms,
            generation,
            uptime_ms,
            url: match self.kind {
                RuntimeKind::Web => Some(self.ready_url()),
                RuntimeKind::Bridge => None,
            },
            restarts: RestartsSnapshot {
                crashes_in_window,
                breaker: BreakerSnapshot {
                    crashes: self.breaker_crashes as u64,
                    window_ms: self.breaker_window.as_millis() as u64,
                },
                backoff_ms,
                last_exit: last_exit.map(|e| LastExitSnapshot {
                    kind: e.kind,
                    detail: e.detail,
                    ms_ago: e.at.elapsed().as_millis() as u64,
                }),
            },
            liveness: LivenessSnapshot {
                last_pong_age_ms,
                rtt_ms,
                missed_pongs,
                missed_pongs_to_kill: self.missed_pongs_to_kill,
                loop_stale_ms,
                transport,
                loop_verdict,
            },
            load: LoadSnapshot {
                tasks: totals.in_flight,
                streams: totals.in_flight_streams,
                isolated: totals.in_flight_isolated,
                queued: totals.in_queue,
                stream_bytes_total: self.stream_bytes_total.load(Ordering::Relaxed),
            },
            self_usage,
            // Runtime-wide app state, not caller-scoped (0007 §5):
            // `null` on the web kind and whenever the generation has
            // reported nothing — `health(None)` still includes it.
            app: if self.kind == RuntimeKind::Web || app_map.is_empty() {
                None
            } else {
                Some(AppSnapshot {
                    components: app_map
                        .iter()
                        .map(|(name, c)| {
                            (
                                name.clone(),
                                AppComponentSnapshot {
                                    phase: c.phase,
                                    progress: c.progress,
                                    detail: c.detail.clone(),
                                    ms_ago: c.at.elapsed().as_millis() as u64,
                                },
                            )
                        })
                        .collect(),
                })
            },
        }
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
        // Fairness queue admission (decisions 0005, PR 2): the share
        // check and the charge are one operation under the lock; the
        // slot is released when the actor dequeues the command.
        self.fair
            .lock()
            .unwrap()
            .try_admit_queue(&owner, self.limits.max_queued_commands)
            .map_err(|ceiling| share_err("the command queue", ceiling, &owner))?;
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
        self.fair
            .lock()
            .unwrap()
            .try_admit_queue(&owner, self.limits.max_queued_commands)
            .map_err(|ceiling| share_err("the command queue", ceiling, &owner))?;
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

    /// The shared blob registry (decision 0010) — the scheme handler,
    /// the window-destroy hook and tests reach it through here.
    #[doc(hidden)]
    pub fn blobs(&self) -> BlobRegistryRef {
        self.blobs.clone()
    }

    /// `python_release_blob` / client `release()`: drop the handle and
    /// unlink the file. Owner-checked like every command — `caller` is
    /// the invoking window's label, never a JS parameter. Idempotent: an
    /// unknown id is Ok(false), a foreign-owned id is PERMISSION_DENIED.
    pub async fn release_blob(&self, id: &str, caller: &str) -> Result<bool, ErrorObj> {
        self.blobs
            .lock()
            .unwrap()
            .release_owned(id, caller)
            .map_err(|_| ErrorObj {
                code: proto::PERMISSION_DENIED,
                message: "blob is owned by another window".into(),
                data: None,
            })
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

    /// Test hook: join/leave the fairness live set. Integration tests
    /// bypass the Tauri command layer where real subscription happens
    /// (`WindowRegistry::subscribe_live`).
    #[doc(hidden)]
    pub fn fairness_join_window(&self, label: &str) {
        self.fair.lock().unwrap().join_window(label);
    }

    /// See `fairness_join_window`.
    #[doc(hidden)]
    pub fn fairness_leave_window(&self, label: &str) {
        self.fair.lock().unwrap().leave_window(label);
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

/// `LIMIT_EXCEEDED` for an owner over its fairness share of a shared
/// cap (decisions 0005). `window` names the refused owner so hog and
/// victim are distinguishable; it is null for the host-side bucket.
pub(super) fn share_err(what: &str, max: usize, owner: &Option<String>) -> ErrorObj {
    ErrorObj {
        code: proto::LIMIT_EXCEEDED,
        message: format!("window is over its fairness share of {what}"),
        data: Some(serde_json::json!({
            "limit": "window_share",
            "max": max,
            "window": owner,
        })),
    }
}
