//! The updater-state file contract (decision 0011 §2): pending.json
//! fields, the skip list, boot/ready/failed markers and the health
//! phase derivation — everything the PR-3 watchdog will read. These
//! tests run without the `updater` feature: the file contract is
//! pylair's own machinery, independent of tauri-plugin-updater.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri_plugin_pylair::update::{
    self, gate::GateOpts, Knobs, PendingFile, UpdateGate, UpdatePhase,
};

fn tmp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pylair-update-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn pending(from: &str, to: &str) -> PendingFile {
    PendingFile {
        from: from.into(),
        to: to.into(),
        to_sha256: Some("newsha".into()),
        artifact: "C:\\cache\\artifact".into(),
        sha256: "cachedsha".into(),
        sig: "sigb64".into(),
        install_path: "C:\\install".into(),
        session_id: "sess-1".into(),
        install_mode: Some("current-user".into()),
        at_ms: Some(1),
        knobs: Knobs::default(),
    }
}

fn gate(dir: &Path, version: &str) -> Arc<UpdateGate> {
    UpdateGate::with_dir(GateOpts {
        dir: dir.to_path_buf(),
        version: version.into(),
        knobs: Knobs::default(),
        install_mode: "current-user".into(),
        endpoints: vec!["https://u.example.com/latest.json".into()],
        session_id: Some("sess-1".into()),
        grace_ms: 20,
    })
}

#[test]
fn pending_roundtrip_carries_all_fields() {
    let dir = tmp();
    let p = pending("1.0.0", "1.1.0");
    update::write_pending(&dir, &p).unwrap();
    let back = update::read_pending(&dir).unwrap();
    assert_eq!(back.from, "1.0.0");
    assert_eq!(back.to, "1.1.0");
    assert_eq!(back.to_sha256.as_deref(), Some("newsha"));
    assert_eq!(back.artifact, "C:\\cache\\artifact");
    assert_eq!(back.sha256, "cachedsha");
    assert_eq!(back.sig, "sigb64");
    assert_eq!(back.install_path, "C:\\install");
    assert_eq!(back.session_id, "sess-1");
    assert_eq!(back.install_mode.as_deref(), Some("current-user"));
    assert_eq!(back.knobs, Knobs::default());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn knobs_default_and_conf_override() {
    assert_eq!(Knobs::default().heartbeat_ms, 60_000);
    assert_eq!(Knobs::default().max_boots, 3);
    assert_eq!(Knobs::default().hard_cap_ms, 600_000);
    let conf = json!({"update": {"heartbeatMs": 5000, "hardCapMs": 120000}});
    let k = Knobs::from_conf(Some(&conf));
    assert_eq!(k.heartbeat_ms, 5_000);
    assert_eq!(k.max_boots, 3); // unset keys keep defaults
    assert_eq!(k.hard_cap_ms, 120_000);
    assert_eq!(Knobs::from_conf(None), Knobs::default());
}

#[test]
fn skip_list_matches_version_and_artifact() {
    let dir = tmp();
    fs::write(
        dir.join("skipped.json"),
        json!([{"version": "1.2.0", "sha256": "bad"}, {"version": "1.3.0"}]).to_string(),
    )
    .unwrap();
    assert!(update::is_skipped(&dir, "1.2.0", Some("bad")));
    // Same version, different artifact → a re-cut build is offerable.
    assert!(!update::is_skipped(&dir, "1.2.0", Some("good")));
    // Version-only entry skips any artifact of that version.
    assert!(update::is_skipped(&dir, "1.3.0", Some("any")));
    assert!(update::is_skipped(&dir, "1.3.0", None));
    assert!(!update::is_skipped(&dir, "1.4.0", Some("bad")));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_marker_written_per_launch_of_pending_version() {
    let dir = tmp();
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    // Running 1.1.0 — a boot of the pending version: marker lands.
    let _g = gate(&dir, "1.1.0");
    let _g2 = gate(&dir, "1.1.0");
    let boots = fs::read_dir(dir.join("boots")).unwrap().count();
    assert_eq!(boots, 2);
    // Running the OLD version (install never applied): no marker —
    // boots only count the pending version's launches.
    let _g3 = gate(&dir, "1.0.0");
    let boots = fs::read_dir(dir.join("boots")).unwrap().count();
    assert_eq!(boots, 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ready_marker_written_when_all_components_ready() {
    let dir = tmp();
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    let g = gate(&dir, "1.1.0");
    g.note_component_pub("model", "loading");
    assert!(!dir.join("ready-1.1.0.json").exists());
    g.note_component_pub("model", "ready");
    let marker: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("ready-1.1.0.json")).unwrap()).unwrap();
    assert_eq!(marker["version"], "1.1.0");
    assert_eq!(marker["artifact_sha256"], "newsha");
    assert_eq!(marker["session_id"], "sess-1");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ready_marker_carries_pendings_current_session() {
    // A newer pending supersedes: the marker keys on the pending's
    // CURRENT session_id, not the one remembered at arm time (§2).
    let dir = tmp();
    let mut p = pending("1.0.0", "1.1.0");
    p.session_id = "sess-2".into();
    update::write_pending(&dir, &p).unwrap();
    let g = gate(&dir, "1.1.0"); // gate was built under sess-1
    g.note_component_pub("ui", "ready");
    let marker: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("ready-1.1.0.json")).unwrap()).unwrap();
    assert_eq!(marker["session_id"], "sess-2");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_ready_marker_for_old_version_or_absent_pending() {
    let dir = tmp();
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    // Old version booted despite a pending.json (install failed to
    // apply): readiness must NOT confirm the pending update.
    let g = gate(&dir, "1.0.0");
    g.note_component_pub("ui", "ready");
    assert!(!dir.join("ready-1.1.0.json").exists());
    let _ = fs::remove_dir_all(&dir);

    let dir = tmp();
    let g = gate(&dir, "1.1.0");
    g.note_component_pub("ui", "ready");
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn failed_component_writes_failed_marker() {
    let dir = tmp();
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    let g = gate(&dir, "1.1.0");
    g.note_component_pub("model", "failed");
    let marker: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("failed-1.1.0.json")).unwrap()).unwrap();
    assert_eq!(marker["version"], "1.1.0");
    assert_eq!(marker["session_id"], "sess-1");
    assert_eq!(marker["component"], "model");
    assert!(!dir.join("ready-1.1.0.json").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn non_instrumented_app_confirms_after_grace() {
    let dir = tmp();
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    let g = gate(&dir, "1.1.0");
    g.note_ready_pub();
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(dir.join("ready-1.1.0.json").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn grace_abandoned_when_component_reports_or_generation_dies() {
    let dir = tmp();
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    let g = gate(&dir, "1.1.0");
    g.note_ready_pub();
    // A component arrives inside the grace window: the grace path
    // must not fire — the component rule now owns confirmation.
    std::thread::sleep(std::time::Duration::from_millis(5));
    g.note_component_pub("ui", "loading");
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(!dir.join("ready-1.1.0.json").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn phase_derivation() {
    let dir = tmp();
    assert_eq!(update::phase(&dir, false), UpdatePhase::None);
    fs::write(dir.join("pending.json"), "{}").unwrap();
    assert_eq!(update::phase(&dir, false), UpdatePhase::Pending);
    assert_eq!(update::phase(&dir, true), UpdatePhase::Installing);
    fs::write(dir.join("update-ok"), "").unwrap();
    fs::remove_file(dir.join("pending.json")).unwrap();
    assert_eq!(update::phase(&dir, false), UpdatePhase::Ready);
    let _ = fs::remove_dir_all(&dir);

    let dir = tmp();
    fs::write(dir.join("rollback-reason.txt"), "{}").unwrap();
    assert_eq!(update::phase(&dir, false), UpdatePhase::RolledBack);
    fs::write(dir.join("rollback-pending-approval"), "{}").unwrap();
    assert_eq!(
        update::phase(&dir, false),
        UpdatePhase::RollbackPendingApproval
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn health_update_reports_state_and_last_rollback() {
    let dir = tmp();
    fs::write(
        dir.join("rollback-reason.txt"),
        json!({"from": "1.0.0", "to": "1.1.0", "reason": "crash-loop", "at": 7}).to_string(),
    )
    .unwrap();
    let u = update::health_update(&dir, false);
    assert_eq!(u["state"], "rolled-back");
    assert_eq!(u["lastRollback"]["reason"], "crash-loop");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn component_state_absent_when_nothing_pending() {
    let dir = tmp();
    let g = gate(&dir, "1.1.0");
    assert_eq!(g.component_state(), None);
    update::write_pending(&dir, &pending("1.0.0", "1.1.0")).unwrap();
    assert_eq!(g.component_state(), Some("pending"));
    g.set_installing(true);
    assert_eq!(g.component_state(), Some("installing"));
    assert_eq!(g.health()["state"], "installing");
    let _ = fs::remove_dir_all(&dir);
}
