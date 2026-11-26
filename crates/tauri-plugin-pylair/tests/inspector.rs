//! Inspector tap tests (docs/inspector.md): outcome capture, boundedness,
//! flag-off no-op, payload previews, release absence. Most drive the
//! `InspectRef` directly — the tap is the unit under test; the session
//! wiring is covered by the integration test at the bottom plus the
//! command-refusal test (non-windows, real webview).

use serde_json::json;
use tauri_plugin_pylair::inspect::{
    classify, diff_pools, InspectRef, Outcome, TimelineEvent, DEFAULT_CAPACITY, STDERR_CAPACITY,
    STDERR_RING_CHARS, TIMELINE_CAPACITY,
};
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::ErrorObj;

fn err(code: i64, data: Option<serde_json::Value>) -> ErrorObj {
    ErrorObj {
        code,
        message: "m".into(),
        data,
    }
}

#[test]
fn flag_off_records_nothing() {
    let tap = InspectRef::new(false, false);
    assert!(!tap.enabled());
    tap.note_start(1, "f", Some("main"), "inline", &json!({"a": 1}), false);
    tap.note_end(1, Outcome::Ok, Some(&json!(2)));
    let snap = tap.snapshot();
    assert_eq!(snap["enabled"], false);
    assert_eq!(snap["records"].as_array().unwrap().len(), 0);
}

#[test]
fn records_ok_with_latency_and_sizes() {
    let tap = InspectRef::new(true, false);
    assert!(tap.enabled());
    tap.note_start(
        7,
        "add",
        Some("main"),
        "inline",
        &json!({"a": 1, "b": 2}),
        false,
    );
    std::thread::sleep(std::time::Duration::from_millis(3));
    tap.note_end(7, Outcome::Ok, Some(&json!(3)));
    let snap = tap.snapshot();
    let recs = snap["records"].as_array().unwrap();
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r["task"], 7);
    assert_eq!(r["function"], "add");
    assert_eq!(r["window"], "main");
    assert_eq!(r["mode"], "inline");
    assert_eq!(r["outcome"]["kind"], "ok");
    assert!(r["latency_ms"].as_u64().unwrap() >= 2, "sane latency");
    assert!(r["req_bytes"].as_u64().unwrap() > 0);
    assert!(r["resp_bytes"].as_u64().unwrap() > 0);
    assert!(r["start_unix_ms"].as_u64().unwrap() > 0);
    // No payload previews without the second flag.
    assert!(r.get("req_preview").is_none());
}

#[test]
fn classify_covers_error_cancel_verdicts_and_limit() {
    let ok = Ok(json!(1));
    let (o, _) = classify(&ok);
    assert!(matches!(o, Outcome::Ok));

    let e = Err(err(proto::INTERNAL, None));
    let (o, _) = classify(&e);
    assert!(matches!(o, Outcome::Error { code, .. } if code == proto::INTERNAL));

    // Both cancel verdicts the wire vocabulary carries.
    for verdict in ["grace", "killed"] {
        let e = Err(err(
            proto::TASK_CANCELLED,
            Some(json!({"verdict": verdict})),
        ));
        let (o, _) = classify(&e);
        match o {
            Outcome::Cancelled { verdict: v } => assert_eq!(v.as_deref(), Some(verdict)),
            other => panic!("expected cancelled, got {other:?}"),
        }
    }

    let e = Err(err(
        proto::LIMIT_EXCEEDED,
        Some(json!({"limit": "max_concurrent_tasks"})),
    ));
    let (o, _) = classify(&e);
    match o {
        Outcome::Limit { limit } => assert_eq!(limit.as_deref(), Some("max_concurrent_tasks")),
        other => panic!("expected limit, got {other:?}"),
    }
}

#[test]
fn stream_chunks_counted_with_duration() {
    let tap = InspectRef::new(true, false);
    tap.note_start(3, "counter", Some("main"), "inline", &json!({"n": 5}), true);
    for _ in 0..5 {
        tap.note_chunk(3, &json!(42));
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    tap.note_end(3, Outcome::Ok, None);
    let snap = tap.snapshot();
    let r = &snap["records"].as_array().unwrap()[0];
    assert_eq!(r["stream"]["chunks"], 5);
    assert!(r["stream"]["chunk_bytes"].as_u64().unwrap() > 0);
    assert!(r["stream"]["stream_ms"].as_u64().is_some());
}

#[test]
fn ring_stays_bounded_under_burst() {
    let tap = InspectRef::new(true, false);
    let burst = DEFAULT_CAPACITY * 3;
    for i in 0..burst {
        let t = i as u64 + 1;
        tap.note_start(t, "f", None, "inline", &json!(null), false);
        tap.note_end(t, Outcome::Ok, Some(&json!(null)));
    }
    let snap = tap.snapshot();
    let recs = snap["records"].as_array().unwrap();
    assert_eq!(recs.len(), DEFAULT_CAPACITY, "ring drops oldest");
    // The NEWEST survive: the last seq recorded is in the ring.
    let last = recs.last().unwrap();
    assert_eq!(last["seq"].as_u64().unwrap(), burst as u64);
    // in-flight empty after paired start/end.
    assert_eq!(snap["in_flight"].as_array().unwrap().len(), 0);
}

#[test]
fn in_flight_cap_is_bounded() {
    let tap = InspectRef::new(true, false);
    // Start many, end none — the in-flight map must not grow without bound.
    for i in 0..5000u64 {
        tap.note_start(i + 1, "f", None, "inline", &json!(null), false);
    }
    let snap = tap.snapshot();
    let open = snap["in_flight"].as_array().unwrap();
    assert!(open.len() <= 4096, "in-flight hard cap holds");
}

#[test]
fn payloads_preview_truncates_and_hides_blob_bytes() {
    let tap = InspectRef::new(true, true);
    let big_b64 = "A".repeat(5000);
    let input = json!({"blob": {"$b64": big_b64}, "pad": "x".repeat(1000)});
    tap.note_start(1, "f", None, "inline", &input, false);
    tap.note_end(1, Outcome::Ok, Some(&json!({"result": "y".repeat(1000)})));
    let snap = tap.snapshot();
    let r = &snap["records"].as_array().unwrap()[0];
    let req = r["req_preview"].as_str().unwrap();
    // $b64 shows as a size, never the bytes.
    assert!(!req.contains(&"A".repeat(100)), "no raw blob bytes");
    assert!(req.contains("bytes"), "size marker present");
    // Truncated around 512 chars.
    assert!(req.chars().count() <= 520, "preview truncated");
    let resp = r["resp_preview"].as_str().unwrap();
    assert!(resp.chars().count() <= 520);
}

#[test]
fn payloads_off_by_default() {
    let tap = InspectRef::new(true, false);
    tap.note_start(1, "f", None, "inline", &json!({"secret": 1}), false);
    tap.note_end(1, Outcome::Ok, Some(&json!({"secret": 2})));
    let snap = tap.snapshot();
    let r = &snap["records"].as_array().unwrap()[0];
    assert!(r.get("req_preview").is_none());
    assert!(r.get("resp_preview").is_none());
}

// ---- the correlated timeline (docs/inspector.md) ----

#[test]
fn lifecycle_events_recorded_with_seqs_shared_with_calls() {
    let tap = InspectRef::new(true, false);
    tap.note_event(TimelineEvent::Spawn {
        generation: 0,
        pid: Some(4321),
    });
    tap.note_start(1, "f", None, "inline", &json!(null), false);
    tap.note_end(1, Outcome::Ok, Some(&json!(null)));
    tap.note_event(TimelineEvent::Exit {
        generation: 0,
        detail: "exited code 1".into(),
    });
    tap.note_stderr(0, "some log line");
    let snap = tap.snapshot();
    let tl = snap["timeline"].as_array().unwrap();
    assert_eq!(tl.len(), 2);
    assert_eq!(tl[0]["event"], "spawn");
    assert_eq!(tl[0]["kind"], "lifecycle");
    assert_eq!(tl[0]["generation"], 0);
    assert_eq!(tl[0]["pid"], 4321);
    assert_eq!(tl[1]["event"], "exit");
    // One shared seq counter: the call's seq sits between the two events,
    // and the stderr line after them — the page interleaves exactly.
    let call_seq = snap["records"][0]["seq"].as_u64().unwrap();
    let stderr_seq = snap["stderr"][0]["seq"].as_u64().unwrap();
    assert!(tl[0]["seq"].as_u64().unwrap() < call_seq);
    assert!(call_seq < tl[1]["seq"].as_u64().unwrap());
    assert!(tl[1]["seq"].as_u64().unwrap() < stderr_seq);
    assert!(snap["timeline"][0]["at_unix_ms"].as_u64().unwrap() > 0);
}

#[test]
fn timeline_ring_bounded() {
    let tap = InspectRef::new(true, false);
    for g in 0..(TIMELINE_CAPACITY * 2) {
        tap.note_event(TimelineEvent::Spawn {
            generation: g as u64,
            pid: None,
        });
    }
    let snap = tap.snapshot();
    let tl = snap["timeline"].as_array().unwrap();
    assert_eq!(tl.len(), TIMELINE_CAPACITY, "timeline drops oldest");
    // the NEWEST survive
    assert_eq!(
        tl.last().unwrap()["generation"].as_u64().unwrap(),
        (TIMELINE_CAPACITY * 2 - 1) as u64
    );
}

#[test]
fn stderr_ring_bounded_and_line_capped() {
    let tap = InspectRef::new(true, false);
    for i in 0..(STDERR_CAPACITY + 100) {
        tap.note_stderr(0, &format!("line {i}"));
    }
    let snap = tap.snapshot();
    let rows = snap["stderr"].as_array().unwrap();
    assert_eq!(rows.len(), STDERR_CAPACITY, "stderr ring drops oldest");
    assert_eq!(
        rows.last().unwrap()["line"].as_str().unwrap(),
        format!("line {}", STDERR_CAPACITY + 99)
    );
    // line cap: a line past STDERR_RING_CHARS is truncated with a marker
    let tap2 = InspectRef::new(true, false);
    let long = "x".repeat(STDERR_RING_CHARS * 3);
    tap2.note_stderr(0, &long);
    let line = tap2.snapshot()["stderr"][0]["line"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(line.chars().count() <= STDERR_RING_CHARS + 1, "capped");
    assert!(line.ends_with('…'), "truncation marker");
    // generation tagging
    assert_eq!(tap2.snapshot()["stderr"][0]["generation"], json!(0));
}

#[test]
fn stderr_flood_does_not_evict_lifecycle() {
    let tap = InspectRef::new(true, false);
    tap.note_event(TimelineEvent::Spawn {
        generation: 0,
        pid: None,
    });
    for _ in 0..(STDERR_CAPACITY * 2) {
        tap.note_stderr(0, "noise");
    }
    let snap = tap.snapshot();
    // the spawn event survives a stderr flood — separate rings.
    assert_eq!(snap["timeline"].as_array().unwrap().len(), 1);
    assert_eq!(snap["stderr"].as_array().unwrap().len(), STDERR_CAPACITY);
}

#[test]
fn classify_error_carries_message_and_optional_detail() {
    // Plain error: message always, the dev-only fields absent.
    let e = Err(err(proto::INTERNAL, None));
    let (o, _) = classify(&e);
    match o {
        Outcome::Error {
            code,
            message,
            traceback,
            suggestions,
            errors,
        } => {
            assert_eq!(code, proto::INTERNAL);
            assert_eq!(message, "m");
            assert!(traceback.is_none());
            assert!(suggestions.is_none());
            assert!(errors.is_none());
        }
        other => panic!("expected error, got {other:?}"),
    }
    // Enriched error (the optional error.data fields): all carried.
    let e = Err(err(
        proto::INTERNAL,
        Some(json!({
            "traceback": "Traceback...\nValueError: nope",
            "suggestions": ["use a plain def", "await asyncio.to_thread(...)"],
            "errors": [{"path": "items.0", "expected": "int", "received": "str"}],
        })),
    ));
    let (o, _) = classify(&e);
    match o {
        Outcome::Error {
            traceback,
            suggestions,
            errors,
            ..
        } => {
            assert_eq!(traceback.as_deref(), Some("Traceback...\nValueError: nope"));
            assert_eq!(suggestions.as_ref().unwrap().len(), 2);
            let errs = errors.unwrap();
            assert_eq!(errs[0]["path"], "items.0");
            assert_eq!(errs[0]["expected"], "int");
        }
        other => panic!("expected error, got {other:?}"),
    }
    // Serialization omits the absent fields but carries present ones.
    let e = Err(err(proto::INTERNAL, Some(json!({"suggestions": ["x"]}))));
    let (o, _) = classify(&e);
    let v = serde_json::to_value(&o).unwrap();
    assert!(v.get("traceback").is_none());
    assert!(v.get("errors").is_none());
    assert_eq!(v["suggestions"][0], "x");
    assert_eq!(v["message"], "m");
}

#[test]
fn diff_pools_reports_spawn_death_reap() {
    let mut prev = std::collections::BTreeMap::new();
    prev.insert(
        "transcribe".to_string(),
        json!({"live": 2, "deaths": 0, "reaps": 0}),
    );
    // nothing changed: no events
    let next = json!({"transcribe": {"live": 2, "deaths": 0, "reaps": 0}});
    assert!(diff_pools(&prev, &next).is_empty());
    // one death + a replacement spawn + a reap in one interval
    let next = json!({"transcribe": {"live": 3, "deaths": 1, "reaps": 1}});
    let evs = diff_pools(&prev, &next);
    assert!(
        evs.iter().any(|e| matches!(e, TimelineEvent::WarmDeath { function, n } if function == "transcribe" && *n == 1)),
        "death diffed: {evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, TimelineEvent::WarmReap { n, .. } if *n == 1)),
        "reap diffed"
    );
    assert!(
        evs.iter().any(
            |e| matches!(e, TimelineEvent::WarmSpawn { n, live, .. } if *n == 1 && *live == 3)
        ),
        "spawn diffed"
    );
    // a deaths-counter RESET (success between deaths) is not an event
    let next = json!({"transcribe": {"live": 3, "deaths": 0, "reaps": 1}});
    assert!(diff_pools(&prev, &next)
        .iter()
        .all(|e| !matches!(e, TimelineEvent::WarmDeath { .. })));
    // first sighting of a pool: the initial fill reports as spawns
    let empty = std::collections::BTreeMap::new();
    let next = json!({"embed": {"live": 2, "deaths": 0, "reaps": 0}});
    let evs = diff_pools(&empty, &next);
    assert!(
        evs.iter().any(|e| matches!(e, TimelineEvent::WarmSpawn { function, n, .. } if function == "embed" && *n == 2)),
        "initial fill: {evs:?}"
    );
}

#[test]
fn codegen_status_dedupes_until_content_changes() {
    let tap = InspectRef::new(true, false);
    let ok = r#"{"at":1,"ok":true,"functions":7}"#;
    tap.note_codegen_status(ok);
    tap.note_codegen_status(ok); // same content — polled repeatedly
    tap.note_codegen_status(ok);
    let snap = tap.snapshot();
    let tl = snap["timeline"].as_array().unwrap();
    assert_eq!(tl.len(), 1, "one entry per change, not per poll");
    assert_eq!(tl[0]["event"], "codegen");
    assert_eq!(tl[0]["kind"], "codegen");
    assert_eq!(tl[0]["ok"], true);
    assert_eq!(tl[0]["detail"], "7 functions");
    // a failure replaces it — with Python's error as the detail
    let bad = r#"{"at":2,"ok":false,"error":"ModuleNotFoundError: no module named x"}"#;
    tap.note_codegen_status(bad);
    let tl = tap.snapshot()["timeline"].as_array().unwrap().clone();
    assert_eq!(tl.len(), 2);
    assert_eq!(tl[1]["ok"], false);
    assert!(tl[1]["detail"]
        .as_str()
        .unwrap()
        .contains("ModuleNotFoundError"));
    // malformed content records nothing but does not wedge the tap
    tap.note_codegen_status("not json");
    assert_eq!(tap.snapshot()["timeline"].as_array().unwrap().len(), 2);
}

#[test]
fn denial_entry_serializes_the_fix() {
    let tap = InspectRef::new(true, false);
    tap.note_event(TimelineEvent::Denial {
        command: "python_call".into(),
        window: "main".into(),
        origin: "http://localhost:1420".into(),
        function: "admin_reset".into(),
        hint: "grant pylair:allow-python-call ... in src-tauri/capabilities/".into(),
    });
    let snap = tap.snapshot();
    let row = &snap["timeline"][0];
    assert_eq!(row["kind"], "denial");
    assert_eq!(row["event"], "denial");
    assert_eq!(row["command"], "python_call");
    assert_eq!(row["window"], "main");
    assert_eq!(row["origin"], "http://localhost:1420");
    assert_eq!(row["function"], "admin_reset");
    assert!(row["hint"].as_str().unwrap().contains("capabilities"));
}

#[test]
fn timeline_event_kind_grouping() {
    assert_eq!(
        TimelineEvent::Spawn {
            generation: 0,
            pid: None
        }
        .kind(),
        "lifecycle"
    );
    assert_eq!(
        TimelineEvent::StallWarn { detail: "x".into() }.kind(),
        "lifecycle"
    );
    assert_eq!(
        TimelineEvent::Codegen {
            ok: true,
            detail: None
        }
        .kind(),
        "codegen"
    );
    assert_eq!(
        TimelineEvent::Denial {
            command: "c".into(),
            window: "w".into(),
            origin: "o".into(),
            function: "f".into(),
            hint: "h".into(),
        }
        .kind(),
        "denial"
    );
}

// Release builds contain no inspector: the stub type compiles the same
// call sites to nothing. This test is meaningless in debug (the real
// module is present) — its value is entirely in the release compile,
// where `inspect` resolves to `inspect_stub.rs` and enabled() is a
// constant false. CI's release check compiles this assertion.
#[test]
fn release_shell_is_inert() {
    #[cfg(not(debug_assertions))]
    {
        let tap = InspectRef::new(true, true);
        assert!(!tap.enabled(), "release must never enable the tap");
        tap.note_start(1, "f", None, "inline", &json!(null), false);
        let snap = tap.snapshot();
        assert_eq!(snap["enabled"], false);
    }
    #[cfg(debug_assertions)]
    {
        // Debug: the real tap honors the flag — the inverse proof that
        // the same source compiles to different code per profile.
        let tap = InspectRef::new(true, true);
        assert!(tap.enabled());
    }
}
