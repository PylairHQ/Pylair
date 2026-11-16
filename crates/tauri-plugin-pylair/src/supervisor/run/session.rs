//! The steady-state half of `run_child`: `Session` holds the per-generation
//! bookkeeping and the methods the `select!` arms call.

use std::collections::{HashMap, HashSet};

use super::{ChildIo, PendingKind, Supervisor, TaskMeta};

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

impl Session<'_> {}
