//! The update watchdog's decision logic + file contract (decision
//! 0011 §2): verdicts for ready/failed/crash-loop/silence/hard-cap,
//! artifact re-verification, the install_path trust check, skip-list
//! ordering, the single-instance lock and the per-machine approval
//! path. Runs only with `--features updater`.
//!
//! Signing uses throwaway `minisign` keypairs generated per run —
//! nothing is committed, nothing real is touched.
#![cfg(feature = "updater")]

use base64::Engine;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tauri_plugin_pylair::update::upwatch::{
    parse_args, run, stage_binary, Hooks, Platform, RestorePlan, Verdict, WatchArgs, WATCH_FLAG,
};
use tauri_plugin_pylair::update::{
    self, write_pending, Knobs, PendingFile, APPROVAL_FILE, BOOTS_DIR, CACHE_DIR, PENDING_FILE,
    PROGRESS_FILE, ROLLBACK_REASON_FILE, SKIPPED_FILE, UPDATE_OK_FILE,
};

fn tmp() -> PathBuf {
    // Counter + nanos: parallel tests in one process get unique dirs
    // even where SystemTime granularity is coarse (Windows ~15ms).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pylair-upwatch-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- throwaway signing -------------------------------------------------------

struct Keys {
    pubkey_b64: String,
    kp: minisign::KeyPair,
}

fn keys() -> Keys {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    Keys {
        pubkey_b64: kp.pk.to_base64(),
        kp,
    }
}

/// Sign `bytes` into the manifest/`pending.sig` wire shape: base64 of
/// the multi-line .sig text (what `pylair update manifest` copies out
/// of `<artifact>.sig`).
fn sign(k: &Keys, bytes: &[u8]) -> String {
    let sb = minisign::sign(
        Some(&k.kp.pk),
        &k.kp.sk,
        bytes,
        Some("timestamp:1\tfile:test\tprehashed"),
        None,
    )
    .unwrap();
    base64::engine::general_purpose::STANDARD.encode(sb.to_string())
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

// ---- fixtures -----------------------------------------------------------------

/// A pending window wired to a real signed artifact under `cache/`.
/// `install_path` is the fake installed app's path (a file, posix-style;
/// the hooks substitute the restore on Windows).
struct Fixture {
    dir: PathBuf,
    pending: PendingFile,
    install_path: PathBuf,
    keys: Keys,
}

fn fixture(knobs: Knobs) -> Fixture {
    let dir = tmp();
    let keys = keys();
    let artifact_bytes = b"fake previous-version bundle bytes".to_vec();
    let artifact = dir.join(CACHE_DIR).join("rollback-artifact");
    fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    fs::write(&artifact, &artifact_bytes).unwrap();
    let install_path = dir.join("install").join("app-binary");
    fs::create_dir_all(install_path.parent().unwrap()).unwrap();
    fs::write(&install_path, b"fake new-version binary").unwrap();
    let pending = PendingFile {
        from: "1.0.0".into(),
        to: "1.1.0".into(),
        to_sha256: Some("new-version-sha".into()),
        artifact: artifact.to_string_lossy().to_string(),
        sha256: sha256(&artifact_bytes),
        sig: sign(&keys, &artifact_bytes),
        install_path: install_path.to_string_lossy().to_string(),
        session_id: "sess-test".into(),
        install_mode: Some("current-user".into()),
        at_ms: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        ),
        knobs,
    };
    write_pending(&dir, &pending).unwrap();
    Fixture {
        dir,
        pending,
        install_path,
        keys,
    }
}

/// Fast hooks for tests: near-instant polling, recording
/// restore/relaunch calls, fake pid liveness.
struct TestHooks {
    hooks: Hooks,
    calls: Rc<RefCell<Vec<String>>>,
}

fn test_hooks() -> TestHooks {
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let rec = calls.clone();
    let rel = calls.clone();
    TestHooks {
        calls,
        hooks: Hooks {
            now_ms: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }),
            // ~4ms polls: heartbeat/hard-cap knobs in the 50-200ms
            // range resolve in well under a second of real time.
            sleep: Box::new(|ms| std::thread::sleep(Duration::from_millis(ms.min(4)))),
            pid_alive: Box::new(|_| false),
            restore: Box::new(move |plan: &RestorePlan| {
                rec.borrow_mut()
                    .push(format!("restore:{}", plan.install_path.display()));
                Ok(())
            }),
            relaunch: Box::new(move |p: &Path| {
                rel.borrow_mut().push(format!("relaunch:{}", p.display()));
                Ok(())
            }),
            install_wait: Duration::from_millis(300),
            platform: platform(),
        },
    }
}

#[cfg(windows)]
const fn platform() -> Platform {
    Platform::Windows
}
#[cfg(not(windows))]
const fn platform() -> Platform {
    Platform::Posix
}

fn args(f: &Fixture) -> WatchArgs {
    WatchArgs {
        state_dir: f.dir.clone(),
        session_id: f.pending.session_id.clone(),
        install_path: f.install_path.clone(),
    }
}

fn knobs(hb: u64, boots: u64, cap: u64) -> Knobs {
    Knobs {
        heartbeat_ms: hb,
        max_boots: boots,
        hard_cap_ms: cap,
    }
}

fn write_ready(f: &Fixture, session: &str) {
    fs::write(
        f.dir.join(format!("ready-{}.json", f.pending.to)),
        json!({
            "version": f.pending.to,
            "artifact_sha256": f.pending.to_sha256,
            "session_id": session,
            "at_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
}

fn write_failed(f: &Fixture, session: &str, component: &str) {
    fs::write(
        f.dir.join(format!("failed-{}.json", f.pending.to)),
        json!({
            "version": f.pending.to,
            "session_id": session,
            "component": component,
            "at_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
}

fn write_boot(f: &Fixture, session: &str) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    fs::create_dir_all(f.dir.join(BOOTS_DIR)).unwrap();
    fs::write(
        f.dir.join(BOOTS_DIR).join(format!("{nanos}-1.json")),
        json!({"version": f.pending.to, "session_id": session, "pid": 1, "at_ms": 1}).to_string(),
    )
    .unwrap();
}

fn reason(f: &Fixture) -> Value {
    serde_json::from_str(&fs::read_to_string(f.dir.join(ROLLBACK_REASON_FILE)).unwrap()).unwrap()
}

// ---- argv parsing ------------------------------------------------------------

#[test]
fn parse_args_requires_the_flag_and_three_contract_args() {
    let none = parse_args(&["app".into(), "--self-test".into()]);
    assert!(none.is_none());
    let full = parse_args(&[
        "app".into(),
        WATCH_FLAG.into(),
        "--state-dir".into(),
        "/sd".into(),
        "--session-id".into(),
        "s1".into(),
        "--install-path".into(),
        "/inst".into(),
    ])
    .unwrap();
    assert_eq!(full.state_dir, PathBuf::from("/sd"));
    assert_eq!(full.session_id, "s1");
    assert_eq!(full.install_path, PathBuf::from("/inst"));
    // Missing contract args are not a watchdog session.
    assert!(parse_args(&["app".into(), WATCH_FLAG.into()]).is_none());
}

// ---- confirm -------------------------------------------------------------------

#[test]
fn confirm_writes_update_ok_and_clears_pending() {
    let f = fixture(knobs(60_000, 3, 600_000));
    write_ready(&f, "sess-test");
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    assert_eq!(v, Verdict::Confirmed);
    assert!(f.dir.join(UPDATE_OK_FILE).exists());
    assert!(!f.dir.join(PENDING_FILE).exists());
    assert!(h.calls.borrow().is_empty());
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn stale_session_ready_marker_is_ignored() {
    let f = fixture(knobs(80, 3, 10_000));
    write_ready(&f, "sess-older-generation");
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    // The stale ready was ignored → silence → rollback.
    match v {
        Verdict::RolledBack { reason } => assert!(reason.starts_with("no_sign_of_life")),
        other => panic!("expected rollback, got {other:?}"),
    }
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn stale_session_watchdog_exits_without_touching() {
    let f = fixture(knobs(80, 3, 10_000));
    let mut a = args(&f);
    a.session_id = "sess-superseded".into();
    let h = test_hooks();
    let v = run(&a, &f.keys.pubkey_b64, &h.hooks);
    assert_eq!(v, Verdict::Confirmed); // exits; pending untouched
    assert!(f.dir.join(PENDING_FILE).exists());
    assert!(!f.dir.join(SKIPPED_FILE).exists());
    let _ = fs::remove_dir_all(&f.dir);
}

// ---- rollback triggers -----------------------------------------------------------

#[test]
fn failed_marker_rolls_back_with_component() {
    let f = fixture(knobs(60_000, 3, 600_000));
    write_failed(&f, "sess-test", "model");
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    match v {
        Verdict::RolledBack { reason } => assert_eq!(reason, "readiness_failed:model"),
        other => panic!("expected rollback, got {other:?}"),
    }
    assert!(f.dir.join(SKIPPED_FILE).exists());
    assert!(h.calls.borrow()[0].starts_with("restore:"));
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn crash_loop_at_max_boots() {
    let f = fixture(knobs(60_000, 2, 600_000));
    write_boot(&f, "sess-test");
    write_boot(&f, "sess-test");
    write_boot(&f, "sess-other"); // another generation doesn't count
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    match v {
        Verdict::RolledBack { reason } => assert!(reason.starts_with("crash_loop:2")),
        other => panic!("expected rollback, got {other:?}"),
    }
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn silence_past_heartbeat_rolls_back() {
    let f = fixture(knobs(60, 3, 60_000));
    let h = test_hooks();
    let t = Instant::now();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    let elapsed = t.elapsed();
    match v {
        Verdict::RolledBack { reason } => assert_eq!(reason, "no_sign_of_life"),
        other => panic!("expected rollback, got {other:?}"),
    }
    assert!(elapsed >= Duration::from_millis(60));
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn live_progress_keeps_the_window_open() {
    let f = fixture(knobs(120, 3, 60_000));
    // run() blocks until a verdict, so the writer owns the clock: it
    // keeps progress.json fresh for ~3 heartbeat windows WHILE the
    // watchdog watches, then goes silent and stamps its stop instant.
    // Both bounds are relative to that instant — silence counts from
    // the last live write, never from run()'s start.
    let dir = f.dir.clone();
    let stopped = std::sync::Arc::new(std::sync::Mutex::new(Instant::now()));
    let stopped_w = stopped.clone();
    let writer = std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_millis(350);
        while Instant::now() < end {
            let _ = fs::write(dir.join(PROGRESS_FILE), "{\"tick\":1}");
            std::thread::sleep(Duration::from_millis(25));
        }
        *stopped_w.lock().unwrap() = Instant::now();
    });
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    let verdict_at = Instant::now();
    writer.join().unwrap();
    let stop_at = *stopped.lock().unwrap();
    match v {
        Verdict::RolledBack { reason } => assert_eq!(reason, "no_sign_of_life"),
        other => panic!("expected rollback, got {other:?}"),
    };
    // (b) never fired while progress was still live…
    assert!(
        verdict_at >= stop_at,
        "rollback fired at {verdict_at:?} — before the writer went silent at {stop_at:?}"
    );
    // (c) …and fired within ~one heartbeat of real silence (slack for
    // poll granularity and loaded runners).
    assert!(
        verdict_at < stop_at + Duration::from_millis(120 + 1_000),
        "rollback at {verdict_at:?} is >1s past heartbeat after silence began ({stop_at:?})"
    );
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn hard_cap_is_the_only_wall_clock() {
    let f = fixture(knobs(60_000, 3, 150));
    let dir = f.dir.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    // Even with live progress, the cap ends the window.
    let writer = std::thread::spawn(move || {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = fs::write(dir.join(PROGRESS_FILE), "{\"tick\":1}");
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    writer.join().unwrap();
    match v {
        Verdict::RolledBack { reason } => assert_eq!(reason, "hard_cap"),
        other => panic!("expected rollback, got {other:?}"),
    }
    let _ = fs::remove_dir_all(&f.dir);
}

// ---- host-side staging ----------------------------------------------------------

/// Windows/macOS stage a state-dir copy of the running exe (the AppImage
/// arm lives on the Linux leg).
#[cfg(not(target_os = "linux"))]
#[test]
fn staging_copies_the_running_exe_into_cache() {
    let f = fixture(knobs(50, 3, 60_000));
    let bin = stage_binary(&f.dir, &f.pending).unwrap();
    assert!(bin.starts_with(f.dir.join(CACHE_DIR)));
    assert!(bin
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("upwatch-sess-test"));
    assert_eq!(
        fs::read(&bin).unwrap(),
        fs::read(std::env::current_exe().unwrap()).unwrap()
    );
    let _ = fs::remove_dir_all(&f.dir);
}

/// Linux runs the cached rollback artifact itself: the old AppImage is
/// self-contained (bundled WebKitGTK) where a bare exe copy could not
/// start outside the mount the install replaced.
#[cfg(target_os = "linux")]
#[test]
fn staging_on_linux_is_the_cached_appimage_chmodded() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture(knobs(50, 3, 60_000));
    let bin = stage_binary(&f.dir, &f.pending).unwrap();
    assert_eq!(bin, PathBuf::from(&f.pending.artifact));
    assert_ne!(fs::metadata(&bin).unwrap().permissions().mode() & 0o111, 0);
    let _ = fs::remove_dir_all(&f.dir);
}

// ---- rollback mechanics ------------------------------------------------------------

#[test]
fn tampered_cache_fails_verify_and_never_restores() {
    let mut f = fixture(knobs(50, 3, 60_000));
    // Re-sign the artifact so sha256 still matches but the minisign is
    // from a DIFFERENT key — the watchdog's compiled-in pubkey rejects.
    let other = keys();
    f.pending.sig = sign(&other, &fs::read(&f.pending.artifact).unwrap());
    update::write_pending(&f.dir, &f.pending).unwrap();
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    match v {
        Verdict::Aborted { detail, .. } => assert_eq!(detail, "verify_failed"),
        other => panic!("expected abort, got {other:?}"),
    }
    assert!(h.calls.borrow().is_empty());
    let r = reason(&f);
    assert!(r["outcome"].as_str().unwrap().starts_with("verify_failed"));
    assert!(!f.dir.join(PENDING_FILE).exists());
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn corrupted_bytes_fail_sha256_before_verify() {
    let f = fixture(knobs(50, 3, 60_000));
    fs::write(&f.pending.artifact, b"tampered bytes").unwrap();
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    match v {
        Verdict::Aborted { detail, .. } => assert_eq!(detail, "verify_failed"),
        other => panic!("expected abort, got {other:?}"),
    }
    assert!(h.calls.borrow().is_empty());
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn install_path_mismatch_refuses_the_restore() {
    let mut f = fixture(knobs(50, 3, 60_000));
    f.pending.install_path = "C:\\elsewhere\\evil".into();
    update::write_pending(&f.dir, &f.pending).unwrap();
    let h = test_hooks();
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    match v {
        Verdict::Aborted { detail, .. } => assert_eq!(detail, "install_path_mismatch"),
        other => panic!("expected abort, got {other:?}"),
    }
    assert!(h.calls.borrow().is_empty());
    // The recorded "install" path was never written to.
    assert!(!Path::new("C:\\elsewhere\\evil").exists());
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn skip_list_is_written_before_restore_and_relaunch() {
    let f = fixture(knobs(50, 3, 60_000));
    let order = Rc::new(RefCell::new(Vec::<String>::new()));
    let rec = order.clone();
    let dir = f.dir.clone();
    let h_calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let rel = h_calls.clone();
    let hooks = Hooks {
        now_ms: Box::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }),
        sleep: Box::new(|ms| std::thread::sleep(Duration::from_millis(ms.min(4)))),
        pid_alive: Box::new(|_| false),
        restore: Box::new(move |_: &RestorePlan| {
            // §2: the skip entry must already be on disk when the
            // restore runs — the relaunched app re-checks immediately.
            rec.borrow_mut().push(format!(
                "restore(skip_present={})",
                dir.join(SKIPPED_FILE).exists()
            ));
            Ok(())
        }),
        relaunch: Box::new(move |_: &Path| {
            rel.borrow_mut().push("relaunch".to_string());
            Ok(())
        }),
        install_wait: Duration::from_millis(100),
        platform: platform(),
    };
    let v = run(&args(&f), &f.keys.pubkey_b64, &hooks);
    assert!(matches!(v, Verdict::RolledBack { .. }));
    assert_eq!(order.borrow().as_slice(), &["restore(skip_present=true)"]);
    assert_eq!(h_calls.borrow().as_slice(), &["relaunch"]);
    // The entry names the failed artifact.
    let skips: Value =
        serde_json::from_str(&fs::read_to_string(f.dir.join(SKIPPED_FILE)).unwrap()).unwrap();
    assert_eq!(skips[0]["version"], "1.1.0");
    assert_eq!(skips[0]["sha256"], "new-version-sha");
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn single_instance_lock_blocks_a_second_watchdog() {
    let f = fixture(knobs(50, 3, 60_000));
    fs::write(
        f.dir.join("upwatch.lock"),
        json!({"pid": 424242, "at_ms": 1}).to_string(),
    )
    .unwrap();
    let mut h = test_hooks();
    h.hooks.pid_alive = Box::new(|pid| pid == 424242);
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    assert_eq!(v, Verdict::Confirmed);
    assert!(h.calls.borrow().is_empty());
    assert!(f.dir.join(PENDING_FILE).exists()); // untouched

    // A dead holder's lock is taken over.
    h.hooks.pid_alive = Box::new(|_| false);
    write_ready(&f, "sess-test");
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    assert_eq!(v, Verdict::Confirmed);
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn per_machine_windows_records_approval_without_restoring() {
    let mut f = fixture(knobs(50, 3, 60_000));
    f.pending.install_mode = Some("per-machine".into());
    update::write_pending(&f.dir, &f.pending).unwrap();
    let mut h = test_hooks();
    h.hooks.platform = Platform::Windows;
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    match v {
        Verdict::ApprovalPending { reason } => assert_eq!(reason, "no_sign_of_life"),
        other => panic!("expected approval-pending, got {other:?}"),
    }
    assert!(f.dir.join(APPROVAL_FILE).exists());
    assert!(f.dir.join(SKIPPED_FILE).exists()); // still skipped
    assert!(h.calls.borrow().is_empty()); // no restore, no relaunch
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn per_user_windows_still_restores_silently() {
    let f = fixture(knobs(50, 3, 60_000));
    let mut h = test_hooks();
    h.hooks.platform = Platform::Windows;
    let v = run(&args(&f), &f.keys.pubkey_b64, &h.hooks);
    assert!(matches!(v, Verdict::RolledBack { .. }));
    assert!(!f.dir.join(APPROVAL_FILE).exists());
    assert_eq!(h.calls.borrow().len(), 2); // restore + relaunch
    let _ = fs::remove_dir_all(&f.dir);
}

#[test]
fn rollback_manifest_url_substitutes_version_and_refuses_when_unset() {
    use tauri_plugin_pylair::update::cmd::rollback_manifest_url;
    let url = rollback_manifest_url(
        Some("https://x.example.com/v{{version}}/latest.json"),
        "1.0.0",
    )
    .unwrap();
    assert_eq!(url, "https://x.example.com/v1.0.0/latest.json");
    let e = rollback_manifest_url(None, "1.0.0").unwrap_err();
    assert_eq!(
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str),
        Some("EndpointError")
    );
}

// ---- OS-level integration against a fake installed app -----------------------------

/// POSIX: the real restore path — the artifact file replaces the
/// install path and keeps the exec bit; the real relaunch spawns it.
#[cfg(target_os = "linux")]
#[test]
fn linux_rollback_replaces_the_appimage_and_relaunches() {
    // The fake installed app + artifact are real executables so the
    // default restore's exec bit + the default relaunch's spawn both
    // work end to end.
    let f = fixture(knobs(50, 3, 60_000));
    let real = Path::new("/bin/true");
    let artifact = f.dir.join(CACHE_DIR).join("rollback-artifact");
    fs::copy(real, &artifact).unwrap();
    let bytes = fs::read(&artifact).unwrap();
    let keys = keys();
    let mut p = f.pending.clone();
    p.sha256 = sha256(&bytes);
    p.sig = sign(&keys, &bytes);
    update::write_pending(&f.dir, &p).unwrap();

    let h = test_hooks();
    // Only the restore/relaunch mechanics are real here.
    let hooks = Hooks {
        restore: h.hooks.restore, // recording seam first…
        ..h.hooks
    };
    drop(hooks);
    // …but the REAL default restore is what this test exercises.
    let mut hooks = Hooks::default();
    hooks.sleep = Box::new(|ms| std::thread::sleep(Duration::from_millis(ms.min(4))));
    hooks.platform = Platform::Posix;
    let v = run(&args(&f), &keys.pubkey_b64, &hooks);
    match v {
        Verdict::RolledBack { reason } => assert_eq!(reason, "no_sign_of_life"),
        other => panic!("expected rollback, got {other:?}"),
    }
    // Install path now holds the artifact's bytes and is executable.
    assert_eq!(fs::read(&f.install_path).unwrap(), bytes);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&f.install_path).unwrap().permissions().mode() & 0o111,
        0o111
    );
    assert!(f.dir.join(SKIPPED_FILE).exists());
    assert!(!f.dir.join(PENDING_FILE).exists());
    let _ = fs::remove_dir_all(&f.dir);
}

/// Windows: per-machine → approval (real path); per-user → the seam
/// records the NSIS restore + relaunch calls in order.
#[cfg(windows)]
#[test]
fn windows_rollback_runs_the_silent_installer_then_relaunches() {
    let f = fixture(knobs(50, 3, 60_000));
    // The "installer" is a copy of the running test exe — invoked as
    // `<exe> /S` it exits without recreating install_path, so the real
    // default restore reports the reappear timeout after
    // install_wait. That exercises the NSIS spawn + wait mechanics for
    // real; success mechanics are covered by the seam test above.
    let exe = std::env::current_exe().unwrap();
    let artifact = f.dir.join(CACHE_DIR).join("rollback-artifact");
    fs::copy(&exe, &artifact).unwrap();
    let bytes = fs::read(&artifact).unwrap();
    let keys = keys();
    let mut p = f.pending.clone();
    p.sha256 = sha256(&bytes);
    p.sig = sign(&keys, &bytes);
    update::write_pending(&f.dir, &p).unwrap();
    // Remove install_path so the wait can't shortcut.
    fs::remove_file(&f.install_path).unwrap();

    let hooks = Hooks {
        sleep: Box::new(|ms| std::thread::sleep(Duration::from_millis(ms.min(4)))),
        install_wait: Duration::from_millis(400),
        ..Default::default()
    };
    let v = run(&args(&f), &keys.pubkey_b64, &hooks);
    match v {
        Verdict::Aborted { detail, .. } => assert_eq!(detail, "restore_failed"),
        other => panic!("expected restore_failed, got {other:?}"),
    }
    // The skip entry was still written BEFORE the failed restore —
    // re-offer protection survives a broken installer.
    assert!(f.dir.join(SKIPPED_FILE).exists());
    let _ = fs::remove_dir_all(&f.dir);
}
