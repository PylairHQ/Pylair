//! Outgoing frame helpers for the data and control lanes (docs/protocol-v1.md
//! section 5.1, "Control frames have their own lane"), plus small pure
//! helpers the actor uses: ownership checks, cancel-ack mapping, restart
//! jitter and trigger-file mtime.

use std::time::Duration;

use serde_json::Value;

use super::handle::CancelOutcome;
use super::run::ChildIo;
use crate::protocol::ErrorObj;

pub(super) fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Why an outgoing frame could not be queued (RC.8A): the data lane is
/// saturated, or the frame exceeds `max_frame` — which would otherwise
/// trip the PEER's read cap and kill the whole runtime.
pub(super) enum SendFail {
    LaneFull,
    Oversize(usize),
}

/// Serialize `body` into a freshly-allocated frame WITH its 4-byte
/// length prefix, so the writer task issues one write per frame instead
/// of prefix+body as two unbuffered writes (docs/performance.md). The
/// prefix slot is reserved up front and patched once the length is
/// known — the serialized bytes never move.
pub(super) fn frame_bytes(body: &Value) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&[0; 4]);
    serde_json::to_writer(&mut buf, body).ok()?;
    let n = (buf.len() - 4) as u32;
    buf[..4].copy_from_slice(&n.to_be_bytes());
    Some(buf)
}

pub(super) fn send_frame(io: &ChildIo, body: &Value, max_frame: usize) -> Result<(), SendFail> {
    match frame_bytes(body) {
        Some(bytes) if bytes.len() - 4 > max_frame => Err(SendFail::Oversize(bytes.len() - 4)),
        Some(bytes) => io.frame_out.try_send(bytes).map_err(|_| SendFail::LaneFull),
        None => Err(SendFail::LaneFull),
    }
}

/// Control lane: cancel / credit / shutdown frames the writer drains before
/// data frames. Bounded — a failed try_send is a visible failure, not a drop.
/// Control frames are always small, but the size check is kept anyway so
/// no path can ever write a frame the peer would reject.
pub(super) fn send_ctrl(io: &ChildIo, body: &Value, max_frame: usize) -> bool {
    match frame_bytes(body) {
        Some(bytes) if bytes.len() - 4 <= max_frame => {
            io.ctrl_out.try_send(Ctrl::Now(bytes)).is_ok()
        }
        _ => false,
    }
}

/// Control lane with data-lane flush ordering: the writer first writes
/// every data frame committed before this one. Used for `shutdown` — a
/// call the host already accepted must reach the runtime ahead of the
/// shutdown, or the task never registers and the caller hears a generic
/// 'unavailable' instead of the drain's real terminal (rc.15: the
/// ctrl-lane initial grant made this reordering deterministic on
/// Windows).
pub(super) fn send_ctrl_flushed(io: &ChildIo, body: &Value, max_frame: usize) -> bool {
    match frame_bytes(body) {
        Some(bytes) if bytes.len() - 4 <= max_frame => {
            io.ctrl_out.try_send(Ctrl::FlushDataThen(bytes)).is_ok()
        }
        _ => false,
    }
}

/// One control-lane frame. Most control traffic overtakes queued data;
/// `FlushDataThen` additionally flushes committed data frames first.
pub(super) enum Ctrl {
    Now(Vec<u8>),
    FlushDataThen(Vec<u8>),
}

/// May `caller` control a task recorded with `owner`? Unowned tasks
/// (supervisor-direct callers, tests) are open to anyone; owned tasks only
/// to the exact webview label Tauri reported for the invoking window.
pub(super) fn owner_allows(owner: &Option<String>, caller: &Option<String>) -> bool {
    match owner {
        None => true,
        Some(o) => caller.as_deref() == Some(o.as_str()),
    }
}

/// Map the runtime's cancel ack — `{"cancelled": bool, "status"?, "reason"?}`
/// — to a distinct outcome. An error reply means the cancel itself failed.
pub(super) fn cancel_outcome(out: &Result<Value, ErrorObj>) -> CancelOutcome {
    match out {
        Ok(v) if v["cancelled"].as_bool().unwrap_or(false) => match v["status"].as_str() {
            Some("terminated") => CancelOutcome::Terminated,
            Some("may_continue") => CancelOutcome::RequestedMayContinue,
            Some("termination_in_progress") => CancelOutcome::TerminationUncertain,
            _ => CancelOutcome::Cancelled,
        },
        // cancelled:false — the task already finished, or the pending
        // buffer refused it. A cancel that merely outraced registration
        // comes back cancelled:true + status:"pending" instead.
        Ok(_) => CancelOutcome::Completed,
        Err(_) => CancelOutcome::TerminationUncertain,
    }
}

pub(super) fn jitter(d: Duration) -> Duration {
    // +/-25% jitter, deterministic-enough without an RNG dep
    let nanos = d.as_nanos() as u64;
    let j = nanos / 4;
    let r = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0))
        % (2 * j + 1);
    Duration::from_nanos(nanos - j + r)
}
