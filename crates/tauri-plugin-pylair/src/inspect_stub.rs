//! Release-build shell for `inspect.rs` (docs/inspector.md): in a
//! release build the inspector is not compiled — this zero-sized type
//! keeps the call sites compiling, and every method is an inline no-op
//! the optimizer erases wholesale. There is no ring, no page, no window.

use serde_json::Value;

/// The outcome vocabulary, type-compatible with the debug tap so the
/// session code compiles unchanged. Never constructed in release.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error { code: i64 },
    Cancelled { verdict: Option<String> },
    Limit { limit: Option<String> },
}

#[derive(Clone, Default)]
pub struct InspectRef;

impl InspectRef {
    pub fn new(_enabled: bool, _payloads: bool) -> Self {
        InspectRef
    }

    #[inline(always)]
    pub fn enabled(&self) -> bool {
        false
    }

    #[inline(always)]
    pub fn note_start(
        &self,
        _task: u64,
        _fn_name: &str,
        _window: Option<&str>,
        _mode: &'static str,
        _input: &Value,
        _is_stream: bool,
    ) {
    }

    #[inline(always)]
    pub fn note_chunk(&self, _task: u64, _value: &Value) {}

    #[inline(always)]
    pub fn note_end(&self, _task: u64, _outcome: Outcome, _resp: Option<&Value>) {}

    pub fn snapshot(&self) -> Value {
        serde_json::json!({"enabled": false, "records": []})
    }
}

/// Never called with a live tap in release — provided for type parity.
pub fn classify(result: &Result<Value, crate::protocol::ErrorObj>) -> (Outcome, Option<&Value>) {
    match result {
        Ok(v) => (Outcome::Ok, Some(v)),
        Err(e) => (Outcome::Error { code: e.code }, None),
    }
}

/// Release: no inspector window ever exists, so this is never called.
pub const WINDOW_LABEL: &str = "pylair-inspector";
