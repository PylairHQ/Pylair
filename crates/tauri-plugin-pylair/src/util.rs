use crate::State;

/// Exact-origin check for wrap mode (P1-8): two URLs share an origin iff
/// scheme, host and effective port are equal — parse, never prefix-match.
/// `http://127.0.0.1:5000.evil.com` (fails to parse as the expected host)
/// and `http://127.0.0.1@evil.com` (userinfo tricks) are NOT the app origin.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (tauri::Url::parse(a), tauri::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

pub(super) fn fmt_state(s: State) -> &'static str {
    match s {
        State::Starting => "starting",
        State::Ready => "ready",
        State::Unresponsive => "unresponsive",
        State::Restarting => "restarting",
        State::Failed => "failed",
        State::Stopped => "stopped",
    }
}
