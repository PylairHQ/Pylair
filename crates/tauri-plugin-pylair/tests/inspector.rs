//! Inspector tap tests (docs/inspector.md): outcome capture, boundedness,
//! flag-off no-op, payload previews, release absence. Most drive the
//! `InspectRef` directly — the tap is the unit under test; the session
//! wiring is covered by the integration test at the bottom plus the
//! command-refusal test (non-windows, real webview).

use serde_json::json;
use tauri_plugin_pylair::inspect::{classify, InspectRef, Outcome, DEFAULT_CAPACITY};
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
    assert!(matches!(o, Outcome::Error { code } if code == proto::INTERNAL));

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
