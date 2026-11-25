//! In-app update file state (decision 0011).
//!
//! The host and the (PR-3) watchdog are separate processes that share
//! one directory — `<app_data_dir>/updater-state/` — and communicate
//! only through files and argv, never env vars (the §1 Windows
//! finding: an NSIS-spawned process does not inherit the installer's
//! environment). This module defines the file contract; [`gate`]
//! holds the live per-app state and [`cmd`] the IPC commands.
//!
//! ## Files
//!
//! | file | written by | meaning |
//! |---|---|---|
//! | `pending.json` | host, before install | update in flight: `{from, to, to_sha256, artifact, sha256, sig, install_path, session_id, knobs}` |
//! | `skipped.json` | watchdog | `[{version, sha256}]` artifacts that failed first-boot verification — never re-offer |
//! | `update-ok` | watchdog | pending update confirmed healthy ("confirmed" in `pylair.update.state()`) |
//! | `boots/<ms>-<pid>.json` | host, per boot of the pending version | crash-loop evidence for `max_boots` |
//! | `ready-<version>.json` | host, when `health().app` reaches ready | `{version, artifact_sha256, session_id}` — the real readiness signal |
//! | `failed-<version>.json` | host | a readiness component reported `failed` during the pending window |
//! | `rollback-reason.txt` | watchdog | JSON `{from, to, reason, at}` — the last rollback outcome |
//! | `rollback-pending-approval` | watchdog | JSON `{from, to, reason, at}` — per-machine install: rollback needs a UAC-approved restore |
//! | `cache/rollback-artifact` | host, before install | the current version's signed bundle, kept outside the install dir |

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(feature = "updater")]
pub mod cmd;
pub mod gate;
#[cfg(feature = "updater")]
pub mod upwatch;

pub use gate::UpdateGate;

/// Directory under the app data dir all update state lives in.
pub const STATE_SUBDIR: &str = "updater-state";
pub const PENDING_FILE: &str = "pending.json";
pub const SKIPPED_FILE: &str = "skipped.json";
pub const UPDATE_OK_FILE: &str = "update-ok";
pub const APPROVAL_FILE: &str = "rollback-pending-approval";
pub const ROLLBACK_REASON_FILE: &str = "rollback-reason.txt";
pub const BOOTS_DIR: &str = "boots";
pub const CACHE_DIR: &str = "cache";
/// Live readiness-progress evidence for the watchdog's heartbeat (§2):
/// the host touches this on every `app.component` report.
pub const PROGRESS_FILE: &str = "progress.json";
/// Name inside `cache/` holding the previous version's signed bundle.
pub const CACHED_ARTIFACT_FILE: &str = "rollback-artifact";

/// Decision 0011 §2 defaults — the app may override via
/// `plugins.pylair.update.{heartbeatMs, maxBoots, hardCapMs}`.
pub const DEFAULT_HEARTBEAT_MS: u64 = 60_000;
pub const DEFAULT_MAX_BOOTS: u64 = 3;
pub const DEFAULT_HARD_CAP_MS: u64 = 10 * 60_000;

/// How long the host waits after the runtime reports Ready for a
/// first `app.component` report before treating the app as
/// non-instrumented and writing the ready marker. The only wall clock
/// on the confirm path — a component-reporting app keeps its full
/// readiness surface instead.
pub const NO_COMPONENTS_GRACE_MS: u64 = 5_000;

/// The watchdog knobs copied verbatim into pending.json (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knobs {
    /// Silence window: no readiness evidence for this long ⇒ dead.
    pub heartbeat_ms: u64,
    /// Boots of the pending version that fail ⇒ crash loop.
    pub max_boots: u64,
    /// Absolute cap on the pending window even with live progress.
    pub hard_cap_ms: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            heartbeat_ms: DEFAULT_HEARTBEAT_MS,
            max_boots: DEFAULT_MAX_BOOTS,
            hard_cap_ms: DEFAULT_HARD_CAP_MS,
        }
    }
}

impl Knobs {
    /// `plugins.pylair.update.{heartbeatMs, maxBoots, hardCapMs}` from
    /// tauri.conf.json — camelCase on the wire, missing keys take the
    /// §2 defaults.
    pub fn from_conf(pylair_plugin: Option<&Value>) -> Self {
        let u = pylair_plugin.and_then(|p| p.get("update"));
        let get = |k: &str| u.and_then(|v| v.get(k)).and_then(Value::as_u64);
        Self {
            heartbeat_ms: get("heartbeatMs").unwrap_or(DEFAULT_HEARTBEAT_MS),
            max_boots: get("maxBoots").unwrap_or(DEFAULT_MAX_BOOTS),
            hard_cap_ms: get("hardCapMs").unwrap_or(DEFAULT_HARD_CAP_MS),
        }
    }
}

/// `pending.json` — everything the watchdog needs to verify and, on
/// failure, restore the previous version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFile {
    /// Version being replaced (what the cached artifact is).
    pub from: String,
    /// Version being installed.
    pub to: String,
    /// sha256 of the new artifact, from the update manifest — the
    /// identity the ready marker's `artifact_sha256` must match.
    #[serde(default)]
    pub to_sha256: Option<String>,
    /// Path to the cached previous-version artifact (outside the
    /// install dir).
    pub artifact: String,
    /// sha256 of the cached artifact bytes.
    pub sha256: String,
    /// Minisign signature of the cached artifact.
    pub sig: String,
    /// The location the watchdog restores to — must equal its own
    /// resolved install location or it aborts (§2 trust rule).
    pub install_path: String,
    /// Nonce for this pending window; a ready marker without it is
    /// stale evidence from an earlier generation.
    pub session_id: String,
    /// Effective install scope (§2): `per-machine` means a silent
    /// rollback hits UAC. Resolved from the conf-declared NSIS mode
    /// plus the install location — see `effective_install_mode` (the
    /// embedded config prunes `bundle.windows.nsis`).
    #[serde(default)]
    pub install_mode: Option<String>,
    #[serde(default)]
    pub at_ms: Option<u64>,
    pub knobs: Knobs,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh per-pending nonce: time + pid, collision-proof across
/// successive updates on one machine.
pub fn new_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", std::process::id())
}

pub fn state_dir(app_data: &Path) -> PathBuf {
    app_data.join(STATE_SUBDIR)
}

/// Whether `path` sits under `root`, comparing components
/// case-insensitively — `Path::starts_with` is case-sensitive even on
/// Windows and would miss `PROGRAM FILES` vs `Program Files`.
fn under_dir(path: &Path, root: &Path) -> bool {
    if root.as_os_str().is_empty() {
        return false;
    }
    let mut pc = path.components();
    root.components().all(|rc| {
        pc.next()
            .is_some_and(|c| c.as_os_str().eq_ignore_ascii_case(rc.as_os_str()))
    })
}

/// Whether `path` sits under a machine-wide install root
/// (`%ProgramFiles%`, `%ProgramFiles(x86)%`, `%ProgramW6432%`).
///
/// This is the runtime signal for NSIS install scope:
/// `app.config()` cannot see `bundle.windows.nsis.installMode` —
/// tauri codegen's `ToTokens` for `WindowsConfig` forwards only
/// `webview_install_mode`, so the embedded config always reports
/// `nsis: None`. A per-user install cannot write under Program
/// Files, so an install located there is definitionally per-machine;
/// a `both`-mode install that resolved per-user is not. The env vars
/// are unset on POSIX, where this always returns false.
pub fn per_machine_install_path(path: &Path) -> bool {
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(std::env::var_os)
        .any(|root| under_dir(path, Path::new(&root)))
}

/// The effective install scope for rollback purposes (§2). A
/// conf-declared `per-machine` stands; every other declared value is
/// resolved by where the install actually lives — the conf's
/// `installMode` never reaches the embedded runtime config, so the
/// location is the authoritative signal. Returns `"per-machine"` or
/// `"current-user"`.
pub fn effective_install_mode(declared: &str, install_path: &Path) -> &'static str {
    match declared {
        "per-machine" => "per-machine",
        _ if per_machine_install_path(install_path) => "per-machine",
        _ => "current-user",
    }
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json_file(path: &Path, v: &Value) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(v)?)
}

pub fn read_pending(dir: &Path) -> Option<PendingFile> {
    read_json_file(&dir.join(PENDING_FILE))
}

pub fn write_pending(dir: &Path, p: &PendingFile) -> io::Result<()> {
    let v = serde_json::to_value(p).map_err(io::Error::other)?;
    write_json_file(&dir.join(PENDING_FILE), &v)
}

// ---- skip list ------------------------------------------------------------

/// `skipped.json`: `[{version, sha256}]`. An entry with no sha256
/// skips every artifact of that version.
pub fn is_skipped(dir: &Path, version: &str, sha256: Option<&str>) -> bool {
    let Some(list) = read_json_file::<Vec<Value>>(&dir.join(SKIPPED_FILE)) else {
        return false;
    };
    list.iter().any(|e| {
        e.get("version").and_then(Value::as_str) == Some(version)
            && match e.get("sha256").and_then(Value::as_str) {
                Some(want) => Some(want) == sha256,
                None => true,
            }
    })
}

// ---- markers (host writes; watchdog reads) ---------------------------------

/// One boot marker per launch of the pending version — the watchdog's
/// crash-loop evidence (§2 `max_boots`). No-op when no pending update
/// targets the running version.
pub fn write_boot_marker(dir: &Path, p: &PendingFile, pid: u32) -> io::Result<PathBuf> {
    // Nanos in the name: two boots inside one millisecond are still
    // distinct files — the watchdog counts entries, not names.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(BOOTS_DIR).join(format!("{nanos}-{pid}.json"));
    write_json_file(
        &path,
        &json!({
            "version": p.to,
            "session_id": p.session_id,
            "pid": pid,
            "at_ms": now_ms(),
        }),
    )?;
    Ok(path)
}

/// `ready-<version>.json` — the real readiness signal (§2): written by
/// the host when `health().app` reaches ready while `to` is the
/// running version. The watchdog accepts it only when version AND the
/// per-pending `session_id` match.
pub fn write_ready_marker(dir: &Path, p: &PendingFile) -> io::Result<PathBuf> {
    let path = dir.join(format!("ready-{}.json", p.to));
    write_json_file(
        &path,
        &json!({
            "version": p.to,
            "artifact_sha256": p.to_sha256,
            "session_id": p.session_id,
            "at_ms": now_ms(),
        }),
    )?;
    Ok(path)
}

/// `progress.json` — the watchdog's heartbeat evidence (§2): written
/// by the update gate's timer while a pending window is open and
/// `app.component` reports keep landing, so a long honest load
/// (progress ticking, no phase change) is "alive" while true silence
/// rolls back. The file's mtime is the evidence; the body records how
/// fresh the newest report was at write time.
pub fn write_progress(dir: &Path, p: &PendingFile, newest_ms_ago: u64) -> io::Result<PathBuf> {
    let path = dir.join(PROGRESS_FILE);
    write_json_file(
        &path,
        &json!({
            "version": p.to,
            "session_id": p.session_id,
            "newest_report_ms_ago": newest_ms_ago,
            "at_ms": now_ms(),
        }),
    )?;
    Ok(path)
}

/// `failed-<version>.json` — readiness evidence of death: an app
/// component reported `failed` during the pending window.
pub fn write_failed_marker(dir: &Path, p: &PendingFile, component: &str) -> io::Result<PathBuf> {
    let path = dir.join(format!("failed-{}.json", p.to));
    write_json_file(
        &path,
        &json!({
            "version": p.to,
            "session_id": p.session_id,
            "component": component,
            "at_ms": now_ms(),
        }),
    )?;
    Ok(path)
}

// ---- health surface ---------------------------------------------------------

/// `health().update.state` / the `updating` readiness component's
/// vocabulary (decision 0011 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    /// No update activity recorded.
    None,
    /// Installed, awaiting first-boot confirmation.
    Pending,
    /// Download + install in flight in this process.
    Installing,
    /// Watchdog wrote `update-ok` — confirmed.
    Ready,
    /// Watchdog restored the previous version.
    RolledBack,
    /// Per-machine install: rollback needs an approved restore.
    RollbackPendingApproval,
}

impl UpdatePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Installing => "installing",
            Self::Ready => "ready",
            Self::RolledBack => "rolled-back",
            Self::RollbackPendingApproval => "rollback-pending-approval",
        }
    }
}

/// Derive the current phase from the files on disk. `installing` is
/// an in-memory flag — on Windows the install call never returns, so
/// a true pending.json is only ever read by the NEXT process.
pub fn phase(dir: &Path, installing: bool) -> UpdatePhase {
    if dir.join(APPROVAL_FILE).exists() {
        return UpdatePhase::RollbackPendingApproval;
    }
    let pending = dir.join(PENDING_FILE).exists();
    if pending {
        if installing {
            return UpdatePhase::Installing;
        }
        if !dir.join(UPDATE_OK_FILE).exists() {
            return UpdatePhase::Pending;
        }
    }
    if dir.join(UPDATE_OK_FILE).exists() {
        return UpdatePhase::Ready;
    }
    if dir.join(ROLLBACK_REASON_FILE).exists() {
        return UpdatePhase::RolledBack;
    }
    UpdatePhase::None
}

/// `health().update`: `{state, lastRollback?}`. `lastRollback` reads
/// the watchdog's `rollback-reason.txt` / `rollback-pending-approval`
/// payloads (both JSON `{from, to, reason, at}` per this contract).
pub fn health_update(dir: &Path, installing: bool) -> Value {
    let phase = phase(dir, installing);
    let mut out = json!({"state": phase.as_str()});
    for file in [APPROVAL_FILE, ROLLBACK_REASON_FILE] {
        if let Some(v) = read_json_file::<Value>(&dir.join(file)) {
            out["lastRollback"] = v;
            break;
        }
    }
    out
}
