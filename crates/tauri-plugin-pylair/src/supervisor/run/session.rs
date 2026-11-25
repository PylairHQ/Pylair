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
use super::super::fairness::release_queued;
use super::super::handle::{limit_err, Command, ShutdownOutcome};
use super::super::health::AppPhase;
use super::super::stream::ChunkFail;
use super::super::wire::{cancel_outcome, mtime, send_ctrl};
use super::{
    classify_pong, drain_exited_inbox, fail_all, kill_and_reap, ChildIo, ChildReadErr, ExitCause,
    PendingKind, PongVerdict, Supervisor, TaskMeta,
};
use crate::blobs::Reject;
use crate::protocol::{self as proto, ErrorObj, InMsg};

/// `$blob` tag ids nested anywhere inside a delivered frame value.
/// Depth-capped far past the wire's own MAX_DEPTH — tags deeper than
/// that never shipped, so their ids stay pending and are released as
/// orphans rather than trusted to a page that cannot see them.
fn blob_ids_in(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    match v {
        Value::Object(map) => {
            if let Some(id) = map
                .get("$blob")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
            {
                out.push(id.to_string());
            }
            for x in map.values() {
                blob_ids_in(x, depth - 1, out);
            }
        }
        Value::Array(xs) => {
            for x in xs {
                blob_ids_in(x, depth - 1, out);
            }
        }
        _ => {}
    }
}

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
    /// This generation's `PYLAIR_BLOB_DIR` (raw path + canonical root the
    /// registry enforces registration containment against), or None when
    /// the dir could not be resolved — blobs are then disabled for the
    /// generation (decision 0010 §3: env absence is the capability bit).
    pub(super) blob_dir: Option<(PathBuf, PathBuf)>,
    /// `blob.register` rejections keyed by task id. A register lands on
    /// the FIFO ahead of the frame carrying its `$blob` tag, so the tag
    /// would arrive unresolvable — the task's own terminal (or the next
    /// `stream.chunk`) delivers the rejection as a typed error instead
    /// (decision 0010 §6: caps are LIMIT_EXCEEDED + data.limit, anything
    /// else INTERNAL).
    pub(super) blob_rejects: HashMap<u64, Reject>,
    /// Ids a `blob.register` succeeded for, whose `$blob` tag the page
    /// has not yet received, keyed by task. One frame can carry several
    /// tags — when the frame is replaced by an error (or never reaches
    /// a consumer), the page never learns the earlier ids; they are
    /// released there rather than holding slots and disk until the
    /// window closes — which is exactly when limits are near.
    pub(super) blob_pending: HashMap<u64, Vec<String>>,
    /// The open loop-stall episode, if any pong reported `loop_stale_ms`
    /// at or past the episode floor since the last clean one — `None`
    /// outside an episode. Carries the warn-once state so an episode
    /// logs exactly one warning and one recovery line.
    pub(super) stall_episode: Option<StallEpisode>,
}

/// One contiguous blocked period — `loop_stale_ms` climbs while the
/// loop is stalled and only drops once it makes progress again, so an
/// episode ends at the first pong back under the floor (`loop_stall_warn`,
/// or `LOOP_STALL_WARN` when the configured threshold is higher — low
/// enough that a stall beginning during app load is already an episode
/// by the time it crosses the warn threshold).
pub(super) struct StallEpisode {
    /// The warning already went out — later stale pongs in the same
    /// episode stay silent.
    pub(super) warned: bool,
    /// The episode began while `health().app` had a component still
    /// `loading` — `on_startup` blocking is expected then, so the
    /// warning (if it ever fires) says the stall began during startup.
    pub(super) began_loading: bool,
    /// Largest `loop_stale_ms` seen this episode — the recovery line's
    /// "blocked for Ns".
    pub(super) peak: Duration,
}

impl<'a> Session<'a> {
    pub(super) fn new(
        sup: &'a mut Supervisor,
        io: ChildIo,
        blob_dir: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        // The new generation's namespace: clears anything left over and
        // sets the canonical containment root for `blob.register`.
        crate::blobs::lock_blobs(&sup.blobs)
            .set_generation(blob_dir.as_ref().map(|(_, c)| c.clone()));
        Session {
            sup,
            done_tasks: HashSet::new(),
            tasks: HashMap::new(),
            task_of_call: HashMap::new(),
            pending: HashMap::new(),
            io,
            pongs_received: 0,
            ping_seq: 0,
            blob_dir,
            blob_rejects: HashMap::new(),
            blob_pending: HashMap::new(),
            stall_episode: None,
        }
    }

    /// A rejected `blob.register` becomes the task's own error
    /// (decision 0010 §6): a registry cap is `LIMIT_EXCEEDED` with the
    /// doc's `data.limit` name and a message naming the env knob;
    /// anything malformed is `INTERNAL`. The alternative — shipping the
    /// response with its `$blob` tag intact — fetches as a 404 with no
    /// hint that `release()` would have freed the slot.
    fn blob_reject_err(&self, rej: Reject) -> ErrorObj {
        let limits = &crate::blobs::lock_blobs(&self.sup.blobs).limits;
        match rej {
            Reject::Limit(name) => {
                let (knob, max) = match name {
                    "blob_bytes" => ("PYLAIR_MAX_BLOB_BYTES", Some(limits.max_blob_bytes)),
                    "blob_total" => ("PYLAIR_MAX_BLOB_TOTAL", Some(limits.max_blob_total)),
                    "blob_handles" => (
                        "PYLAIR_MAX_BLOBS_PER_WINDOW",
                        Some(limits.max_blobs_per_window as u64),
                    ),
                    _ => ("per-window share of PYLAIR_MAX_BLOB_TOTAL", None),
                };
                let hint = if name == "blob_bytes" {
                    "the blob itself is over the cap — releasing other handles cannot help"
                } else {
                    "call release() on BlobHandles the page is done with"
                };
                ErrorObj {
                    code: proto::LIMIT_EXCEEDED,
                    message: format!("blob.register refused on the {name} cap ({knob}) — {hint}"),
                    data: Some(match max {
                        Some(max) => serde_json::json!({"limit": name, "max": max}),
                        None => serde_json::json!({"limit": name}),
                    }),
                }
            }
            Reject::Bad(why) => ErrorObj {
                code: proto::INTERNAL,
                message: format!("blob.register rejected: {why}"),
                data: None,
            },
            Reject::Foreign => ErrorObj {
                code: proto::INTERNAL,
                message: "blob.register rejected: handle owned by another window".into(),
                data: None,
            },
        }
    }

    /// Release every registered-but-undelivered blob id a task holds —
    /// the frame that would have carried their tags was replaced or
    /// never reached a consumer, so the page can never name them. The
    /// registry release unlinks the file and frees the handle slot and
    /// generation bytes.
    fn release_pending_blobs(&mut self, task: u64) {
        let Some(ids) = self.blob_pending.remove(&task) else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        let mut reg = crate::blobs::lock_blobs(&self.sup.blobs);
        for id in &ids {
            reg.release(id);
        }
        let _ = self.sup.events.try_send(RuntimeEvent::Stderr(format!(
            "task {task}: released {} orphaned blob handle(s)",
            ids.len()
        )));
    }

    /// A frame reached the page: its `$blob` ids are page-owned now.
    /// Anything else registered since the last forwarded frame is an
    /// orphan — released before it can hold a slot.
    fn settle_shipped_blobs(&mut self, task: u64, shipped: &[String]) {
        let Some(ids) = self.blob_pending.remove(&task) else {
            return;
        };
        let orphans: Vec<String> = ids
            .into_iter()
            .filter(|id| !shipped.iter().any(|s| s == id))
            .collect();
        if orphans.is_empty() {
            return;
        }
        let mut reg = crate::blobs::lock_blobs(&self.sup.blobs);
        for id in &orphans {
            reg.release(id);
        }
        let _ = self.sup.events.try_send(RuntimeEvent::Stderr(format!(
            "task {task}: released {} orphaned blob handle(s)",
            orphans.len()
        )));
    }

    /// Scan the value being delivered for `$blob` ids, then settle.
    fn settle_forwarded_blobs(&mut self, task: u64, v: &Value) {
        if !self.blob_pending.contains_key(&task) {
            return;
        }
        let mut shipped = Vec::new();
        blob_ids_in(v, 64, &mut shipped);
        self.settle_shipped_blobs(task, &shipped);
    }

    /// Tombstone a task the host ended early — the same treatment a
    /// client cancel gives it: drop the accounting, then send the
    /// runtime a `cancel` request so the producer stops instead of
    /// running on unobserved.
    fn finish_task(&mut self, call_id: u64, task: u64) {
        self.task_of_call.remove(&call_id);
        if let Some(meta) = self.tasks.remove(&task) {
            self.sup.fair.lock().unwrap().note_finished(
                &meta.owner,
                meta.is_stream,
                meta.is_isolated,
            );
        }
        self.blob_rejects.remove(&task);
        self.release_pending_blobs(task);
        if self.done_tasks.len() >= 1024 {
            self.done_tasks.clear();
        }
        self.done_tasks.insert(task);
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

    /// The warn-without-kill half of stall handling (`loop_stall_warn`):
    /// one `pylair` stderr line per stall episode, a matching recovery
    /// line when the loop frees, and nothing at all while `health().app`
    /// still reports a component `loading` — `on_startup` blocking is
    /// legitimate. Called for every non-StallKill pong; a configured
    /// kill is fatal before a warning could matter.
    fn note_loop_stall(&mut self, stale: Duration, app_loading: bool) {
        let Some(warn_at) = self.sup.config.loop_stall_warn else {
            return;
        };
        // An episode is one contiguous blocked period — `loop_stale_ms`
        // only drops when the loop makes progress. It starts at the
        // diagnostic floor (or the configured threshold when that is
        // lower) so a stall that began during app load and crosses the
        // warn threshold once the app is ready still carries
        // `began_loading`.
        let floor = warn_at.min(super::LOOP_STALL_WARN);
        if stale < floor {
            // Episode over (or never began): a warned episode earns its
            // recovery line; an unwarned one ends silently.
            if let Some(ep) = self.stall_episode.take() {
                if ep.warned {
                    let _ = self.sup.events.try_send(RuntimeEvent::Stderr(format!(
                        "pylair: event loop recovered after {:?} blocked",
                        ep.peak
                    )));
                }
            }
            return;
        }
        let ep = self.stall_episode.get_or_insert(StallEpisode {
            warned: false,
            began_loading: app_loading,
            peak: stale,
        });
        ep.peak = ep.peak.max(stale);
        if ep.warned || stale < warn_at {
            return; // one warning per episode, not one per pong
        }
        if app_loading {
            // Suppressed — on_startup may legitimately block the loop
            // for minutes (a model load). If the stall outlasts the
            // load, the next pong warns and says it began then.
            return;
        }
        ep.warned = true;
        let mut msg = format!(
            "pylair: event loop blocked for {stale:?} — every call waits. \
             Blocking work inside an `async def` stalls the whole loop; use a plain \
             `def` (Pylair runs it in a worker thread), `await asyncio.to_thread(...)`, \
             or worker=\"isolated\"/\"warm\" for heavy CPU or model work"
        );
        // The call(s) in flight when the stall began are the likeliest
        // holders — name up to three so the warning points at the code.
        if !self.tasks.is_empty() {
            let mut names: Vec<&str> = self.tasks.values().map(|m| m.fn_name.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            let more = names.len().saturating_sub(3);
            let shown: Vec<&str> = names.into_iter().take(3).collect();
            msg.push_str(&format!(" — in flight: {}", shown.join(", ")));
            if more > 0 {
                msg.push_str(&format!(" (+{more} more)"));
            }
        }
        if ep.began_loading {
            msg.push_str(
                " (the stall began during app startup, when on_startup blocking is expected)",
            );
        }
        let _ = self.sup.events.try_send(RuntimeEvent::Stderr(msg));
    }
}

impl Drop for Session<'_> {
    /// Teardown, not the next spawn, ends a generation's accounting:
    /// every exit path out of `run_child` lands here, so a crashed
    /// generation's `in_flight` is zeroed before the restart backoff
    /// instead of leaking into it (queue debt is untouched - a queued
    /// command still owes its release when dequeued).
    fn drop(&mut self) {
        // into_inner, not unwrap: a poisoned mutex must not panic in
        // drop - a second panic while unwinding aborts the process.
        self.sup
            .fair
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end_generation();
        // Per-generation liveness dies with the Session
        // (decisions 0006 §1): pong ages, RTT and the seq map mean
        // nothing once the child is gone. A verdict the generation
        // emitted is corrected to `unknown` first (0006 §5); a
        // generation that never emitted stays silent.
        let health_ev = {
            let mut h = self.sup.health.lock().unwrap_or_else(|e| e.into_inner());
            h.end_generation(self.sup.config.kind, self.sup.config.missed_pongs_to_kill)
        };
        if let Some(payload) = health_ev {
            let _ = self
                .sup
                .events
                .try_send(RuntimeEvent::HealthChanged(payload));
        }
        // Blob lifetime ends with the generation (decision 0010 §6): the
        // registry empties and the dir is deleted wholesale. Every exit
        // path out of `run_child` lands here, so crash, clean shutdown
        // and dev restart are covered the same way.
        crate::blobs::lock_blobs(&self.sup.blobs).end_generation();
        if let Some((dir, _)) = self.blob_dir.take() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                let _ = self.sup.events.try_send(RuntimeEvent::Stderr(format!(
                    "pylair: could not remove blob dir {} ({e})",
                    dir.display()
                )));
            }
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
        let health_ev = {
            let mut h = self.sup.health.lock().unwrap();
            h.note_missed(pongs_outstanding);
            h.health_event(
                self.sup.config.kind,
                true,
                self.sup.config.missed_pongs_to_kill,
            )
        };
        if let Some(payload) = health_ev {
            let _ = self
                .sup
                .events
                .try_send(RuntimeEvent::HealthChanged(payload));
        }
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
        if let Some(bytes) = super::super::wire::frame_bytes(&body) {
            if bytes.len() - 4 <= self.sup.config.max_frame {
                *self.io.ping_slot.lock().unwrap() = Some(bytes);
                self.io.ping_notify.notify_one();
                self.sup
                    .health
                    .lock()
                    .unwrap()
                    .note_ping(self.ping_seq, self.sup.config.missed_pongs_to_kill as usize);
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
    ) -> ExitCause {
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
            &self.sup.fair,
        )
        .await;
        fail_all(&mut self.pending, proto::RUNTIME_CRASHED, &detail).await;
        ExitCause::Crashed(detail)
    }

    pub(super) async fn on_frame(
        &mut self,
        msg: Option<Result<InMsg, ChildReadErr>>,
    ) -> ControlFlow<ExitCause> {
        match msg {
            Some(Ok(InMsg::Response { id, result, error })) => {
                let task = self.task_of_call.get(&id).copied();
                let mut out = match (result, error) {
                    (_, Some(e)) => Err(e),
                    (r, None) => Ok(r.unwrap_or(Value::Null)),
                };
                // A blob.register rejection arrived ahead of this
                // terminal on the FIFO — the result's $blob tag would
                // 404. Deliver the rejection as the call's error
                // instead (decision 0010 §6).
                if let Some(t) = task {
                    if let Some(rej) = self.blob_rejects.remove(&t) {
                        out = Err(self.blob_reject_err(rej));
                    }
                }
                // Inspector end-tap (docs/inspector.md): classify while
                // `out` is still whole, record at the removal below.
                let inspect_end = if self.sup.inspect.enabled() {
                    let (outcome, _) = crate::inspect::classify(&out);
                    let resp = out.as_ref().ok().cloned();
                    Some((outcome, resp))
                } else {
                    None
                };
                let mut delivered = false;
                if let Some(kind) = self.pending.remove(&id) {
                    delivered = true;
                    // Settle pending blob ids while `out` is still
                    // readable: Ok ships its tags to the page; any
                    // error ships none — those handles are orphans.
                    if let Some(t) = task {
                        match &out {
                            Ok(v) => self.settle_forwarded_blobs(t, v),
                            Err(_) => self.release_pending_blobs(t),
                        }
                    }
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
                // A terminal nobody receives — the call was already
                // finished by a cancel, or the id was never a call.
                // Its registered blobs are orphans regardless.
                if !delivered {
                    if let Some(t) = task {
                        self.release_pending_blobs(t);
                    }
                }
                if let Some(task) = self.task_of_call.remove(&id) {
                    if let Some((outcome, resp)) = inspect_end {
                        self.sup.inspect.note_end(task, outcome, resp.as_ref());
                    }
                    if let Some(meta) = self.tasks.remove(&task) {
                        self.sup.fair.lock().unwrap().note_finished(
                            &meta.owner,
                            meta.is_stream,
                            meta.is_isolated,
                        );
                    }
                    self.blob_rejects.remove(&task);
                    self.release_pending_blobs(task);
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
                        let (health_ev, app_loading) = {
                            let mut h = self.sup.health.lock().unwrap();
                            h.note_pong(p["seq"].as_u64(), stale, self.sup.config.ping_interval);
                            // Warm-pool status rides the pong (0008
                            // §health) — mirrored verbatim, replaced
                            // wholesale each time.
                            if let Some(pools) = p.get("pools") {
                                h.note_pools(pools);
                            }
                            (
                                h.health_event(
                                    self.sup.config.kind,
                                    true,
                                    self.sup.config.missed_pongs_to_kill,
                                ),
                                h.app.values().any(|c| c.phase == AppPhase::Loading),
                            )
                        };
                        if let Some(payload) = health_ev {
                            let _ = self
                                .sup
                                .events
                                .try_send(RuntimeEvent::HealthChanged(payload));
                        }
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
                            verdict => {
                                self.note_loop_stall(stale, app_loading);
                                match verdict {
                                    PongVerdict::StallWarn => self.sup.set_state(
                                        State::Unresponsive,
                                        format!("event loop blocked for {stale:?} (diagnostic)"),
                                    ),
                                    _ => self.sup.set_state(State::Ready, "pong received"),
                                }
                            }
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
                        // A blob.register rejection for this task
                        // already landed — the chunk's $blob would be
                        // unresolvable. End the stream with the reason
                        // and cancel the producer the way a client
                        // cancel would.
                        if let Some(rej) = self.blob_rejects.remove(&task) {
                            let _ = self.sup.events.try_send(RuntimeEvent::Stderr(format!(
                                "task {task}: blob.register rejected — ending the stream"
                            )));
                            // Ids that DID register since the last
                            // forwarded chunk never reach the page —
                            // release them before the error goes out.
                            self.release_pending_blobs(task);
                            if let Some(call_id) = self.tasks.get(&task).map(|m| m.call_id) {
                                if let Some(PendingKind::Stream(sink)) =
                                    self.pending.remove(&call_id)
                                {
                                    sink.finish(Err(self.blob_reject_err(rej)));
                                }
                                self.finish_task(call_id, task);
                            }
                            return ControlFlow::Continue(());
                        }
                        if self.sup.inspect.enabled() {
                            self.sup.inspect.note_chunk(task, &value);
                        }
                        // The ids this chunk ships — needed only when a
                        // pending registration exists; scanned before
                        // `value` moves into the sink.
                        let shipped = if self.blob_pending.contains_key(&task) {
                            let mut ids = Vec::new();
                            blob_ids_in(&value, 64, &mut ids);
                            ids
                        } else {
                            Vec::new()
                        };
                        let mut abandoned = None;
                        let mut delivered_chunk = false;
                        if let Some(meta) = self.tasks.get_mut(&task) {
                            // Each delivered chunk consumed one
                            // credit (RC.8A accounting).
                            meta.credit -= 1;
                            if let Some(PendingKind::Stream(sink)) = self.pending.get(&meta.call_id)
                            {
                                if let Err(why) = sink.chunk(seq, value) {
                                    abandoned = Some((meta.call_id, why));
                                } else {
                                    delivered_chunk = true;
                                }
                            }
                        }
                        if delivered_chunk {
                            // The tags it carries are page-owned now;
                            // any id still pending is an orphan —
                            // released inside the settle.
                            self.settle_shipped_blobs(task, &shipped);
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
                            // The stream is over (its terminal was just
                            // delivered): tombstone it like a finished task,
                            // so a later cancel says `completed` and a credit
                            // grant says "already finished", not `not_found`.
                            self.finish_task(call_id, task);
                        }
                    }
                    "app.component" => {
                        // App readiness telemetry (decision 0007 §3/§4).
                        // Bounded and best-effort: a malformed frame is
                        // dropped, never a reason to kill the runtime —
                        // the protocol-violation path stays reserved for
                        // frames that corrupt call/stream accounting.
                        let phase = match p["phase"].as_str() {
                            Some("loading") => Some(AppPhase::Loading),
                            Some("ready") => Some(AppPhase::Ready),
                            Some("failed") => Some(AppPhase::Failed),
                            _ => None,
                        };
                        let name = p["name"].as_str().unwrap_or("");
                        let progress = p["progress"]
                            .as_f64()
                            .filter(|x| x.is_finite() && (0.0..=1.0).contains(x));
                        let detail = p["detail"].as_str().map(str::to_string);
                        let ev = match phase {
                            Some(phase) if !name.is_empty() => self
                                .sup
                                .health
                                .lock()
                                .unwrap()
                                .note_app_component(name, phase, progress, detail),
                            _ => None,
                        };
                        if let Some(payload) = ev {
                            let _ = self.sup.events.try_send(RuntimeEvent::AppChanged(payload));
                        }
                    }
                    "blob.register" => {
                        // Owner binding (decision 0010 §2): the task_id in
                        // the payload only RESOLVES the owner — the label
                        // itself comes from TaskMeta.owner, never from
                        // Python or JS input. A register for a task this
                        // generation doesn't know (or an owner-less one)
                        // cannot name a window and is dropped. FIFO makes
                        // this land before the frame carrying the $blob.
                        let task = p["task_id"].as_u64();
                        let owner = task
                            .and_then(|t| self.tasks.get(&t))
                            .and_then(|m| m.owner.clone());
                        match owner {
                            Some(owner) => {
                                match crate::blobs::lock_blobs(&self.sup.blobs).register(&p, &owner)
                                {
                                    Ok(()) => {
                                        // Registered but not yet
                                        // shipped — released if the
                                        // carrying frame never makes
                                        // the page.
                                        if let (Some(t), Some(id)) = (task, p["id"].as_str()) {
                                            self.blob_pending
                                                .entry(t)
                                                .or_default()
                                                .push(id.to_string());
                                        }
                                    }
                                    Err(why) => {
                                        let _ = self.sup.events.try_send(RuntimeEvent::Stderr(
                                            format!(
                                                "pylair: blob.register {} rejected: {why:?}",
                                                p["id"].as_str().unwrap_or("?")
                                            ),
                                        ));
                                        // The rejection is the task's
                                        // problem: its carrying frame is
                                        // next on this FIFO, and that
                                        // frame's terminal (or next chunk)
                                        // delivers it as the error.
                                        if let Some(t) = task {
                                            self.blob_rejects.insert(t, why);
                                        }
                                    }
                                }
                            }
                            None => {
                                let _ = self.sup.events.try_send(RuntimeEvent::Stderr(
                                    format!(
                                        "pylair: blob.register {} for unknown/owner-less task — dropped",
                                        p["id"].as_str().unwrap_or("?")
                                    ),
                                ));
                            }
                        }
                    }
                    "blob.release" => {
                        // Backend-initiated release — drop + unlink. The
                        // runtime is trusted app code: no owner check here
                        // (the JS release path checks via release_owned).
                        let id = p["id"].as_str().unwrap_or_default();
                        crate::blobs::lock_blobs(&self.sup.blobs).release(id);
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
                let waited = timeout(Duration::from_secs(5), &mut self.io.exited).await;
                let detail = match &waited {
                    Ok(Ok(Ok(status))) => format!("exited {status}"),
                    _ => "runtime gone".into(),
                };
                fail_all(&mut self.pending, proto::RUNTIME_CRASHED, &detail).await;
                // No status within the wait: the runtime closed stdout but may
                // still be running. Kill and reap it so the next spawn cannot
                // overlap it (the exit channel is still pending only in this
                // case; once it has completed it must not be awaited again).
                if waited.is_err() {
                    kill_and_reap(&mut self.io).await;
                }
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
        // The command freed its queue slot the moment it was dequeued
        // (decisions 0005): release before any dispatch.
        release_queued(&self.sup.fair, &cmd);
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
