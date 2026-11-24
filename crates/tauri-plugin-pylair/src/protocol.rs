//! Protocol v1 message types and error codes (docs/protocol-v1.md).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// Default max frame body size; overridable via PYLAIR_MAX_FRAME (bytes).
pub const DEFAULT_MAX_FRAME: usize = 16 * 1024 * 1024;

pub fn max_frame() -> usize {
    std::env::var("PYLAIR_MAX_FRAME")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_FRAME)
}

// ---- error codes (docs/protocol-v1.md §8) ----
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const FUNCTION_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32000;
pub const TASK_NOT_FOUND: i64 = -32001;
pub const TASK_CANCELLED: i64 = -32010;
pub const WORKER_DIED: i64 = -32011;
pub const RUNTIME_WEDGED: i64 = -32012;
pub const RUNTIME_UNAVAILABLE: i64 = -32030;
pub const RUNTIME_CRASHED: i64 = -32031;
pub const PROTOCOL_VIOLATION: i64 = -32032;
/// In-flight call killed by a dev-mode hot reload — not a crash; the UI
/// can present it as "restarting" rather than an error.
pub const RUNTIME_RESTARTED: i64 = -32033;
/// A configured limit was hit — `data.limit` names it (RC.8A).
pub const LIMIT_EXCEEDED: i64 = -32040;
/// The invoking window's capability scope does not permit this function
/// (docs/security.md, per-function permissions). Raised host-side before the
/// supervisor is touched; `data` carries `fn` and `window`.
pub const PERMISSION_DENIED: i64 = -32050;

/// Update machinery failure (decision 0011): the stable machine code
/// travels in `data.code` ("NotUpdatable", "NeedsApproval", ...).
pub const UPDATE_FAILED: i64 = -32060;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObj {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A message from runtime -> host.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InMsg {
    /// Response to a request we sent: has `id`, no `method`.
    Response {
        id: u64,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<ErrorObj>,
    },
    /// Notification: has `method`, no `id`.
    Notification {
        method: String,
        params: Option<Value>,
    },
}

impl InMsg {
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Build a host -> runtime request frame body.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Build a host -> runtime notification frame body.
pub fn notification(method: &str, params: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_response() {
        let m = InMsg::parse(br#"{"jsonrpc":"2.0","id":3,"result":{"x":1}}"#).unwrap();
        assert!(matches!(m, InMsg::Response { id: 3, .. }));
    }

    #[test]
    fn parses_error_response() {
        let m = InMsg::parse(
            br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32010,"message":"task cancelled"}}"#,
        )
        .unwrap();
        match m {
            InMsg::Response { error: Some(e), .. } => assert_eq!(e.code, TASK_CANCELLED),
            _ => panic!("expected error response"),
        }
    }

    #[test]
    fn parses_notification() {
        let m = InMsg::parse(
            br#"{"jsonrpc":"2.0","method":"stream.chunk","params":{"task":1,"seq":0,"value":"t"}}"#,
        )
        .unwrap();
        assert!(matches!(m, InMsg::Notification { ref method, .. } if method == "stream.chunk"));
    }
}
