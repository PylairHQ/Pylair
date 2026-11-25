//! Dev-only live inspector (docs/inspector.md): a bounded in-memory
//! record of the most recent calls, tapped in the supervisor at call
//! start and end. Compiled ONLY into debug builds (`cfg(debug_assertions)`
//! â€” see `lib.rs`); the release build carries a zero-sized stub whose
//! methods inline to nothing, so no inspector code exists in release.
//!
//! Even in debug builds the tap is inert unless `PYLAIR_INSPECT=1` is in
//! the environment at plugin setup â€” with the flag off every method is a
//! no-op behind one bool check. Payload previews are a second opt-in
//! (`PYLAIR_INSPECT_PAYLOADS=1`).
//!
//! Hot-path rules: recording never touches the bounded events channel
//! (the AppProgress lesson â€” a full channel must not drop real events),
//! never blocks (a contended lock drops the sample rather than waiting),
//! and never allocates unboundedly â€” the ring is fixed-capacity and the
//! in-flight map is bounded by `max_concurrent_tasks` plus a hard cap.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Default ring capacity â€” the last N completed calls.
pub const DEFAULT_CAPACITY: usize = 1000;
/// Hard cap on in-flight entries: normal operation is bounded by
/// `max_concurrent_tasks`, but a lost start/end pair must never grow
/// memory â€” past the cap the oldest in-flight entries are dropped.
const MAX_IN_FLIGHT: usize = 4096;
/// Payload preview truncation (chars) â€” docs/inspector.md Â§payloads.
pub const PREVIEW_CHARS: usize = 512;

/// Shared with the `Session` bookkeeping paths â€” the `HealthRef`
/// precedent. `None`-empty when the flag is off.
#[derive(Clone)]
pub struct InspectRef {
    /// Fast-path flag mirrored out of the mutex — the flag-off hot path
    /// is one atomic load, no lock at all.
    enabled: Arc<std::sync::atomic::AtomicBool>,
    inner: Arc<Mutex<Option<Inner>>>,
}

struct Inner {
    ring: VecDeque<Record>,
    capacity: usize,
    in_flight: HashMap<u64, InFlight>,
    /// In-flight insertion order for the hard-cap eviction.
    in_flight_order: VecDeque<u64>,
    payloads: bool,
    /// Monotonic row id, survives ring eviction.
    next_seq: u64,
}

struct InFlight {
    seq: u64,
    fn_name: String,
    window: Option<String>,
    mode: &'static str,
    start: Instant,
    start_unix_ms: u64,
    req_bytes: u64,
    req_preview: Option<String>,
    is_stream: bool,
    chunks: u64,
    chunk_bytes: u64,
    first_chunk_ms: Option<u64>,
    last_chunk_ms: Option<u64>,
}

/// One finished call â€” the inspector table row. Serialization shape is
/// the inspector page's contract (docs/inspector.md).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Record {
    /// Sequence number â€” monotonically increasing per runtime, lets the
    /// page merge polls without re-rendering rows it already drew.
    pub seq: u64,
    /// The supervisor task id.
    pub task: u64,
    pub function: String,
    /// The owning window label; `null` for unattributed calls.
    pub window: Option<String>,
    /// `inline` | `isolated` | `warm`.
    pub mode: &'static str,
    /// Wall-clock start, unix milliseconds.
    pub start_unix_ms: u64,
    pub latency_ms: u64,
    pub outcome: Outcome,
    pub req_bytes: u64,
    pub resp_bytes: u64,
    /// Streams only: chunks forwarded and the ms between the first and
    /// the last chunk (0 for a single chunk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamStats>,
    /// `PYLAIR_INSPECT_PAYLOADS=1` only: truncated JSON previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub req_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resp_preview: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamStats {
    pub chunks: u64,
    pub chunk_bytes: u64,
    pub stream_ms: u64,
}

/// How the call ended. `cancelled` carries the cancel verdict the wire
/// reported; `limit` carries the `data.limit` cap name.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error { code: i64 },
    Cancelled { verdict: Option<String> },
    Limit { limit: Option<String> },
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `$b64` never ships its bytes into a preview (docs/inspector.md): the
/// tag object is replaced by a size marker. Depth-capped like the blob
/// scan â€” anything deeper is simply not rewritten.
fn sanitize_b64(v: &mut Value, depth: usize) {
    if depth == 0 {
        return;
    }
    match v {
        Value::Object(map) => {
            if let Some(b64) = map.get("$b64").and_then(Value::as_str) {
                // base64 expands 3 bytes into 4 chars.
                let bytes = b64.len() / 4 * 3;
                *v = serde_json::json!({"$b64": format!("<{bytes} bytes>")});
                return;
            }
            for x in map.values_mut() {
                sanitize_b64(x, depth - 1);
            }
        }
        Value::Array(xs) => {
            for x in xs.iter_mut() {
                sanitize_b64(x, depth - 1);
            }
        }
        _ => {}
    }
}

/// A truncated JSON preview of a call payload â€” never blob bytes.
fn preview(v: &Value) -> String {
    let mut v = v.clone();
    sanitize_b64(&mut v, 64);
    let s = v.to_string();
    if s.chars().count() <= PREVIEW_CHARS {
        s
    } else {
        let cut: String = s.chars().take(PREVIEW_CHARS).collect();
        format!("{cut}â€¦")
    }
}

impl InspectRef {
    /// The shared tap. `enabled` is the `PYLAIR_INSPECT=1` runtime flag,
    /// `payloads` the second opt-in â€” both resolved once at plugin setup.
    pub fn new(enabled: bool, payloads: bool) -> Self {
        InspectRef {
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(enabled)),
            inner: Arc::new(Mutex::new(if enabled {
                Some(Inner {
                    ring: VecDeque::new(),
                    capacity: DEFAULT_CAPACITY,
                    in_flight: HashMap::new(),
                    in_flight_order: VecDeque::new(),
                    payloads,
                    next_seq: 1,
                })
            } else {
                None
            })),
        }
    }

    /// The no-flag fast path: one lock, one bool. Everything below early-
    /// returns through this.
    pub fn enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Call start â€” the supervisor committed the task to the wire.
    /// `mode` is `inline`/`isolated`/`warm` as derived from the hello
    /// manifest. Never blocks: a contended lock drops the sample.
    #[allow(clippy::too_many_arguments)]
    pub fn note_start(
        &self,
        task: u64,
        fn_name: &str,
        window: Option<&str>,
        mode: &'static str,
        input: &Value,
        is_stream: bool,
    ) {
        if !self.enabled() {
            return;
        }
        let Ok(mut guard) = self.inner.try_lock() else {
            return;
        };
        let Some(inner) = guard.as_mut() else {
            return;
        };
        // Hard cap: drop the oldest in-flight entries. A dropped entry
        // means the end will record nothing â€” dev tooling tolerates the
        // hole; memory does not grow.
        while inner.in_flight.len() >= MAX_IN_FLIGHT {
            let Some(oldest) = inner.in_flight_order.pop_front() else {
                break;
            };
            inner.in_flight.remove(&oldest);
        }
        let seq = inner.ring.back().map(|r| r.seq + 1).unwrap_or(1);
        // seq must survive ring eviction â€” track it separately.
        let seq = seq.max(inner.in_flight.values().map(|f| f.seq + 1).max().unwrap_or(1));
        let _ = seq;
        let seq = {
            inner.next_seq
        };
        inner.next_seq += 1;
        inner.in_flight_order.push_back(task);
        inner.in_flight.insert(
            task,
            InFlight {
                seq,
                fn_name: fn_name.to_string(),
                window: window.map(str::to_string),
                mode,
                start: Instant::now(),
                start_unix_ms: unix_ms(),
                // `to_string` length approximates the wire bytes closely
                // enough for a dev tool; computed only when recording.
                req_bytes: input.to_string().len() as u64,
                req_preview: if inner.payloads {
                    Some(preview(input))
                } else {
                    None
                },
                is_stream,
                chunks: 0,
                chunk_bytes: 0,
                first_chunk_ms: None,
                last_chunk_ms: None,
            },
        );
    }

    /// A stream chunk was forwarded for `task` â€” count it.
    pub fn note_chunk(&self, task: u64, value: &Value) {
        let Ok(mut guard) = self.inner.try_lock() else {
            return;
        };
        let Some(inner) = guard.as_mut() else {
            return;
        };
        let Some(f) = inner.in_flight.get_mut(&task) else {
            return;
        };
        let ms = f.start.elapsed().as_millis() as u64;
        if f.first_chunk_ms.is_none() {
            f.first_chunk_ms = Some(ms);
        }
        f.last_chunk_ms = Some(ms);
        f.chunks += 1;
        f.chunk_bytes += value.to_string().len() as u64;
    }

    /// Call end â€” terminal outcome known. `outcome` classification
    /// happens at the tap call site (it has the wire error in hand).
    pub fn note_end(&self, task: u64, outcome: Outcome, resp: Option<&Value>) {
        let Ok(mut guard) = self.inner.try_lock() else {
            return;
        };
        let Some(inner) = guard.as_mut() else {
            return;
        };
        let Some(f) = inner.in_flight.remove(&task) else {
            return;
        };
        inner.in_flight_order.retain(|t| *t != task);
        let stream = if f.is_stream {
            Some(StreamStats {
                chunks: f.chunks,
                chunk_bytes: f.chunk_bytes,
                stream_ms: match (f.first_chunk_ms, f.last_chunk_ms) {
                    (Some(a), Some(b)) => b.saturating_sub(a),
                    _ => 0,
                },
            })
        } else {
            None
        };
        let resp_bytes = resp.map(|v| v.to_string().len() as u64).unwrap_or(0);
        let resp_preview = if inner.payloads {
            resp.map(preview)
        } else {
            None
        };
        if inner.ring.len() >= inner.capacity {
            inner.ring.pop_front();
        }
        inner.ring.push_back(Record {
            seq: f.seq,
            task,
            function: f.fn_name,
            window: f.window,
            mode: f.mode,
            start_unix_ms: f.start_unix_ms,
            latency_ms: f.start.elapsed().as_millis() as u64,
            outcome,
            req_bytes: f.req_bytes,
            resp_bytes,
            stream,
            req_preview: f.req_preview,
            resp_preview,
        });
    }

    /// The inspector snapshot: every recorded call plus the still-open
    /// in-flight rows (so the table shows long-running calls live).
    pub fn snapshot(&self) -> Value {
        let Ok(guard) = self.inner.lock() else {
            return serde_json::json!({"enabled": false, "records": []});
        };
        let Some(inner) = guard.as_ref() else {
            return serde_json::json!({"enabled": false, "records": []});
        };
        let done: Vec<&Record> = inner.ring.iter().collect();
        let open: Vec<Value> = inner
            .in_flight
            .values()
            .map(|f| {
                serde_json::json!({
                    "seq": f.seq,
                    "task": 0,
                    "function": f.fn_name,
                    "window": f.window,
                    "mode": f.mode,
                    "start_unix_ms": f.start_unix_ms,
                    "latency_ms": f.start.elapsed().as_millis() as u64,
                    "outcome": {"kind": "in_flight"},
                    "req_bytes": f.req_bytes,
                    "resp_bytes": 0,
                    "stream": if f.is_stream {
                        Some(serde_json::json!({
                            "chunks": f.chunks,
                            "chunk_bytes": f.chunk_bytes,
                            "stream_ms": 0,
                        }))
                    } else {
                        None
                    },
                    "req_preview": f.req_preview,
                })
            })
            .collect();
        serde_json::json!({
            "enabled": true,
            "capacity": inner.capacity,
            "records": done,
            "in_flight": open,
        })
    }
}

/// Classify a wire outcome for the record â€” the single place the
/// error-code vocabulary is mapped (protocol-v1 Â§3).
pub fn classify(result: &Result<Value, crate::protocol::ErrorObj>) -> (Outcome, Option<&Value>) {
    match result {
        Ok(v) => (Outcome::Ok, Some(v)),
        Err(e) => {
            let limit = e
                .data
                .as_ref()
                .and_then(|d| d.get("limit"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let outcome = if e.code == crate::protocol::LIMIT_EXCEEDED {
                Outcome::Limit { limit }
            } else if e.code == crate::protocol::TASK_CANCELLED {
                let verdict = e
                    .data
                    .as_ref()
                    .and_then(|d| d.get("verdict"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Outcome::Cancelled { verdict }
            } else {
                Outcome::Error { code: e.code }
            };
            (outcome, None)
        }
    }
}
