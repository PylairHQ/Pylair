use crate::{RuntimeKind, State};
use serde_json::{json, Value};

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

/// Validate the `url` of a web runtime's `ready` notification
/// (docs/protocol-v1.md section 4.1) and return it normalised.
///
/// The url is where the window will navigate, so it must be the app's own
/// loopback origin and nothing else: scheme `http`, host spelled exactly
/// `127.0.0.1`, an explicit non-zero port, no credentials, and (via
/// [`same_origin`]) the very origin `http://127.0.0.1:<port>`. Anything
/// else is a protocol violation by the runtime. Returns the reason on error.
pub(crate) fn validate_ready_url(params: Option<&serde_json::Value>) -> Result<String, String> {
    let raw = params
        .and_then(|p| p.get("url"))
        .and_then(|u| u.as_str())
        .ok_or("`ready` needs a string `url`")?;
    // A control character, whitespace or backslash never belongs in the url
    // the launcher sends. The url parser silently drops CR, LF and TAB and
    // reads a backslash as a slash, so a trailing CR that was JSON-escaped
    // and never decoded (backslash, then `r`, as the W0 spike hit on
    // Windows) would navigate to `/r`. Refuse it at the source instead of
    // normalising it.
    if raw
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(format!(
            "url {raw:?} contains a control character, whitespace or backslash"
        ));
    }
    // The parser would normalise `http://127.1:80` or `http://0x7f.1:80`
    // to the loopback address; the wire contract is the exact spelling.
    if !raw.starts_with("http://127.0.0.1:") {
        return Err(format!(
            "url {raw:?} is not http://127.0.0.1:<port>/ (loopback only)"
        ));
    }
    let url = tauri::Url::parse(raw).map_err(|e| format!("url {raw:?} does not parse: {e}"))?;
    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") {
        return Err(format!("url {raw:?} is not an http://127.0.0.1 url"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("url {raw:?} carries credentials"));
    }
    let port = match url.port() {
        Some(p) if p != 0 => p,
        _ => return Err(format!("url {raw:?} needs an explicit non-zero port")),
    };
    if !same_origin(url.as_str(), &format!("http://127.0.0.1:{port}")) {
        return Err(format!("url {raw:?} is not the origin 127.0.0.1:{port}"));
    }
    Ok(url.to_string())
}

/// The `pylair://state` payload. `url` is the web runtime's current ready
/// url and rides only on states in which a server is up (`ready`,
/// `unresponsive`); the key is absent otherwise, and always absent for the
/// bridge kind, whose payload is exactly `{from, to, detail}` as before.
pub(crate) fn state_payload(from: State, to: State, detail: &str, url: Option<&str>) -> Value {
    let mut p = json!({"from": fmt_state(from), "to": fmt_state(to), "detail": detail});
    if let (Some(url), State::Ready | State::Unresponsive) = (url, to) {
        p["url"] = json!(url);
    }
    p
}

/// The `python_state` answer: the bare state string for the bridge kind
/// (unchanged), `{state, url}` for a web runtime.
pub(crate) fn state_reply(kind: RuntimeKind, state: State, url: Option<String>) -> Value {
    match kind {
        RuntimeKind::Bridge => json!(fmt_state(state)),
        RuntimeKind::Web => json!({"state": fmt_state(state), "url": url}),
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

#[cfg(test)]
mod tests {
    use super::{state_payload, state_reply, validate_ready_url};
    use crate::{RuntimeKind, State};
    use serde_json::json;

    #[test]
    fn bridge_state_payload_and_reply_are_unchanged() {
        // No url key, ever, for the bridge kind (the host never has one).
        assert_eq!(
            state_payload(State::Starting, State::Ready, "handshake complete", None),
            json!({"from": "starting", "to": "ready", "detail": "handshake complete"})
        );
        assert_eq!(
            state_reply(RuntimeKind::Bridge, State::Ready, None),
            json!("ready")
        );
        // Even a stray url cannot leak into a bridge reply.
        assert_eq!(
            state_reply(RuntimeKind::Bridge, State::Ready, Some("http://x".into())),
            json!("ready")
        );
    }

    #[test]
    fn web_state_carries_the_url_only_while_a_server_is_up() {
        let u = Some("http://127.0.0.1:5000/");
        for (to, has) in [
            (State::Ready, true),
            (State::Unresponsive, true),
            (State::Starting, false),
            (State::Restarting, false),
            (State::Failed, false),
            (State::Stopped, false),
        ] {
            let p = state_payload(State::Starting, to, "d", u);
            assert_eq!(p.get("url").is_some(), has, "to={to:?}");
            if has {
                assert_eq!(p["url"], "http://127.0.0.1:5000/");
            }
        }
        assert_eq!(
            state_reply(RuntimeKind::Web, State::Ready, u.map(String::from)),
            json!({"state": "ready", "url": "http://127.0.0.1:5000/"})
        );
        assert_eq!(
            state_reply(RuntimeKind::Web, State::Restarting, None),
            json!({"state": "restarting", "url": null})
        );
    }

    fn check(url: serde_json::Value) -> Result<String, String> {
        validate_ready_url(Some(&json!({ "url": url })))
    }

    #[test]
    fn accepts_the_launcher_url_and_normalises_it() {
        assert_eq!(
            check(json!("http://127.0.0.1:51234/")).unwrap(),
            "http://127.0.0.1:51234/"
        );
        assert_eq!(
            check(json!("http://127.0.0.1:51234")).unwrap(),
            "http://127.0.0.1:51234/"
        );
        assert_eq!(
            check(json!("http://127.0.0.1:8501/app?x=1")).unwrap(),
            "http://127.0.0.1:8501/app?x=1"
        );
    }

    #[test]
    fn rejects_everything_that_is_not_the_exact_loopback_origin() {
        for bad in [
            "http://evil.example/",
            "http://evil.example:80/",
            "http://localhost:5000/",
            "http://[::1]:5000/",
            "http://127.0.0.2:5000/",
            "http://127.0.0.1/",
            "http://127.0.0.1:0/",
            "https://127.0.0.1:5000/",
            "ftp://127.0.0.1:5000/",
            "http://127.0.0.1@evil.com:5000/",
            "http://127.0.0.1:5000@evil.com/",
            "http://127.0.0.1:5000.evil.com/",
            "http://user:pw@127.0.0.1:5000/",
            "http://127.1:5000/",
            "http://0x7f.1:5000/",
            " http://127.0.0.1:5000/",
            "127.0.0.1:5000",
            "//127.0.0.1:5000/",
            "",
            "http://127.0.0.1:99999/",
            "javascript:alert(1)",
            // The W0 Windows bug: a CR on the end of the url.
            "http://127.0.0.1:5000\r",
            "http://127.0.0.1:5000/\r\n",
            "http://127.0.0.1:5000\t/",
            "http://127.0.0.1:5000\\r",
            "http://127.0.0.1:5000\\",
        ] {
            assert!(check(json!(bad)).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn rejects_missing_or_non_string_urls() {
        assert!(validate_ready_url(None).is_err());
        assert!(validate_ready_url(Some(&json!({}))).is_err());
        assert!(validate_ready_url(Some(&json!("http://127.0.0.1:1/"))).is_err());
        for bad in [json!(null), json!(5000), json!(["http://127.0.0.1:5000/"])] {
            assert!(check(bad).is_err());
        }
    }
}
