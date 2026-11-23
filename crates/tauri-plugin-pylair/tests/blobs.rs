//! Decision-0010 crate coverage (doc §7): the `pylair-blob` scheme's
//! decision tree — origin gate → label → lookup → mutation check →
//! Range/206 with caps — plus registry limits, release, and generation
//! cleanup. Engine/webview proofs for these paths live in the spike
//! (`spikes/b0-blobs`, runs linked in the decision doc); these tests pin
//! the host-side semantics.

use serde_json::json;
use std::path::{Path, PathBuf};
use tauri::http::{self, HeaderMap, StatusCode};
use tauri_plugin_pylair::blobs::{BlobLimits, BlobRegistry, BlobRegistryRef, Reject};

const APP_ORIGIN: &str = "http://tauri.localhost";
const FOREIGN: &str = "http://127.0.0.1:8080";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pylair-blobtest-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A registry owning a fresh canonicalized generation dir; returns the
/// registry, the dir, and the blob-file path for a given id.
fn fixture(limits: BlobLimits) -> (BlobRegistryRef, PathBuf) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (r, _) = BlobRegistry::new(limits);
    let dir = tmp(&format!("g-{}-{n}", std::process::id()))
        .canonicalize()
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    r.lock().unwrap().set_generation(Some(dir.clone()));
    (r, dir)
}

fn small_limits() -> BlobLimits {
    // Same defaults the env reader applies, with tiny serve caps so the
    // tests exercise capping with small files.
    let mut l = BlobLimits::from_env();
    l.max_full_body = 8;
    l.max_range_bytes = 4;
    l
}

fn reg_blob(r: &BlobRegistryRef, dir: &Path, id: &str, owner: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(format!("{id}.bin"));
    std::fs::write(&path, bytes).unwrap();
    let res = r.lock().unwrap().register(
        &json!({"id": id, "path": path.to_string_lossy(), "mime": "application/pdf"}),
        owner,
    );
    assert!(res.is_ok(), "register failed: {res:?}");
    path
}

fn req(path: &str, headers: &[(&str, &str)]) -> http::Request<Vec<u8>> {
    let mut b = http::Request::builder().method("GET").uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Vec::new()).unwrap()
}

fn serve(
    r: &BlobRegistryRef,
    label: &str,
    req: &http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    r.lock()
        .unwrap()
        .serve(label, &[APP_ORIGIN.to_string()], req)
}

fn hdr<'a>(h: &'a HeaderMap, name: &str) -> &'a str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn assert_base_headers(h: &HeaderMap) {
    assert_eq!(hdr(h, "access-control-allow-origin"), APP_ORIGIN);
    assert_eq!(hdr(h, "vary"), "Origin");
    assert_eq!(hdr(h, "accept-ranges"), "bytes");
    assert_eq!(hdr(h, "cache-control"), "no-store");
    assert_eq!(hdr(h, "x-content-type-options"), "nosniff");
    assert_ne!(hdr(h, "access-control-allow-origin"), "*");
}

#[test]
fn full_body_200_headers_and_content() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"12345678");
    let res = serve(&r, "main", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::OK);
    assert_base_headers(res.headers());
    assert_eq!(hdr(res.headers(), "content-type"), "application/pdf");
    assert_eq!(res.body(), b"12345678");
    // An absent Origin is an element load — still served under the
    // label check (§2).
    let res2 = serve(&r, "main", &req("/b/a1", &[]));
    assert_eq!(res2.status(), StatusCode::OK);
}

#[test]
fn foreign_origin_denied_before_lookup_with_hardened_headers() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"12345678");
    // The owner window navigated to a foreign page — same label, bad
    // Origin → 403 even for an id that exists, and 403 for a bogus one
    // too (the gate precedes lookup).
    for id in ["a1", "nope"] {
        let res = serve(
            &r,
            "main",
            &req(&format!("/b/{id}"), &[("Origin", FOREIGN)]),
        );
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_base_headers(res.headers());
    }
}

#[test]
fn cross_window_403() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"12345678");
    let res = serve(&r, "second", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_base_headers(res.headers());
}

#[test]
fn unknown_id_404_and_bad_path_400() {
    let (r, _dir) = fixture(small_limits());
    let res = serve(&r, "main", &req("/b/missing", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = serve(&r, "main", &req("/x/missing", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = serve(&r, "main", &req("/b/../evil", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn range_206_and_short_206_under_cap() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"0123456789abcdef");
    let res = serve(
        &r,
        "main",
        &req("/b/a1", &[("Origin", APP_ORIGIN), ("Range", "bytes=2-5")]),
    );
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(hdr(res.headers(), "content-range"), "bytes 2-5/16");
    assert_eq!(res.body(), b"2345");
    // Cap is 4 bytes: an open range asking for everything comes back
    // short, with a truncated Content-Range the client re-requests from.
    let res = serve(
        &r,
        "main",
        &req("/b/a1", &[("Origin", APP_ORIGIN), ("Range", "bytes=4-")]),
    );
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(hdr(res.headers(), "content-range"), "bytes 4-7/16");
    assert_eq!(res.body(), b"4567");
    // Suffix form.
    let res = serve(
        &r,
        "main",
        &req("/b/a1", &[("Origin", APP_ORIGIN), ("Range", "bytes=-2")]),
    );
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.body(), b"ef");
}

#[test]
fn unsatisfiable_range_416_with_extent() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"0123456789abcdef");
    for range in ["bytes=16-20", "bytes=9-4", "items=0-1", "bytes=x-y"] {
        let res = serve(
            &r,
            "main",
            &req("/b/a1", &[("Origin", APP_ORIGIN), ("Range", range)]),
        );
        assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(hdr(res.headers(), "content-range"), "bytes */16");
        assert_base_headers(res.headers());
    }
}

#[test]
fn oversized_full_body_416_points_at_range() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", &[0u8; 16]); // 16 > 8-byte cap
    let res = serve(&r, "main", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(hdr(res.headers(), "content-range"), "bytes */16");
    let body = String::from_utf8_lossy(res.body());
    assert!(body.contains("Range"), "body should direct callers: {body}");
    // ...while a ranged read of the same blob serves fine.
    let res = serve(
        &r,
        "main",
        &req("/b/a1", &[("Origin", APP_ORIGIN), ("Range", "bytes=0-3")]),
    );
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
}

#[test]
fn mutated_source_410_gone() {
    let (r, dir) = fixture(small_limits());
    let path = reg_blob(&r, &dir, "a1", "main", b"12345678");
    // In-place rewrite: same path, different size+mtime → the registered
    // metadata no longer verifies (decision 0010 §4).
    std::fs::write(&path, b"DIFFERENT").unwrap();
    let res = serve(&r, "main", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::GONE);
    assert_base_headers(res.headers());
}

#[test]
fn release_idempotent_and_owner_checked() {
    let (r, dir) = fixture(small_limits());
    let path = reg_blob(&r, &dir, "a1", "main", b"12345678");
    {
        let mut reg = r.lock().unwrap();
        assert!(reg.release_owned("a1", "second").is_err()); // foreign → denied
        assert!(reg.release_owned("a1", "main").unwrap()); // owner → released
        assert!(!reg.release_owned("a1", "main").unwrap()); // double release idempotent
        assert!(!reg.has("a1"));
    }
    assert!(!path.exists(), "released blob's file must be unlinked");
    let res = serve(&r, "main", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[test]
fn drop_owner_frees_only_that_windows_blobs() {
    let (r, dir) = fixture(small_limits());
    let a = reg_blob(&r, &dir, "a1", "main", b"11111111");
    let b = reg_blob(&r, &dir, "b2", "second", b"22222222");
    r.lock().unwrap().drop_owner("main");
    assert!(!a.exists());
    assert!(b.exists());
    assert!(!r.lock().unwrap().has("a1"));
    assert!(r.lock().unwrap().has("b2"));
}

#[test]
fn limits_blob_handles_per_window() {
    let mut l = small_limits();
    l.max_blobs_per_window = 2;
    let (r, dir) = fixture(l);
    reg_blob(&r, &dir, "a1", "main", b"1");
    reg_blob(&r, &dir, "a2", "main", b"1");
    let path = dir.join("a3.bin");
    std::fs::write(&path, b"1").unwrap();
    let res = r
        .lock()
        .unwrap()
        .register(&json!({"id": "a3", "path": path.to_string_lossy()}), "main");
    assert!(matches!(res, Err(Reject::Limit("blob_handles"))));
    // A second window's allowance is independent — with its own payload
    // file, since a3's rejected payload was unlinked.
    let path2 = dir.join("b1.bin");
    std::fs::write(&path2, b"1").unwrap();
    assert!(r
        .lock()
        .unwrap()
        .register(
            &json!({"id": "b1", "path": path2.to_string_lossy()}),
            "second"
        )
        .is_ok());
}

#[test]
fn limits_blob_bytes_and_generation_total() {
    let mut l = small_limits();
    l.max_blob_bytes = 4;
    l.max_blob_total = 6;
    let (r, dir) = fixture(l);
    let big = dir.join("big.bin");
    std::fs::write(&big, b"12345").unwrap();
    let res = r
        .lock()
        .unwrap()
        .register(&json!({"id": "big", "path": big.to_string_lossy()}), "main");
    assert!(matches!(res, Err(Reject::Limit("blob_bytes"))));
    reg_blob(&r, &dir, "a1", "main", b"1234");
    reg_blob(&r, &dir, "a2", "main", b"12");
    let over = dir.join("over.bin");
    std::fs::write(&over, b"1").unwrap();
    let res = r.lock().unwrap().register(
        &json!({"id": "over", "path": over.to_string_lossy()}),
        "main",
    );
    assert!(
        matches!(
            res,
            Err(Reject::Limit("blob_total")) | Err(Reject::Limit("blob_bytes_share"))
        ),
        "expected a generation/pool limit, got {res:?}"
    );
    // Releasing frees generation budget.
    assert!(r.lock().unwrap().release("a1"));
    reg_blob(&r, &dir, "a3", "main", b"1");
}

#[test]
fn register_rejects_foreign_paths_dup_ids_and_no_dir() {
    let (r, dir) = fixture(small_limits());
    let outside = tmp("outside").join("x.bin");
    std::fs::write(&outside, b"x").unwrap();
    let res = r.lock().unwrap().register(
        &json!({"id": "x", "path": outside.to_string_lossy()}),
        "main",
    );
    assert!(matches!(res, Err(Reject::Bad(_))));
    // A pre-containment rejection never unlinks: the outside file is
    // not ours to touch.
    assert!(outside.exists());
    let live = reg_blob(&r, &dir, "a1", "main", b"1");
    // Duplicate id naming the LIVE blob's own path — the classic case
    // where an unlink would kill a serving blob.
    let dup = r
        .lock()
        .unwrap()
        .register(&json!({"id": "a1", "path": live.to_string_lossy()}), "main");
    assert!(matches!(dup, Err(Reject::Bad(_))));
    assert!(live.exists());
    let res = serve(&r, "main", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::OK);
    // Duplicate id with a fresh payload path — still pre-containment,
    // so that file stays on disk for the generation-end sweep.
    let dup_path = dir.join("dup-payload.bin");
    std::fs::write(&dup_path, b"2").unwrap();
    let dup2 = r.lock().unwrap().register(
        &json!({"id": "a1", "path": dup_path.to_string_lossy()}),
        "main",
    );
    assert!(matches!(dup2, Err(Reject::Bad(_))));
    assert!(dup_path.exists());
    // No generation dir (capability off) → every register is Bad.
    let (r2, _) = BlobRegistry::new(small_limits());
    let res = r2.lock().unwrap().register(
        &json!({"id": "y", "path": outside.to_string_lossy()}),
        "main",
    );
    assert!(matches!(res, Err(Reject::Bad(_))));
}

/// A rejection AFTER the containment check unlinks the payload: Python
/// materialized it before registering and no registry entry exists to
/// clean it up, so it would otherwise leak uncounted disk until the
/// generation ends.
#[test]
fn post_containment_rejection_unlinks_the_payload() {
    let mut limits = small_limits();
    limits.max_blob_bytes = 4;
    limits.max_blob_total = 64;
    limits.max_blobs_per_window = 1;
    let (r, dir) = fixture(limits);
    // Over the per-blob cap.
    let big = dir.join("big.bin");
    std::fs::write(&big, b"12345").unwrap();
    let res = r
        .lock()
        .unwrap()
        .register(&json!({"id": "big", "path": big.to_string_lossy()}), "main");
    assert!(matches!(res, Err(Reject::Limit("blob_bytes"))));
    assert!(!big.exists());
    // Invalid mime — also post-containment.
    let badmime = dir.join("badmime.bin");
    std::fs::write(&badmime, b"1").unwrap();
    let res = r.lock().unwrap().register(
        &json!({"id": "bm", "path": badmime.to_string_lossy(), "mime": "bad\nmime"}),
        "main",
    );
    assert!(matches!(res, Err(Reject::Bad(_))));
    assert!(!badmime.exists());
    // A refused payload is not ours only when a live blob aliases the
    // path — then the unlink is skipped and the live blob still serves.
    // (`max_blobs_per_window = 1` forces the rejection on `a2`.)
    let live = reg_blob(&r, &dir, "a1", "main", b"1");
    let res = r
        .lock()
        .unwrap()
        .register(&json!({"id": "a2", "path": live.to_string_lossy()}), "main");
    assert!(matches!(res, Err(Reject::Limit("blob_handles"))));
    assert!(live.exists());
    let res = serve(&r, "main", &req("/b/a1", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::OK);
}

#[test]
fn head_serves_headers_only() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"12345678");
    let req = http::Request::builder()
        .method("HEAD")
        .uri("/b/a1")
        .header("Origin", APP_ORIGIN)
        .body(Vec::new())
        .unwrap();
    let res = serve(&r, "main", &req);
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.body().is_empty());
    assert_eq!(hdr(res.headers(), "content-length"), "8");
    // POST is refused.
    let req = http::Request::builder()
        .method("POST")
        .uri("/b/a1")
        .header("Origin", APP_ORIGIN)
        .body(Vec::new())
        .unwrap();
    assert_eq!(
        serve(&r, "main", &req).status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[test]
fn zero_byte_blob_no_panic_right_statuses() {
    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "empty", "main", b"");
    // Any Range on an empty blob is 416 with the object's extent —
    // bytes=-5 once underflowed `total - 1` and panicked under the lock.
    for range in ["bytes=0-", "bytes=-5", "bytes=0-0"] {
        let res = serve(
            &r,
            "main",
            &req("/b/empty", &[("Origin", APP_ORIGIN), ("Range", range)]),
        );
        assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(hdr(res.headers(), "content-range"), "bytes */0");
        assert_base_headers(res.headers());
    }
    // No Range → a normal empty 200.
    let res = serve(&r, "main", &req("/b/empty", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.body().is_empty());
    // The registry still works — the panic would have poisoned its mutex.
    reg_blob(&r, &dir, "after", "main", b"1");
    let res = serve(&r, "main", &req("/b/after", &[("Origin", APP_ORIGIN)]));
    assert_eq!(res.status(), StatusCode::OK);
}

#[test]
fn range_on_oversized_blob_serves_exactly_the_cap() {
    let mut l = small_limits();
    l.max_range_bytes = 32;
    let (r, dir) = fixture(l);
    // 100 bytes > the 8-byte full-body cap: unreadable whole, readable in
    // capped pieces — and the 206 must carry ONLY the capped window, not
    // a whole-file read (bounded IO is the point of the two-phase serve).
    let data: Vec<u8> = (0..100u8).collect();
    reg_blob(&r, &dir, "big", "main", &data);
    let res = serve(
        &r,
        "main",
        &req("/b/big", &[("Origin", APP_ORIGIN), ("Range", "bytes=10-")]),
    );
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.body().len(), 32);
    assert_eq!(hdr(res.headers(), "content-range"), "bytes 10-41/100");
    assert_eq!(res.body(), &data[10..42]);
}

#[test]
fn register_from_another_thread_while_a_read_is_in_progress() {
    let (r, dir) = fixture(small_limits());
    // A blob read large enough to span the concurrent register below.
    let data = vec![0xABu8; 8 * 1024 * 1024];
    reg_blob(&r, &dir, "big", "main", &data);
    // Phase 1 under the lock — the guard is dropped before phase 2 IO.
    let plan = {
        let reg = r.lock().unwrap();
        reg.prepare_serve(
            "main",
            &[APP_ORIGIN.to_string()],
            &req("/b/big", &[("Origin", APP_ORIGIN), ("Range", "bytes=0-")]),
        )
    };
    let r2 = r.clone();
    let dir2 = dir.clone();
    let writer = std::thread::spawn(move || {
        // If the scheme handler still held the lock across its disk IO,
        // this lock() would sit behind the whole multi-MiB read.
        for i in 0..50 {
            let p = dir2.join(format!("t{i}.bin"));
            std::fs::write(&p, b"x").unwrap();
            let res = r2.lock().unwrap().register(
                &serde_json::json!({"id": format!("t{i}"), "path": p.to_string_lossy()}),
                "main",
            );
            assert!(res.is_ok(), "register {i} failed: {res:?}");
        }
    });
    let res = tauri_plugin_pylair::blobs::BlobRegistry::finish_serve(plan);
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    // 8-byte full-body cap doesn't apply to ranges; range cap is 4 →
    // exactly 4 bytes served, from a bounded read of a bounded window.
    assert_eq!(res.body().len(), 4);
    writer.join().unwrap();
    assert_eq!(r.lock().unwrap().len(), 51);
}

#[test]
fn generation_lifecycle_sweep_and_end() {
    // Stale gen dirs are swept at startup; the live one ends empty.
    let root = tmp("root");
    for g in ["gen-9", "gen-10", "not-a-gen"] {
        std::fs::create_dir_all(root.join(g).join("keep")).unwrap();
    }
    let removed = BlobRegistry::sweep(&root);
    assert_eq!(removed, 2);
    assert!(root.join("not-a-gen").exists());
    assert!(!root.join("gen-9").exists());

    let (r, dir) = fixture(small_limits());
    reg_blob(&r, &dir, "a1", "main", b"12345678");
    let (new_dir, canon) = BlobRegistry::generation_dir(dir.parent(), 2).unwrap();
    assert!(new_dir.exists());
    assert_eq!(new_dir.canonicalize().unwrap(), canon);
    let mut reg = r.lock().unwrap();
    reg.end_generation();
    assert!(reg.is_empty());
    // The session deletes the dir itself on generation end.
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists());
}
