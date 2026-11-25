//! Unit tests for supervisor internals (the former inline `mod tests`).

#[cfg(windows)]
use super::platform_windows::child_creation_flags;
use super::stderr::{
    StderrLines, StderrSink, STDERR_LINE_CAP, STDERR_QUEUE_CAP, STDERR_TRUNC_MARK,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::config::{clamp_hello_timeout_ms, parse_hello_timeout_ms};
use super::run::{classify_pong, PongVerdict, LOOP_STALL_WARN};
use super::stream::{stream_pair, ChunkFail};
use super::*;

/// F5: PYLAIR_HELLO_TIMEOUT_MS parsing — real values pass through,
/// garbage falls back to the default with a warning.
#[test]
fn hello_timeout_env_parses_or_defaults() {
    assert_eq!(
        parse_hello_timeout_ms(None),
        (30_000, None),
        "unset env must default to 30s"
    );
    assert_eq!(parse_hello_timeout_ms(Some("5000".into())).0, 5_000);
    assert_eq!(parse_hello_timeout_ms(Some(" 45000 ".into())).0, 45_000);
    // Garbage must not kill config: default + a warning to surface.
    for bad in ["abc", "", "-5", "1.5", "0x10"] {
        let (ms, warn) = parse_hello_timeout_ms(Some(bad.into()));
        assert_eq!(ms, 30_000, "{bad:?} must fall back to default");
        assert!(warn.is_some(), "{bad:?} must produce a warning");
    }
}

/// F5: the supported window is 1s..=600s, clamped with a warning.
#[test]
fn hello_timeout_clamps_into_supported_range() {
    assert_eq!(clamp_hello_timeout_ms(1_000).0, 1_000);
    assert_eq!(clamp_hello_timeout_ms(600_000).0, 600_000);
    assert_eq!(clamp_hello_timeout_ms(30_000).0, 30_000);
    let (lo, w1) = clamp_hello_timeout_ms(0);
    assert_eq!(lo, 1_000);
    assert!(w1.is_some());
    let (lo2, w2) = clamp_hello_timeout_ms(500);
    assert_eq!(lo2, 1_000);
    assert!(w2.is_some());
    let (hi, w3) = clamp_hello_timeout_ms(999_999);
    assert_eq!(hi, 600_000);
    assert!(w3.is_some());
    // In-range produces no warning.
    assert!(clamp_hello_timeout_ms(30_000).1.is_none());
}

#[test]
fn stderr_lines_lossy_and_capped() {
    let mut l = StderrLines::new();
    // Invalid UTF-8 mid-stream: arrives as U+FFFD, does not stop parsing.
    let out = l.feed(b"\xff\xfe bad \x80\nsecond line\n");
    assert_eq!(out.len(), 2);
    assert!(out[0].contains('\u{FFFD}'));
    assert_eq!(out[1], "second line");
    // A line over the cap is truncated with the marker, then parsing
    // resumes on the next line.
    let mut big = vec![b'x'; STDERR_LINE_CAP + 500];
    big.extend_from_slice(b"\nok\n");
    let out = l.feed(&big);
    assert_eq!(out.len(), 2);
    assert!(out[0].len() <= STDERR_LINE_CAP + STDERR_TRUNC_MARK.len());
    assert!(out[0].ends_with(STDERR_TRUNC_MARK));
    assert_eq!(out[1], "ok");
    // Exactly at the cap nothing was dropped: no marker. One byte
    // over and the marker returns.
    let mut exact = vec![b'e'; STDERR_LINE_CAP];
    exact.extend_from_slice(b"\n");
    let out = l.feed(&exact);
    assert_eq!(out.len(), 1);
    assert!(!out[0].ends_with(STDERR_TRUNC_MARK));
    assert_eq!(out[0].len(), STDERR_LINE_CAP);
    let mut one_over = vec![b'o'; STDERR_LINE_CAP + 1];
    one_over.extend_from_slice(b"\n");
    let out = l.feed(&one_over);
    assert!(out[0].ends_with(STDERR_TRUNC_MARK));
    // Partial line held until newline or finish.
    assert!(l.feed(b"no newline yet").is_empty());
    assert_eq!(l.finish().as_deref(), Some("no newline yet"));
}

#[tokio::test]
async fn stderr_sink_drops_oldest_with_marker() {
    let sink = Arc::new(StderrSink::new());
    for i in 0..(STDERR_QUEUE_CAP + 10) {
        sink.push(format!("line-{i}"));
    }
    sink.finish();
    let (tx, mut rx) = crate::eventbus::pair(STDERR_QUEUE_CAP * 2);
    let fwd = tokio::spawn({
        let s = sink.clone();
        async move { s.forward(tx).await }
    });
    let mut got = Vec::new();
    while let Some(RuntimeEvent::Stderr(l)) = rx.recv().await {
        got.push(l);
    }
    fwd.await.unwrap();
    // 522 pushes into a 512 queue: the last 10 pushes each evict one
    // head line — oldest first, newest survives, and ONE marker
    // reports the cumulative count at delivery time.
    assert_eq!(got.last().unwrap(), "line-521");
    assert_eq!(got.first().unwrap(), "[dropped 10 stderr lines]");
    assert!(!got.contains(&"line-0".to_string()));
    assert!(!got.contains(&"line-9".to_string()));
    assert!(got.contains(&"line-10".to_string()));
}

#[cfg(windows)]
#[test]
fn console_flag_suppressed_unless_pylair_console() {
    use std::ffi::OsStr;
    // Default: CREATE_NO_WINDOW so the console-subsystem runtime stays
    // invisible under a GUI app.
    assert_eq!(child_creation_flags(None), 0x0800_0000);
    // PYLAIR_CONSOLE present (any value) → no flag, window allowed.
    for v in ["1", "0", ""] {
        assert_eq!(child_creation_flags(Some(OsStr::new(v))), 0);
    }
}

#[test]
fn pong_classification_boundaries() {
    use PongVerdict::*;
    let kill = Some(Duration::from_secs(300));
    // Below the warn threshold: Ready even with a kill configured.
    assert_eq!(classify_pong(Duration::ZERO, kill), Ready);
    assert_eq!(
        classify_pong(LOOP_STALL_WARN - Duration::from_millis(1), kill),
        Ready
    );
    // Warn band: from threshold up to (but not including) kill.
    assert_eq!(classify_pong(LOOP_STALL_WARN, kill), StallWarn);
    assert_eq!(classify_pong(Duration::from_secs(299), kill), StallWarn);
    // At/over the configured kill: StallKill.
    assert_eq!(classify_pong(Duration::from_secs(300), kill), StallKill);
    // No kill configured: any stall is only a diagnostic, forever.
    assert_eq!(classify_pong(Duration::from_secs(3600), None), StallWarn);
    assert_eq!(classify_pong(Duration::MAX, None), StallWarn);
    // A kill configured below the warn threshold still wins (config is king).
    assert_eq!(
        classify_pong(Duration::from_millis(500), Some(Duration::from_millis(500))),
        StallKill
    );
}

#[test]
fn stream_byte_caps_distinguish_per_stream_and_total() {
    use serde_json::json;
    // Deterministic counterpart to the e2e limits test: two streams
    // sharing one total budget — the chunk that crosses the SHARED cap
    // must be reported as TotalBytes even though its own per-stream
    // usage is under the per cap.
    let total = Arc::new(AtomicU64::new(0));
    let (s1, mut r1) = stream_pair(64, 1024, total.clone(), 1024);
    let (s2, _r2) = stream_pair(64, 1024, total.clone(), 1024);

    for i in 0..5u64 {
        assert!(s1.chunk(i, json!(0)).is_ok());
        assert!(s2.chunk(i, json!(0)).is_ok());
    }
    // Push the shared counter near the cap via a third view of the same
    // Arc — equivalent to bytes buffered by a consumer that isn't
    // draining.
    total.store(1024 - 8, Ordering::Relaxed);
    // s2's next chunk (18 bytes serialized) crosses the shared budget:
    // TotalBytes even though s2's own usage (~23 bytes) is under per.
    let err = json!("x".repeat(16));
    let res = s2.chunk(99, err.clone());
    assert!(
        matches!(res, Err(ChunkFail::TotalBytes)),
        "expected TotalBytes, got {res:?}"
    );
    // Draining s1's buffered chunks releases its leases, but the total
    // is still over — s1's next chunk trips the shared cap too.
    while r1.chunks.try_recv().is_ok() {}
    assert!(matches!(s1.chunk(99, err), Err(ChunkFail::TotalBytes)));
}
