//! Per-window fairness accounting for the shared runtime limits
//! (docs/decisions/0005-per-window-fairness.md). This is the PR-1
//! scaffold: owners and the live set are counted and ceilings are
//! computable, but NOTHING is refused yet - every existing limit
//! outcome is unchanged.
//!
//! The map is shared Handle/actor under one `Mutex`, held only for
//! synchronous counter updates - never across an await:
//!
//! - `in_queue`: the Handle increments before `try_send` on
//!   Call/CallStream and the actor decrements at every `cmd_rx.recv()`
//!   site (`session::on_command`, `sleep_or_shutdown`, `park_failed`,
//!   `await_ready`) plus the `try_send` error path itself.
//! - `in_flight`: the actor increments at `tasks.insert` and
//!   decrements at every `tasks.remove`; `Session::drop` zeroes it
//!   because a generation's tasks die with it (teardown, not the next
//!   `Session::new`, so the dead generation's counts never leak into
//!   the restart backoff).
//! - live set: `WindowRegistry` joins on `subscribe`, leaves on
//!   `drop_label`. The `None` host bucket is live only while it holds
//!   work.
//!
//! Lock order: `WindowRegistry`'s mutex is held while this mutex is
//! taken (`subscribe`/`drop_label` call `join_window`/`leave_window`
//! under it); every other call site takes this mutex with no lock
//! held. Registry -> Fairness, never the reverse - and this mutex is
//! never held while awaiting or taking any other lock.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::handle::Command;

/// Shared Handle/actor owner map.
pub(crate) type FairnessRef = Arc<Mutex<Fairness>>;

/// Per-owner counters. An owner whose counters both reach 0 is pruned:
/// the map stays proportional to live work, and for the `None` bucket
/// an empty entry is exactly what removes it from the live set.
#[derive(Debug, Default)]
struct Counts {
    in_queue: u64,
    in_flight: u64,
}

/// Which counter feeds `in_use` for a ceiling check. Enforcement lands
/// in PR 2; today only the unit tests compute ceilings.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resource {
    /// Queued commands: an admission occupies a queue slot and then a
    /// task slot, so `in_use = in_queue + in_flight`.
    QueuedCommands,
    /// In-flight tasks (plain calls, streams, isolated workers).
    Tasks,
}

#[derive(Debug)]
pub(crate) struct Fairness {
    /// When false (`Limits.per_window_fairness` off) every mutator is a
    /// no-op, so disabling restores exactly today's behaviour.
    enabled: bool,
    owners: HashMap<Option<String>, Counts>,
    /// Live webview labels, maintained by `WindowRegistry` (join on
    /// `subscribe`, leave on `drop_label`).
    live_windows: HashSet<String>,
}

impl Fairness {
    pub(crate) fn new(enabled: bool) -> FairnessRef {
        Arc::new(Mutex::new(Fairness {
            enabled,
            owners: HashMap::new(),
            live_windows: HashSet::new(),
        }))
    }

    /// A webview invoked its first Pylair command (`WindowRegistry::subscribe`).
    pub(crate) fn join_window(&mut self, label: &str) {
        if self.enabled {
            self.live_windows.insert(label.to_string());
        }
    }

    /// The window is gone (`WindowRegistry::drop_label`).
    pub(crate) fn leave_window(&mut self, label: &str) {
        if self.enabled {
            self.live_windows.remove(label);
        }
    }

    /// Handle side: a Call/CallStream is about to `try_send`.
    pub(crate) fn admit_queue(&mut self, owner: &Option<String>) {
        if self.enabled {
            self.owners.entry(owner.clone()).or_default().in_queue += 1;
        }
    }

    /// The dequeued command freed its queue slot.
    pub(crate) fn release_queue(&mut self, owner: &Option<String>) {
        if self.enabled {
            // A release without an admission is an accounting bug; once
            // PR 2 enforces on these counts a hidden double release
            // would over-admit. Debug builds refuse to hide it.
            let Some(c) = self.owners.get_mut(owner) else {
                debug_assert!(false, "release_queue with no admission: {owner:?}");
                return;
            };
            debug_assert!(c.in_queue > 0, "in_queue double release: {owner:?}");
            c.in_queue = c.in_queue.saturating_sub(1);
            self.prune(owner);
        }
    }

    /// The actor registered a task (`tasks.insert`).
    pub(crate) fn note_started(&mut self, owner: &Option<String>) {
        if self.enabled {
            self.owners.entry(owner.clone()).or_default().in_flight += 1;
        }
    }

    /// The actor removed a task (`tasks.remove`) - finished, cancelled,
    /// reaped, or drained from a dead inbox.
    pub(crate) fn note_finished(&mut self, owner: &Option<String>) {
        if self.enabled {
            // A task leaving must have been counted in; a silent
            // underflow would over-admit under PR 2 enforcement.
            let Some(c) = self.owners.get_mut(owner) else {
                debug_assert!(false, "note_finished with no admission: {owner:?}");
                return;
            };
            debug_assert!(c.in_flight > 0, "in_flight double release: {owner:?}");
            c.in_flight = c.in_flight.saturating_sub(1);
            self.prune(owner);
        }
    }

    /// The generation's tasks all died with its `Session` map, so
    /// `in_flight` zeroes wholesale. Called at `Session::drop` -
    /// teardown, not the next spawn - so a dead generation never
    /// inflates `in_use` through the restart backoff: a stale count
    /// would shrink the crashed owner's own reservation (the unsafe
    /// direction for any check read before the next session exists).
    /// `in_queue` is untouched - a queued command still owes its release
    /// when it is eventually dequeued (normally or via fail-fast).
    pub(crate) fn end_generation(&mut self) {
        if !self.enabled {
            return;
        }
        for c in self.owners.values_mut() {
            c.in_flight = 0;
        }
        self.owners.retain(|_, c| c.in_queue > 0 || c.in_flight > 0);
    }

    /// Membership: a webview is live while subscribed; the host bucket
    /// (`None`) is live only while it holds work - a transient caller
    /// must not reserve capacity permanently.
    fn is_live(&self, owner: &Option<String>) -> bool {
        match owner {
            Some(label) => self.live_windows.contains(label),
            None => self
                .owners
                .get(&None)
                .is_some_and(|c| c.in_queue + c.in_flight > 0),
        }
    }

    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.live_windows.len() + usize::from(self.is_live(&None))
    }

    #[cfg(test)]
    fn in_use(&self, owner: &Option<String>, resource: Resource) -> usize {
        let Some(c) = self.owners.get(owner) else {
            return 0;
        };
        (match resource {
            Resource::QueuedCommands => c.in_queue + c.in_flight,
            Resource::Tasks => c.in_flight,
        }) as usize
    }

    /// `ceiling(owner, cap)` under the decisions-0005 formula: the cap
    /// minus, per OTHER live owner, `max(0, min_reserve - in_use)`.
    /// Used only by tests in PR 1 - enforcement lands in PR 2/3.
    #[cfg(test)]
    pub(crate) fn ceiling_for(
        &self,
        owner: &Option<String>,
        resource: Resource,
        cap: usize,
    ) -> usize {
        let others = self
            .owners
            .keys()
            .cloned()
            .chain(self.live_windows.iter().map(|l| Some(l.clone())))
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|o| o != owner && self.is_live(o))
            .map(|o| self.in_use(&o, resource));
        ceiling(cap, self.live_count(), others)
    }

    fn prune(&mut self, owner: &Option<String>) {
        if self
            .owners
            .get(owner)
            .is_some_and(|c| c.in_queue == 0 && c.in_flight == 0)
        {
            self.owners.remove(owner);
        }
    }

    /// Point-in-time copy of the map for integration tests, which see
    /// the supervisor only through `Handle` - PR 1's whole job is to
    /// prove the counting, so the map is observable.
    pub(crate) fn snapshot(&self) -> FairnessSnapshot {
        let mut owners: Vec<OwnerUsage> = Vec::new();
        let mut seen: HashSet<Option<String>> = HashSet::new();
        for (owner, c) in &self.owners {
            seen.insert(owner.clone());
            owners.push(OwnerUsage {
                owner: owner.clone(),
                in_queue: c.in_queue,
                in_flight: c.in_flight,
                live: self.is_live(owner),
            });
        }
        for label in &self.live_windows {
            let owner = Some(label.clone());
            if !seen.contains(&owner) {
                owners.push(OwnerUsage {
                    owner,
                    in_queue: 0,
                    in_flight: 0,
                    live: true,
                });
            }
        }
        owners.sort_by(|a, b| a.owner.cmp(&b.owner));
        FairnessSnapshot { owners }
    }
}

/// Release the queue slot a dequeued command held. Call at EVERY
/// `cmd_rx.recv()` site and on the `try_send` error path (the command
/// came back unsent, so the admission never happened). Call/CallStream
/// are the only variants that increment; everything else is a no-op.
pub(super) fn release_queued(fair: &FairnessRef, cmd: &Command) {
    let owner = match cmd {
        Command::Call { owner, .. } | Command::CallStream { owner, .. } => owner,
        _ => return,
    };
    fair.lock().unwrap().release_queue(owner);
}

/// The guaranteed minimum one live window keeps of a resource
/// (decisions 0005): a usable burst (an eighth of the cap, at least 1)
/// that costs an idle window far less than an equal split, and never
/// more than one. Past `live > cap` the equal split floors at 0 and
/// reservations collapse - the global cap then rules alone.
#[cfg(test)]
fn min_reserve(cap: usize, live: usize) -> usize {
    if live == 0 {
        return 0;
    }
    (cap / 8).max(1).min(cap / live)
}

/// `ceiling(A)`: `cap` minus, per OTHER live owner,
/// `max(0, min_reserve - in_use)` - a window's existing holdings are
/// its reservation, so a window past its minimum reserves nothing.
#[cfg(test)]
fn ceiling(cap: usize, live: usize, others_in_use: impl Iterator<Item = usize>) -> usize {
    let reserved: usize = others_in_use
        .map(|u| min_reserve(cap, live).saturating_sub(u))
        .sum();
    cap.saturating_sub(reserved)
}

/// One owner's counters in a [`FairnessSnapshot`]. `None` is the
/// host-side bucket.
#[doc(hidden)]
#[derive(Debug)]
pub struct OwnerUsage {
    pub owner: Option<String>,
    pub in_queue: u64,
    pub in_flight: u64,
    /// Membership at snapshot time: a window while subscribed, the
    /// host bucket only while it holds work.
    pub live: bool,
}

/// A point-in-time copy of the fairness map. Test/observability surface
/// only - PR 1 proves the counting through it; nothing reads it in the
/// serving path.
#[doc(hidden)]
#[derive(Debug)]
pub struct FairnessSnapshot {
    pub owners: Vec<OwnerUsage>,
}

impl FairnessSnapshot {
    /// `(in_queue, in_flight)` for `owner`; `(0, 0)` when absent.
    pub fn usage(&self, owner: Option<&str>) -> (u64, u64) {
        self.owners
            .iter()
            .find(|o| o.owner.as_deref() == owner)
            .map(|o| (o.in_queue, o.in_flight))
            .unwrap_or((0, 0))
    }

    /// Membership at snapshot time.
    pub fn is_live(&self, owner: Option<&str>) -> bool {
        self.owners
            .iter()
            .find(|o| o.owner.as_deref() == owner)
            .is_some_and(|o| o.live)
    }

    /// Windows subscribed, plus the host bucket if it holds work.
    pub fn live_count(&self) -> usize {
        self.owners.iter().filter(|o| o.live).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Fairness {
        Fairness {
            enabled: true,
            owners: HashMap::new(),
            live_windows: HashSet::new(),
        }
    }

    // ---- min_reserve / ceiling: every worked example in the doc ----

    #[test]
    fn min_reserve_values() {
        assert_eq!(
            min_reserve(64, 1),
            8,
            "the eighth rules until the split is smaller"
        );
        assert_eq!(min_reserve(64, 2), 8);
        assert_eq!(min_reserve(32, 2), 4);
        assert_eq!(min_reserve(8, 2), 1);
        assert_eq!(min_reserve(64, 8), 8);
        assert_eq!(min_reserve(64, 9), 7, "capped by the equal split");
        assert_eq!(min_reserve(64, 100), 0, "live > cap: reservations collapse");
        assert_eq!(min_reserve(4, 8), 0);
        assert_eq!(min_reserve(64, 0), 0);
    }

    #[test]
    fn settings_window_idle_costs_eight_not_half() {
        // doc example: settings subscribed, holds 0 -> main's ceiling is 56
        assert_eq!(ceiling(64, 2, [0].into_iter()), 56);
    }

    #[test]
    fn settings_window_in_flight_reserves_less() {
        // settings holds 1 -> reservation 7 -> main's ceiling is 57
        assert_eq!(ceiling(64, 2, [1].into_iter()), 57);
    }

    #[test]
    fn a_window_past_its_minimum_reserves_nothing() {
        // settings holds 8 (= min_reserve) -> main's ceiling is the cap
        assert_eq!(ceiling(64, 2, [8].into_iter()), 64);
        assert_eq!(ceiling(64, 2, [20].into_iter()), 64);
    }

    #[test]
    fn all_idle_never_locks_out() {
        // live = 8 all idle: ceiling(A) = 64 - 7*8 = 8 = min_reserve
        assert_eq!(ceiling(64, 8, [0; 7].into_iter()), 8);
    }

    #[test]
    fn live_past_cap_collapses_to_the_global_cap() {
        assert_eq!(ceiling(4, 8, [0; 7].into_iter()), 4);
        assert_eq!(ceiling(64, 100, [0; 99].into_iter()), 64);
    }

    #[test]
    fn single_live_window_gets_the_whole_cap() {
        assert_eq!(ceiling(64, 1, [].into_iter()), 64);
        assert_eq!(ceiling(32, 1, [].into_iter()), 32);
    }

    // ---- Fairness bookkeeping ----

    #[test]
    fn join_leave_live_set() {
        let mut f = on();
        assert!(!f.is_live(&Some("main".into())));
        f.join_window("main");
        assert!(f.is_live(&Some("main".into())));
        assert_eq!(f.live_count(), 1);
        f.leave_window("main");
        assert_eq!(f.live_count(), 0);
    }

    #[test]
    fn none_bucket_is_live_only_while_busy() {
        let mut f = on();
        assert!(!f.is_live(&None));
        f.admit_queue(&None);
        assert!(f.is_live(&None), "queued host work makes it live");
        f.release_queue(&None);
        f.note_started(&None);
        assert!(f.is_live(&None), "in-flight host work keeps it live");
        f.note_finished(&None);
        assert!(!f.is_live(&None), "idle again -> not live, pruned");
        assert!(!f.owners.contains_key(&None));
    }

    #[test]
    fn ceiling_for_counts_only_other_live_owners() {
        let mut f = on();
        f.join_window("main");
        f.join_window("settings");
        // settings holds nothing: main's task ceiling = 64 - 8 = 56
        let main = Some("main".to_string());
        assert_eq!(f.ceiling_for(&main, Resource::Tasks, 64), 56);
        // settings now holds 1 in flight: reservation 7 -> ceiling 57
        f.note_started(&Some("settings".to_string()));
        assert_eq!(f.ceiling_for(&main, Resource::Tasks, 64), 57);
        // a window that left does not reserve
        f.leave_window("settings");
        assert_eq!(f.ceiling_for(&main, Resource::Tasks, 64), 64);
        // and settings' ceiling counts main's reservation only while
        // main is under its minimum
        let settings = Some("settings".to_string());
        for _ in 0..56 {
            f.note_started(&main);
        }
        f.join_window("settings");
        assert_eq!(f.ceiling_for(&settings, Resource::Tasks, 64), 64);
    }

    #[test]
    fn queue_usage_includes_in_flight() {
        let mut f = on();
        f.join_window("a");
        f.join_window("b");
        f.admit_queue(&Some("a".into()));
        f.note_started(&Some("a".into()));
        // b's queue ceiling subtracts min_reserve - in_use(a) = 8 - 2 = 6
        assert_eq!(
            f.ceiling_for(&Some("b".into()), Resource::QueuedCommands, 64),
            58
        );
    }

    #[test]
    fn end_generation_zeroes_in_flight_only() {
        let mut f = on();
        f.admit_queue(&Some("a".into()));
        f.note_started(&Some("a".into()));
        f.end_generation();
        let (q, fl) = (
            f.owners[&Some("a".into())].in_queue,
            f.owners[&Some("a".into())].in_flight,
        );
        assert_eq!(
            (q, fl),
            (1, 0),
            "queue debt survives a generation; tasks do not"
        );
    }

    #[test]
    fn disabled_counts_nothing() {
        let mut f = Fairness {
            enabled: false,
            owners: HashMap::new(),
            live_windows: HashSet::new(),
        };
        f.join_window("a");
        f.admit_queue(&Some("a".into()));
        f.note_started(&Some("a".into()));
        assert_eq!(f.live_count(), 0);
        assert!(f.snapshot().owners.is_empty());
    }
}
