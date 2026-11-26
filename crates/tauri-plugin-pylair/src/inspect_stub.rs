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
    Error {
        code: i64,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        traceback: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        suggestions: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        errors: Option<Vec<Value>>,
    },
    Cancelled {
        verdict: Option<String>,
    },
    Limit {
        limit: Option<String>,
    },
}

/// Type parity with the debug timeline — constructed at the tap sites
/// and erased with the no-op methods. Never serialized in release.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TimelineEvent {
    Spawn {
        generation: u64,
        pid: Option<u32>,
    },
    Exit {
        generation: u64,
        detail: String,
    },
    Restart {
        generation: u64,
        detail: String,
    },
    Breaker {
        generation: u64,
        detail: String,
    },
    DevReload {
        generation: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
    WarmSpawn {
        function: String,
        n: u64,
        live: u64,
    },
    WarmDeath {
        function: String,
        n: u64,
    },
    WarmReap {
        function: String,
        n: u64,
    },
    StallWarn {
        detail: String,
    },
    StallRecovered {
        blocked_ms: u64,
    },
    Codegen {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Denial {
        command: String,
        window: String,
        origin: String,
        function: String,
        hint: String,
    },
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

    #[inline(always)]
    pub fn note_event(&self, _ev: TimelineEvent) {}

    #[inline(always)]
    pub fn note_stderr(&self, _generation: u64, _line: &str) {}

    #[inline(always)]
    pub fn note_codegen_status(&self, _raw: &str) {}

    pub fn snapshot(&self) -> Value {
        serde_json::json!({"enabled": false, "records": []})
    }
}

/// Never called with a live tap in release — provided for type parity.
pub fn classify(result: &Result<Value, crate::protocol::ErrorObj>) -> (Outcome, Option<&Value>) {
    match result {
        Ok(v) => (Outcome::Ok, Some(v)),
        Err(e) => (
            Outcome::Error {
                code: e.code,
                message: e.message.clone(),
                traceback: None,
                suggestions: None,
                errors: None,
            },
            None,
        ),
    }
}

/// Dead in release — the pong arm's diff feeds no-op methods.
pub fn diff_pools(
    _prev: &std::collections::BTreeMap<String, Value>,
    _next: &Value,
) -> Vec<TimelineEvent> {
    Vec::new()
}

/// Release: no inspector window ever exists, so this is never called.
pub const WINDOW_LABEL: &str = "pylair-inspector";
