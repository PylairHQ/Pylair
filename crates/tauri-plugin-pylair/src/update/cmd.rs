//! The `python_update_*` IPC commands (decision 0011 §2) — compiled
//! only under `feature = "updater"` so apps without `--with-updater`
//! never pull tauri-plugin-updater.

use super::{
    is_skipped, write_pending, PendingFile, UpdateGate, CACHED_ARTIFACT_FILE, CACHE_DIR,
    UPDATE_OK_FILE,
};
use crate::{protocol, ErrorObj};
use serde_json::{json, Value};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Stable machine codes carried in `data.code` — the client's
/// `UpdateError.code`. Keep additive; never rename.
pub mod codes {
    /// App Translocation (macOS), no updater configured, or a build
    /// shape that cannot be updated in place.
    pub const NOT_UPDATABLE: &str = "NotUpdatable";
    /// Per-machine install / rollback needs an elevated approval.
    pub const NEEDS_APPROVAL: &str = "NeedsApproval";
    /// Manifest or artifact signature failed verification.
    pub const SIGNATURE_INVALID: &str = "SignatureInvalid";
    /// `download` or `install` was called with no checked offer.
    pub const NO_OFFER: &str = "NoOffer";
    /// The update/rollback endpoint could not be reached or parsed.
    pub const ENDPOINT: &str = "EndpointError";
    /// `check()` itself failed (network, manifest, pubkey).
    pub const CHECK: &str = "CheckFailed";
    pub const DOWNLOAD: &str = "DownloadFailed";
    /// The rollback cache could not be fetched, verified or written.
    pub const CACHE: &str = "CacheFailed";
    pub const INSTALL: &str = "InstallFailed";
}

fn err(code: &'static str, message: impl Into<String>) -> ErrorObj {
    ErrorObj {
        code: protocol::UPDATE_FAILED,
        message: message.into(),
        data: Some(json!({"code": code})),
    }
}

fn gate<R: Runtime>(app: &AppHandle<R>) -> Result<Arc<UpdateGate>, ErrorObj> {
    app.try_state::<Arc<UpdateGate>>()
        .map(|s| (*s).clone())
        .ok_or_else(|| {
            err(
                codes::NOT_UPDATABLE,
                "app was not built with update support (no plugins.updater config)",
            )
        })
}

/// macOS App Translocation (§2): a .app running from a translocated
/// read-only path cannot update in place — refuse before any download.
fn refuse_translocated() -> Result<(), ErrorObj> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(exe) = std::env::current_exe() {
            if exe.to_string_lossy().contains("/AppTranslocation/") {
                return Err(err(
                    codes::NOT_UPDATABLE,
                    "app is running under macOS App Translocation — move it to /Applications to enable updates",
                ));
            }
        }
    }
    Ok(())
}

/// The install location the watchdog will restore to (§2): the
/// AppImage path when running from one, the `.app` bundle on macOS,
/// the exe itself on Windows (the unit NSIS reinstalls — the restore
/// waits for it to reappear and the relaunch spawns it), the exe's
/// directory elsewhere. `install_path` in pending.json must equal the
/// watchdog's own resolution of this — a mismatch aborts the restore
/// rather than writing into the wrong place.
fn install_path() -> Result<PathBuf, ErrorObj> {
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        if !appimage.is_empty() {
            return Ok(PathBuf::from(appimage));
        }
    }
    let exe = std::env::current_exe().map_err(|e| {
        err(
            codes::NOT_UPDATABLE,
            format!("cannot resolve current exe: {e}"),
        )
    })?;
    #[cfg(target_os = "macos")]
    {
        if let Some(app) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
        {
            return Ok(app.to_path_buf());
        }
    }
    #[cfg(target_os = "windows")]
    {
        Ok(exe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    exe.parent().map(Path::to_path_buf).ok_or_else(|| {
        err(
            codes::NOT_UPDATABLE,
            "cannot resolve install location from the running executable",
        )
    })
}

/// The `Update` stashed by check is taken out while in use and put
/// back afterwards — `Update` is not `Clone`, and a std-Mutex guard
/// cannot live across `.await` in a command future.
fn take_offer(gate: &UpdateGate) -> Result<Update, ErrorObj> {
    gate.offer
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| err(codes::NO_OFFER, "call update.check() first"))
}

/// `{version, notes, sha256}` for the newest offered update, or `null`
/// when none exists or the artifact is on the skip list (§3 — the
/// list is consulted BEFORE offering, so a rolled-back artifact is
/// never re-presented).
#[tauri::command]
pub async fn python_update_check<R: Runtime>(app: AppHandle<R>) -> Result<Value, ErrorObj> {
    let gate = gate(&app)?;
    refuse_translocated()?;
    let updater = app
        .updater()
        .map_err(|e| err(codes::CHECK, format!("updater unavailable: {e}")))?;
    let update = updater
        .check()
        .await
        .map_err(|e| err(codes::CHECK, e.to_string()))?;
    let Some(u) = update else {
        return Ok(Value::Null);
    };
    let sha256 = u
        .raw_json
        .get("sha256")
        .and_then(Value::as_str)
        .map(String::from);
    if is_skipped(&gate.dir, &u.version, sha256.as_deref()) {
        return Ok(Value::Null);
    }
    let reply = json!({
        "version": u.version,
        "notes": u.body,
        "sha256": sha256,
        "current": u.current_version,
    });
    *gate.offer.lock().unwrap() = Some(u);
    *gate.downloaded.lock().unwrap() = None;
    Ok(reply)
}

/// Download the checked update; progress rides `on_event` in
/// upstream's event shapes (`Started{contentLength}`, `Progress
/// {chunkLength}`, `Finished`).
#[tauri::command]
pub async fn python_update_download<R: Runtime>(
    app: AppHandle<R>,
    on_event: Channel<Value>,
) -> Result<(), ErrorObj> {
    let gate = gate(&app)?;
    let update = take_offer(&gate)?;
    let mut started = false;
    let ev = on_event;
    let bytes = update
        .download(
            |chunk, total| {
                if !started {
                    let _ = ev.send(json!({
                        "event": "Started",
                        "data": {"contentLength": total},
                    }));
                    started = true;
                }
                let _ = ev.send(json!({
                    "event": "Progress",
                    "data": {"chunkLength": chunk},
                }));
            },
            || {
                let _ = ev.send(json!({"event": "Finished", "data": {}}));
            },
        )
        .await
        .map_err(|e| err(codes::DOWNLOAD, e.to_string()))?;
    *gate.downloaded.lock().unwrap() = Some(bytes);
    *gate.offer.lock().unwrap() = Some(update);
    Ok(())
}

/// The rollback manifest URL for a rollbackUrl template + current
/// version (§6): `{{version}}` substitutes the version being replaced.
/// Unset refuses — no rollback manifest means no fallback to offer.
#[doc(hidden)]
pub fn rollback_manifest_url(template: Option<&str>, version: &str) -> Result<String, ErrorObj> {
    let template = template.ok_or_else(|| {
        err(
            codes::ENDPOINT,
            "plugins.updater.rollbackUrl is not set — updates require a \
             rollback manifest (no rollback → no install, decision 0011 §6)",
        )
    })?;
    Ok(template.replace("{{version}}", version))
}

/// Fetch the rollback manifest via `plugins.updater.rollbackUrl` and
/// cache the current version's signed artifact under
/// `updater-state/cache/` — outside the install dir, so an in-place
/// install cannot clobber the fallback. Each release's own
/// `latest.json` is its rollback manifest (§6): `{{version}}` in the
/// template becomes the *current* version. No rollback manifest means
/// no fallback, so install refuses rather than updating unguarded.
/// Returns `(artifact_path, sha256_hex, sig)` for pending.json.
async fn cache_current_artifact<R: Runtime>(
    app: &AppHandle<R>,
    gate: &UpdateGate,
) -> Result<(PathBuf, String, String), ErrorObj> {
    let url = rollback_manifest_url(gate.rollback_url.as_deref(), &gate.version)?;
    let fetch = url.clone();
    let body: Value = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        ureq::get(&fetch)
            .call()
            .map_err(|e| e.to_string())?
            .into_json()
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| err(codes::CACHE, e.to_string()))?
    .map_err(|e| {
        err(
            codes::ENDPOINT,
            format!("no rollback manifest at {url}: {e}"),
        )
    })?;
    // restore_update takes the full manifest, re-validates it and
    // picks the entry for the current target; download() verifies
    // signature + hash against it before returning bytes. The
    // comparator MUST be equality: upstream's default is `version >
    // current`, under which the running version's own manifest is
    // never "an update" and the lookup returns None (found by the
    // PR-4 acceptance leg). A comparator the app set is overridden
    // here deliberately — the rollback manifest is defined as the
    // current version's own latest.json (§6).
    let updater = app
        .updater_builder()
        .version_comparator(|current, release| release.version == current)
        .build()
        .map_err(|e| err(codes::CACHE, format!("updater unavailable: {e}")))?;
    let rollback = updater
        .restore_update(body)
        .map_err(|e| {
            err(
                codes::SIGNATURE_INVALID,
                format!("rollback manifest failed validation: {e}"),
            )
        })?
        .ok_or_else(|| {
            err(
                codes::CACHE,
                format!(
                    "rollback manifest has no platform entry for v{}",
                    gate.version
                ),
            )
        })?;
    let sig = rollback.signature.clone();
    let bytes = rollback
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| err(codes::CACHE, format!("rollback artifact fetch failed: {e}")))?;
    let sha256 = format!("{:x}", sha2::Sha256::digest(&bytes));
    let path = gate.dir.join(CACHE_DIR).join(CACHED_ARTIFACT_FILE);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)
            .map_err(|e| err(codes::CACHE, format!("create {}: {e}", d.display())))?;
    }
    std::fs::write(&path, &bytes)
        .map_err(|e| err(codes::CACHE, format!("write {}: {e}", path.display())))?;
    Ok((path, sha256, sig))
}

/// Stage the watchdog binary outside the install dir and spawn it
/// detached in watchdog mode (argv only, never env — §1). Windows and
/// macOS run a state-dir copy of the running exe; Linux runs the
/// cached rollback AppImage itself (self-contained libs — a bare exe
/// copy could not start). Either way the watchdog IS the app binary,
/// so the trust anchor (compiled-in `plugins.updater.pubkey`) travels
/// with it (§2/§6).
fn spawn_watchdog(gate: &UpdateGate, p: &PendingFile) -> Result<(), ErrorObj> {
    let bin = super::upwatch::stage_binary(&gate.dir, p).map_err(|e| err(codes::INSTALL, e))?;
    let args = super::upwatch::spawn_args(&gate.dir, &p.session_id, Path::new(&p.install_path));
    std::process::Command::new(&bin)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            err(
                codes::INSTALL,
                format!("spawn watchdog {}: {e}", bin.display()),
            )
        })?;
    Ok(())
}

/// Arm the watchdog contract and install. Everything the rollback
/// needs — cached artifact, pending.json, the spawned watchdog — is in
/// place BEFORE `install()` runs, because on Windows that call never
/// returns (§1).
#[tauri::command]
pub async fn python_update_install<R: Runtime>(app: AppHandle<R>) -> Result<(), ErrorObj> {
    let gate = gate(&app)?;
    refuse_translocated()?;
    // §2: a per-machine install PROCEEDS — its NSIS installer prompts
    // UAC once, which is acceptable since the user initiated the
    // update. The approval-needed state only exists on the ROLLBACK
    // side: install_mode travels in pending.json so the watchdog knows
    // a silent restore is impossible and records
    // rollback-pending-approval instead.
    let bytes = gate
        .downloaded
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| err(codes::NO_OFFER, "call update.download() first"))?;
    let update = take_offer(&gate)?;
    let (artifact, sha256, sig) = cache_current_artifact(&app, &gate).await?;
    let pending = PendingFile {
        from: gate.version.clone(),
        to: update.version.clone(),
        to_sha256: update
            .raw_json
            .get("sha256")
            .and_then(Value::as_str)
            .map(String::from),
        artifact: artifact.to_string_lossy().to_string(),
        sha256,
        sig,
        install_path: install_path()?.to_string_lossy().to_string(),
        session_id: gate.session_id.clone(),
        install_mode: Some(gate.install_mode.clone()),
        at_ms: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        ),
        knobs: gate.knobs,
    };
    write_pending(&gate.dir, &pending)
        .map_err(|e| err(codes::INSTALL, format!("write pending.json: {e}")))?;
    // A new pending window supersedes any prior confirmation.
    let _ = std::fs::remove_file(gate.dir.join(UPDATE_OK_FILE));
    // The watchdog must be armed BEFORE install: on Windows install
    // never returns, so it is the only component that can observe the
    // new version's first boot (§2).
    spawn_watchdog(&gate, &pending)?;
    gate.set_installing(true);
    update
        .install(&bytes)
        .map_err(|e| err(codes::INSTALL, e.to_string()))?;
    Ok(())
}

/// Relaunch into the installed version (`app.restart()` — the updater
/// plugin's restart works on all three OSes).
#[tauri::command]
pub async fn python_update_relaunch<R: Runtime>(app: AppHandle<R>) -> Result<(), ErrorObj> {
    gate(&app)?;
    app.restart();
}
