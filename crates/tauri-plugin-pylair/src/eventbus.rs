//! Event delivery: lossless latest-state slots + a bounded bulk channel.
//!
//! State events (`StateChanged`, `HealthChanged`, `AppChanged`,
//! `Exposes`) must never be dropped: a webview that misses a readiness
//! transition stays "loading" forever. They are delivered through
//! latest-state slots — re-entries into the same state (or repeated
//! reports for the same component) may coalesce under a flood, but
//! distinct transitions and the NEWEST state always reach the drain,
//! no matter how far behind it falls. Chatty bulk
//! events (`Stderr`) keep the bounded lossy channel: a flooded log
//! drops lines, never states.
//!
//! Correctness lives in the sequence watermarks, not in the watch
//! channel's value semantics: every store increments a sequence inside
//! the slot lock, and the drain re-emits every entry newer than its
//! watermark in sequence order. The watch channel is purely a wakeup
//! mechanism, so no interleaving of store and drain can lose an entry.
//!
//! The supervisor never blocks: stores are a short lock + notify,
//! bulk sends stay `try_send`.

use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use tokio::sync::{mpsc, watch};

use crate::supervisor::{RuntimeEvent, State};

/// The latest state per kind. `seq` is a global monotone counter
/// incremented inside the slot lock on every store, so drain ordering
/// across kinds matches store order exactly.
#[derive(Clone, Default)]
pub struct SlotValue {
    seq: u64,
    /// Keyed by the TARGET state, not held as a single slot: distinct
    /// transitions are distinct facts (a dev restart's `Restarting` and
    /// the new generation's `Starting` fire microseconds apart and are
    /// both meaningful), while repeated entries into the SAME state —
    /// crash-loop churn — coalesce. Memory is bounded by the finite
    /// `State` enum, never by transition volume.
    state: BTreeMap<State, (u64, State, String)>,
    health: Option<(u64, Value)>,
    exposes: Option<(u64, Value)>,
    /// Per-component latest, keyed by `(generation, name)`. Entries of
    /// generations older than the newest reporting one are dropped at
    /// insert time — a dead generation's state is stale by definition,
    /// and the map stays bounded by one generation's component cap.
    app: BTreeMap<(u64, String), (u64, Value)>,
}

/// The producing side, cloned into everything that emits.
#[derive(Clone)]
pub struct EventBus {
    bulk: mpsc::Sender<RuntimeEvent>,
    slots: watch::Sender<SlotValue>,
}

/// Create the connected pair. `capacity` bounds the BULK channel only;
/// state slots are fixed-size by construction.
pub fn pair(capacity: usize) -> (EventBus, EventDrain) {
    let (bulk_tx, bulk_rx) = mpsc::channel(capacity);
    let (slots_tx, slots_rx) = watch::channel(SlotValue::default());
    (
        EventBus {
            bulk: bulk_tx,
            slots: slots_tx,
        },
        EventDrain {
            bulk: bulk_rx,
            slots: slots_rx,
            seen: 0,
            pending: VecDeque::new(),
            bulk_open: true,
            slots_open: true,
        },
    )
}

impl EventBus {
    /// Route an event: state kinds go to the lossless latest-state
    /// slots, everything else to the bounded bulk channel. Never blocks.
    pub fn emit(&self, ev: RuntimeEvent) {
        match ev {
            RuntimeEvent::StateChanged { from, to, detail } => {
                self.slots.send_modify(|s| {
                    s.seq += 1;
                    let seq = s.seq;
                    s.state.insert(to, (seq, from, detail));
                });
            }
            RuntimeEvent::HealthChanged(v) => {
                self.slots.send_modify(|s| {
                    s.seq += 1;
                    let seq = s.seq;
                    s.health = Some((seq, v));
                });
            }
            RuntimeEvent::Exposes(v) => {
                self.slots.send_modify(|s| {
                    s.seq += 1;
                    let seq = s.seq;
                    s.exposes = Some((seq, v));
                });
            }
            RuntimeEvent::AppChanged(v) => {
                let generation = v["generation"].as_u64().unwrap_or(0);
                let name = v["name"].as_str().unwrap_or("").to_string();
                self.slots.send_modify(|s| {
                    s.seq += 1;
                    let seq = s.seq;
                    // Superseded generations drop out: their state is
                    // stale the moment a newer generation reports, and
                    // the map never outgrows one generation's cap.
                    s.app.retain(|(g, _), _| *g >= generation);
                    s.app.insert((generation, name), (seq, v));
                });
            }
            other => {
                let _ = self.bulk.try_send(other);
            }
        }
    }

    /// Backpressured bulk send for the stderr forwarder: runtime stderr
    /// is queue-drop-marked upstream (protocol §9), so the channel must
    /// apply backpressure rather than drop lines silently. This is the
    /// only awaiting send in the bus, and only the dedicated forwarder
    /// task calls it — never the supervisor actor.
    pub(crate) async fn send_bulk(
        &self,
        ev: RuntimeEvent,
    ) -> Result<(), mpsc::error::SendError<RuntimeEvent>> {
        self.bulk.send(ev).await
    }
}

/// The consuming side: one logical stream of events, bulk interleaved
/// with losslessly-delivered state. `recv` yields state events in
/// store order (sequence order) and never terminates early while
/// either channel is open.
pub struct EventDrain {
    bulk: mpsc::Receiver<RuntimeEvent>,
    slots: watch::Receiver<SlotValue>,
    seen: u64,
    pending: VecDeque<RuntimeEvent>,
    bulk_open: bool,
    slots_open: bool,
}

impl EventDrain {
    /// Receive the next event, or `None` once both channels are closed
    /// and every queued state entry has been delivered.
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if !self.bulk_open && !self.slots_open {
                return None;
            }
            // A state change may already be pending without a fresh
            // wakeup in flight (it landed while bulk events were being
            // yielded): collect before parking.
            if self.collect_slots() {
                continue;
            }
            tokio::select! {
                // State first under contention: a bulk flood must not
                // starve the slots, or the flood itself would delay the
                // very transitions it is not allowed to drop.
                biased;
                changed = self.slots.changed(), if self.slots_open => {
                    if changed.is_err() {
                        self.slots_open = false;
                    }
                    self.collect_slots();
                }
                ev = self.bulk.recv(), if self.bulk_open => {
                    match ev {
                        Some(e) => return Some(e),
                        None => self.bulk_open = false,
                    }
                }
            }
        }
    }

    /// Non-blocking receive, mirroring `mpsc::Receiver::try_recv` —
    /// test drains use it to harvest everything currently delivered.
    pub fn try_recv(&mut self) -> Result<RuntimeEvent, mpsc::error::TryRecvError> {
        if let Some(ev) = self.pending.pop_front() {
            return Ok(ev);
        }
        self.collect_slots();
        if let Some(ev) = self.pending.pop_front() {
            return Ok(ev);
        }
        self.bulk.try_recv()
    }

    /// Move every slot entry newer than the watermark into `pending`
    /// in sequence order. Returns true if anything was collected.
    fn collect_slots(&mut self) -> bool {
        let slots = self.slots.borrow_and_update();
        let mut batch: Vec<(u64, RuntimeEvent)> = Vec::new();
        for (to, (seq, from, detail)) in slots.state.iter() {
            if *seq > self.seen {
                batch.push((
                    *seq,
                    RuntimeEvent::StateChanged {
                        from: *from,
                        to: *to,
                        detail: detail.clone(),
                    },
                ));
            }
        }
        if let Some((seq, v)) = &slots.health {
            if *seq > self.seen {
                batch.push((*seq, RuntimeEvent::HealthChanged(v.clone())));
            }
        }
        if let Some((seq, v)) = &slots.exposes {
            if *seq > self.seen {
                batch.push((*seq, RuntimeEvent::Exposes(v.clone())));
            }
        }
        for (seq, v) in slots.app.values() {
            if *seq > self.seen {
                batch.push((*seq, RuntimeEvent::AppChanged(v.clone())));
            }
        }
        if batch.is_empty() {
            return false;
        }
        batch.sort_by_key(|(seq, _)| *seq);
        if let Some((max_seq, _)) = batch.last() {
            self.seen = *max_seq;
        }
        for (_, ev) in batch {
            self.pending.push_back(ev);
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app_ev(generation: u64, name: &str, phase: &str) -> RuntimeEvent {
        RuntimeEvent::AppChanged(json!({
            "version": 1,
            "generation": generation,
            "name": name,
            "phase": phase,
        }))
    }

    /// The guarantee the whole bus exists for: flood the bounded bulk
    /// channel far past capacity, then change state — the drain must
    /// observe the final state even though most bulk lines dropped.
    #[tokio::test]
    async fn state_survives_a_bulk_flood() {
        let (bus, mut drain) = pair(8);
        for i in 0..200 {
            bus.emit(RuntimeEvent::Stderr(format!("flood-{i}")));
        }
        bus.emit(RuntimeEvent::AppChanged(json!({
            "version": 1, "generation": 0, "name": "db", "phase": "loading",
        })));
        bus.emit(RuntimeEvent::AppChanged(json!({
            "version": 1, "generation": 0, "name": "db", "phase": "ready",
        })));
        bus.emit(RuntimeEvent::StateChanged {
            from: State::Starting,
            to: State::Ready,
            detail: "hello".into(),
        });
        for i in 200..400 {
            bus.emit(RuntimeEvent::Stderr(format!("flood-{i}")));
        }

        let mut app_phases = Vec::new();
        let mut states = Vec::new();
        let mut bulk = 0usize;
        while let Ok(ev) = drain.try_recv() {
            match ev {
                RuntimeEvent::AppChanged(v) => {
                    app_phases.push(v["phase"].as_str().unwrap().to_string())
                }
                RuntimeEvent::StateChanged { to, .. } => states.push(to),
                RuntimeEvent::Stderr(_) => bulk += 1,
                _ => {}
            }
        }
        // The newest state is always delivered…
        assert_eq!(app_phases.last().unwrap(), "ready");
        assert!(states.contains(&State::Ready));
        // …and the bulk channel really did overflow (lossy by design),
        // proving the state events did not depend on it.
        assert!(bulk <= 8, "bulk must stay bounded, got {bulk}");
    }

    /// Every distinct component's latest state survives the tightest
    /// possible flood — state is per-component, never a single slot.
    #[tokio::test]
    async fn every_component_survives_back_to_back_stores() {
        let (bus, mut drain) = pair(1);
        for i in 0..40 {
            bus.emit(app_ev(0, &format!("flood{i}"), "loading"));
        }
        let mut names = std::collections::HashSet::new();
        while let Ok(ev) = drain.try_recv() {
            if let RuntimeEvent::AppChanged(v) = ev {
                names.insert(v["name"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(names.len(), 40, "every component must arrive: {names:?}");
    }

    /// Same-component transitions may coalesce, but the newest one
    /// always lands; ordering across kinds follows store order.
    #[tokio::test]
    async fn coalescing_keeps_the_newest_in_order() {
        let (bus, mut drain) = pair(1);
        bus.emit(app_ev(0, "db", "loading"));
        bus.emit(RuntimeEvent::StateChanged {
            from: State::Starting,
            to: State::Ready,
            detail: String::new(),
        });
        bus.emit(app_ev(0, "db", "ready"));
        let mut kinds = Vec::new();
        while let Ok(ev) = drain.try_recv() {
            kinds.push(match ev {
                RuntimeEvent::AppChanged(v) => format!("app:{}", v["phase"].as_str().unwrap()),
                RuntimeEvent::StateChanged { .. } => "state".to_string(),
                _ => "other".to_string(),
            });
        }
        // "ready" is always the last word on the component, and it
        // never arrives before the state change that preceded it.
        let ready_pos = kinds.iter().position(|k| k == "app:ready").unwrap();
        let state_pos = kinds.iter().position(|k| k == "state").unwrap();
        assert!(state_pos < ready_pos, "{kinds:?}");
        assert_eq!(kinds.last().unwrap(), "app:ready");
    }

    /// The dev-restart race: `Restarting` and the new generation's
    /// `Starting` fire microseconds apart. Distinct target states are
    /// distinct facts — both must survive even with no drain in between.
    #[tokio::test]
    async fn distinct_transitions_never_coalesce() {
        let (bus, mut drain) = pair(1);
        bus.emit(RuntimeEvent::StateChanged {
            from: State::Ready,
            to: State::Restarting,
            detail: "source changed".into(),
        });
        bus.emit(RuntimeEvent::StateChanged {
            from: State::Restarting,
            to: State::Starting,
            detail: "spawning (gen 1)".into(),
        });
        bus.emit(RuntimeEvent::StateChanged {
            from: State::Starting,
            to: State::Ready,
            detail: "handshake complete".into(),
        });
        let mut targets = Vec::new();
        while let Ok(ev) = drain.try_recv() {
            if let RuntimeEvent::StateChanged { to, .. } = ev {
                targets.push(to);
            }
        }
        assert_eq!(
            targets,
            vec![State::Restarting, State::Starting, State::Ready],
            "every distinct transition must arrive in order: {targets:?}"
        );
    }

    /// Repeated entries into the SAME state are versions of one fact:
    /// they coalesce, and the newest detail wins — crash-loop churn can
    /// never flood the drain.
    #[tokio::test]
    async fn same_state_reentry_coalesces_to_the_newest() {
        let (bus, mut drain) = pair(1);
        for gen in 1..=50 {
            bus.emit(RuntimeEvent::StateChanged {
                from: State::Restarting,
                to: State::Starting,
                detail: format!("spawning (gen {gen})"),
            });
        }
        let mut details = Vec::new();
        while let Ok(ev) = drain.try_recv() {
            if let RuntimeEvent::StateChanged { detail, .. } = ev {
                details.push(detail);
            }
        }
        assert_eq!(
            details,
            vec!["spawning (gen 50)".to_string()],
            "{details:?}"
        );
    }

    /// A newer generation's first report retires the older generation's
    /// undelivered entries: stale state is never delivered, and the map
    /// stays bounded by one generation.
    #[tokio::test]
    async fn newer_generation_retires_undelivered_older_state() {
        let (bus, mut drain) = pair(1);
        bus.emit(app_ev(0, "db", "loading"));
        bus.emit(app_ev(0, "cache", "loading"));
        bus.emit(app_ev(1, "db", "loading"));
        let mut seen = Vec::new();
        while let Ok(ev) = drain.try_recv() {
            if let RuntimeEvent::AppChanged(v) = ev {
                seen.push((
                    v["generation"].as_u64().unwrap(),
                    v["name"].as_str().unwrap().to_string(),
                ));
            }
        }
        assert_eq!(seen, vec![(1, "db".to_string())], "{seen:?}");
    }

    /// A bus that outlives its drain (or never had one listening yet)
    /// must never panic or block — emission is unconditional.
    #[tokio::test]
    async fn emit_with_no_receiver_never_panics() {
        let (bus, drain) = pair(1);
        drop(drain);
        bus.emit(app_ev(0, "db", "ready"));
        bus.emit(RuntimeEvent::StateChanged {
            from: State::Starting,
            to: State::Ready,
            detail: String::new(),
        });
        bus.emit(RuntimeEvent::Stderr("line".into()));
    }

    /// Closing the bus ends the drain — but only after everything
    /// already stored has been delivered.
    #[tokio::test]
    async fn close_delivers_everything_then_ends() {
        let (bus, mut drain) = pair(1);
        bus.emit(app_ev(0, "db", "ready"));
        bus.emit(RuntimeEvent::Stderr("last line".into()));
        drop(bus);
        let mut got_app = false;
        let mut got_stderr = false;
        while let Some(ev) = drain.recv().await {
            match ev {
                RuntimeEvent::AppChanged(_) => got_app = true,
                RuntimeEvent::Stderr(_) => got_stderr = true,
                _ => {}
            }
        }
        assert!(got_app && got_stderr);
    }
}
