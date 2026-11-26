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
/// Timeline (lifecycle/codegen/denial) capacity. Small entries, but the
/// point of giving stderr its own ring below is that a log burst can
/// never evict them. One `next_seq` counter orders everything across
/// rings, so the page interleaves them exactly.
pub const TIMELINE_CAPACITY: usize = 500;
/// stderr ring capacity — separate from the timeline so a log burst
/// evicts old stderr lines, never lifecycle events.
pub const STDERR_CAPACITY: usize = 1000;
/// A stderr line is already byte-capped upstream (protocol §9); the
/// ring caps it further so a full ring stays small even at the
/// upstream cap.
pub const STDERR_RING_CHARS: usize = 2048;
/// Error-detail caps (failed calls explain themselves): the message,
/// traceback, suggestions and field errors the wire carried — dev-only
/// optional `error.data` fields, truncated rather than ever growing
/// the ring without bound.
pub const ERR_MESSAGE_CHARS: usize = 1024;
pub const TRACEBACK_CHARS: usize = 8192;
pub const MAX_SUGGESTIONS: usize = 8;
pub const SUGGESTION_CHARS: usize = 512;
pub const MAX_FIELD_ERRORS: usize = 16;
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
    /// Monotonic row id, survives ring eviction. Shared by calls,
    /// timeline events and stderr lines — the page interleaves rings
    /// by it, so ordering across kinds is exact.
    next_seq: u64,
    /// Lifecycle, codegen and denial entries — the timeline minus
    /// stderr, which is high-volume enough to need its own ring.
    timeline: VecDeque<TimelineRow>,
    /// Python stderr lines, generation-tagged and timestamped. A call
    /// row expands to the lines inside its time window — "during this
    /// call", never "from this call" (docs/inspector.md).
    stderr: VecDeque<StderrRow>,
    /// The last codegen-status file content seen — timeline entries
    /// fire on CHANGE only, not on every snapshot poll.
    last_codegen: Option<String>,
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
    Error {
        code: i64,
        /// The wire error message, truncated — a failed call explains
        /// itself without a trip through terminal scrollback.
        message: String,
        /// Dev-only optional `error.data` fields (protocol-v1): shown
        /// when the runtime enriches them, absent otherwise — the
        /// inspector never depends on them existing.
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

/// One entry of the lifecycle/codegen/denial timeline. Serde-flattened
/// into its row, so the wire shape is `{seq, at_unix_ms, kind, event,
/// ...fields}` — `kind` is the page's filter grouping.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TimelineEvent {
    /// A runtime generation spawned — carries the child pid when the
    /// platform reported one.
    Spawn { generation: u64, pid: Option<u32> },
    /// The generation ended unexpectedly — the exit detail (code or
    /// signal) or the kill reason that ended it.
    Exit { generation: u64, detail: String },
    /// The supervisor decided to respawn — the crash reason is the why.
    Restart { generation: u64, detail: String },
    /// The crash breaker tripped: too many crashes inside the window —
    /// the runtime parks Failed instead of restarting.
    Breaker { generation: u64, detail: String },
    /// Dev hot reload: the restart trigger fired. `file` names the
    /// changed source file when the CLI reported one.
    DevReload {
        generation: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
    /// A warm pool's live-members count rose — diffed off the pong
    /// stats (host-observed, approximate: several spawns inside one
    /// ping interval arrive as one entry).
    WarmSpawn { function: String, n: u64, live: u64 },
    /// A warm pool's consecutive-deaths counter rose.
    WarmDeath { function: String, n: u64 },
    /// A warm pool's idle-TTL reap counter rose.
    WarmReap { function: String, n: u64 },
    /// The loop-stall warning fired (one per stall episode).
    StallWarn { detail: String },
    /// The stall episode ended — how long the loop was blocked at peak.
    StallRecovered { blocked_ms: u64 },
    /// `pylair dev`'s codegen result changed (ok, or the failure with
    /// Python's error) — mirrored off the status file the CLI writes.
    Codegen {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// The host refused a call for capability reasons — with the fix:
    /// the permission identifier and file the window needs.
    Denial {
        command: String,
        window: String,
        origin: String,
        function: String,
        hint: String,
    },
}

impl TimelineEvent {
    /// The page's filter grouping — lifecycle is everything the
    /// supervisor itself did; codegen/denial are their own kinds.
    pub fn kind(&self) -> &'static str {
        match self {
            TimelineEvent::Codegen { .. } => "codegen",
            TimelineEvent::Denial { .. } => "denial",
            _ => "lifecycle",
        }
    }
}

/// A timeline entry as serialized: the shared sequence number and the
/// wall-clock timestamp flattened with the event itself.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineRow {
    pub seq: u64,
    pub at_unix_ms: u64,
    pub kind: &'static str,
    #[serde(flatten)]
    pub ev: TimelineEvent,
}

/// One Python stderr line — generation-tagged so lines on either side
/// of a restart never confuse, timestamped for the call-window join.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StderrRow {
    pub seq: u64,
    pub at_unix_ms: u64,
    pub generation: u64,
    pub line: String,
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
                    timeline: VecDeque::new(),
                    stderr: VecDeque::new(),
                    last_codegen: None,
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
        let seq = seq.max(
            inner
                .in_flight
                .values()
                .map(|f| f.seq + 1)
                .max()
                .unwrap_or(1),
        );
        let _ = seq;
        let seq = { inner.next_seq };
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

    /// One lifecycle/codegen/denial entry into the timeline ring. Same
    /// rules as calls: drop the sample on a contended lock, evict the
    /// oldest at capacity, never touch the events channel.
    pub fn note_event(&self, ev: TimelineEvent) {
        if !self.enabled() {
            return;
        }
        let Ok(mut guard) = self.inner.try_lock() else {
            return;
        };
        let Some(inner) = guard.as_mut() else {
            return;
        };
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.timeline.len() >= TIMELINE_CAPACITY {
            inner.timeline.pop_front();
        }
        inner.timeline.push_back(TimelineRow {
            seq,
            at_unix_ms: unix_ms(),
            kind: ev.kind(),
            ev,
        });
    }

    /// One Python stderr line, tapped in the drain before the event
    /// channel — the tap must not depend on that channel's delivery
    /// (drop-oldest there would silently truncate the timeline too).
    /// The line is re-capped to `STDERR_RING_CHARS` even though the
    /// upstream assembler already caps it: the ring's memory bound is
    /// its own contract.
    pub fn note_stderr(&self, generation: u64, line: &str) {
        if !self.enabled() {
            return;
        }
        let Ok(mut guard) = self.inner.try_lock() else {
            return;
        };
        let Some(inner) = guard.as_mut() else {
            return;
        };
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.stderr.len() >= STDERR_CAPACITY {
            inner.stderr.pop_front();
        }
        inner.stderr.push_back(StderrRow {
            seq,
            at_unix_ms: unix_ms(),
            generation,
            line: truncate_chars(line, STDERR_RING_CHARS),
        });
    }

    /// The codegen-status file's content, polled by the snapshot path.
    /// Records a timeline entry only when the content CHANGED — the
    /// file is rewritten per codegen run, so a change is a run.
    pub fn note_codegen_status(&self, raw: &str) {
        if !self.enabled() {
            return;
        }
        let parsed: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Ok(mut guard) = self.inner.try_lock() else {
            return; // contended — the next poll re-reads the same file
        };
        let Some(inner) = guard.as_mut() else {
            return;
        };
        if inner.last_codegen.as_deref() == Some(raw) {
            return;
        }
        inner.last_codegen = Some(raw.to_string());
        let ok = parsed["ok"].as_bool().unwrap_or(false);
        let detail = if ok {
            parsed["functions"]
                .as_u64()
                .map(|n| format!("{n} functions"))
        } else {
            parsed["error"]
                .as_str()
                .map(|e| truncate_chars(e, ERR_MESSAGE_CHARS))
        };
        let ev = TimelineEvent::Codegen { ok, detail };
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.timeline.len() >= TIMELINE_CAPACITY {
            inner.timeline.pop_front();
        }
        inner.timeline.push_back(TimelineRow {
            seq,
            at_unix_ms: unix_ms(),
            kind: ev.kind(),
            ev,
        });
    }

    /// The inspector snapshot: every recorded call plus the still-open
    /// in-flight rows (so the table shows long-running calls live),
    /// the timeline and stderr rings the page interleaves by `seq`.
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
        let timeline: Vec<&TimelineRow> = inner.timeline.iter().collect();
        let stderr: Vec<&StderrRow> = inner.stderr.iter().collect();
        serde_json::json!({
            "enabled": true,
            "capacity": inner.capacity,
            "records": done,
            "in_flight": open,
            "timeline": timeline,
            "stderr": stderr,
        })
    }
}

/// Char-capped copy — truncation never splits a multibyte character.
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let cut: String = s.chars().take(cap).collect();
    format!("{cut}…")
}

/// Extract the dev-only optional error detail a wire error carries
/// (protocol-v1: `traceback`, `suggestions`, `errors` under `data`).
/// Every field is independently optional — the inspector displays
/// what is present and never depends on any of it existing.
fn error_outcome(e: &crate::protocol::ErrorObj) -> Outcome {
    let message = truncate_chars(&e.message, ERR_MESSAGE_CHARS);
    let (mut traceback, mut suggestions, mut errors) = (None, None, None);
    if let Some(data) = e.data.as_ref() {
        traceback = data
            .get("traceback")
            .and_then(Value::as_str)
            .map(|t| truncate_chars(t, TRACEBACK_CHARS));
        suggestions = data.get("suggestions").and_then(Value::as_array).map(|xs| {
            xs.iter()
                .filter_map(Value::as_str)
                .take(MAX_SUGGESTIONS)
                .map(|s| truncate_chars(s, SUGGESTION_CHARS))
                .collect()
        });
        errors = data
            .get("errors")
            .and_then(Value::as_array)
            .map(|xs| xs.iter().take(MAX_FIELD_ERRORS).cloned().collect());
    }
    Outcome::Error {
        code: e.code,
        message,
        traceback,
        suggestions,
        errors,
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
                error_outcome(e)
            };
            (outcome, None)
        }
    }
}

/// Diff one pong's warm-pool stats against the previous mirror — the
/// host's only window into member churn (0008: pools are runtime-side;
/// what the inspector can know is what the pong reported). Approximate
/// by construction: several spawns/deaths inside one ping interval
/// arrive as one entry with a count, and a deaths-counter reset (a
/// success between deaths) is not an event. Pure for testing.
pub fn diff_pools(
    prev: &std::collections::BTreeMap<String, Value>,
    next: &Value,
) -> Vec<TimelineEvent> {
    let mut out = Vec::new();
    let Some(map) = next.as_object() else {
        return out;
    };
    let get = |stats: &Value, key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);
    for (fn_name, stats) in map {
        let before = prev.get(fn_name);
        let live_before = before.map(|b| get(b, "live")).unwrap_or(0);
        let live = get(stats, "live");
        let deaths_before = before.map(|b| get(b, "deaths")).unwrap_or(0);
        let deaths = get(stats, "deaths");
        let reaps_before = before.map(|b| get(b, "reaps")).unwrap_or(0);
        let reaps = get(stats, "reaps");
        if deaths > deaths_before {
            out.push(TimelineEvent::WarmDeath {
                function: fn_name.clone(),
                n: deaths - deaths_before,
            });
        }
        if reaps > reaps_before {
            out.push(TimelineEvent::WarmReap {
                function: fn_name.clone(),
                n: reaps - reaps_before,
            });
        }
        if live > live_before {
            out.push(TimelineEvent::WarmSpawn {
                function: fn_name.clone(),
                n: live - live_before,
                live,
            });
        }
    }
    out
}

// ------------------------------------------------------------------
// The inspector window: a custom scheme serves the bundled page and
// the JSON snapshot; both are gated to the inspector window's own
// label, so app windows cannot read the tap even if they guess the
// URL. The page polls `snapshot` over the scheme — no Tauri command
// surface is needed inside the window at all (docs/inspector.md).
// ------------------------------------------------------------------

/// The inspector window's label — fixed so the scheme handler can gate
/// on it and the command can refuse every other window.
pub const WINDOW_LABEL: &str = "pylair-inspector";
/// The custom scheme the page and snapshot are served over.
pub const SCHEME: &str = "pylair-inspect";

/// The bundled page — one static file, no framework, no network.
const PAGE: &str = include_str!("inspect/page.html");

/// Serve one inspector-scheme request. Anything but the inspector
/// window itself gets 403 — the tap is dev tooling and must not become
/// a data channel into app windows. Routes: `/` and `/index.html`
/// serve the page, `/snapshot` the tap+health JSON.
pub fn serve<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    label: &str,
    path: &str,
) -> (u16, &'static str, Vec<u8>) {
    if label != WINDOW_LABEL {
        return (403, "text/plain", b"inspector only".to_vec());
    }
    let _ = label;
    if path == "/" || path == "/index.html" || path.is_empty() {
        return (200, "text/html", PAGE.as_bytes().to_vec());
    }
    if path == "/snapshot" {
        let snap = inspect_snapshot_payload(app);
        return (200, "application/json", snap.to_string().into_bytes());
    }
    (404, "text/plain", b"not found".to_vec())
}

/// The dev-only codegen status file `pylair dev` writes next to its
/// restart trigger (`.pylair/codegen-status.json`) and points the app
/// at through `PYLAIR_CODEGEN_STATUS` — the simplest channel that
/// stays dev-only: the variable only ever reaches the dev child, and
/// the inspector merely reads a file it was explicitly handed.
/// Absent variable or unreadable file: no banner, no timeline entries.
pub fn read_codegen_status() -> Option<String> {
    let path = std::env::var_os("PYLAIR_CODEGEN_STATUS")?;
    std::fs::read_to_string(path).ok()
}

/// Poll the codegen status file into the tap (recording a timeline
/// entry on change) and return the parsed status for the banner.
/// Shared by the scheme snapshot and the command so both surfaces
/// tell the same story.
pub fn poll_codegen(tap: &InspectRef) -> Option<Value> {
    let raw = read_codegen_status()?;
    tap.note_codegen_status(&raw);
    serde_json::from_str(&raw).ok()
}

/// The `/snapshot` body: tap records plus a health snapshot. Shared by
/// the scheme handler and the `python_inspect_snapshot` command.
pub fn inspect_snapshot_payload<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Value {
    use tauri::Manager;
    let mut snap = serde_json::json!({"enabled": false, "records": [], "in_flight": []});
    if let Some(rt) = app.try_state::<crate::PyRuntime>() {
        let codegen = poll_codegen(&rt.0.inspect());
        snap = rt.0.inspect().snapshot();
        snap["health"] = serde_json::to_value(rt.0.health(None)).unwrap_or(Value::Null);
        if let Some(c) = codegen {
            snap["codegen"] = c;
        }
    }
    snap
}
