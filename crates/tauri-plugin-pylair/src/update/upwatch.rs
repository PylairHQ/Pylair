//! The update watchdog (decision 0011 §2) — the app binary re-entered
//! in a hidden mode.
//!
//! `python_update_install` copies the running exe into the state dir
//! and spawns the copy detached as
//! `app --pylair-upwatch --state-dir <dir> --session-id <id>
//! --install-path <path>` — argv only, never env (the §1 Windows
//! finding). The template's `run()` calls [`entry`] before the Tauri
//! builder, so watchdog mode never touches GUI machinery.
//!
//! Why the app binary doubles as the watchdog: the §2 trust anchor is a
//! pubkey *compiled into the watchdog binary* — the app's own embedded
//! `plugins.updater.pubkey` is exactly that, and the binary is already
//! signed and shipped inside the bundle. The running copy lives outside
//! the install dir so an in-place reinstall can't lock or replace it.
//!
//! ## What it decides
//!
//! The watchdog waits for `ready-<to>` carrying the pending window's
//! `session_id`. It rolls back on a `failed` marker, `max_boots` boot
//! markers without ready, `heartbeat_ms` of silence (no ready, failed,
//! boot or progress file), or `hard_cap_ms` total. On ready it writes
//! `update-ok` and clears pending; on failure it re-verifies the cached
//! artifact (sha256 + minisign vs the compiled-in pubkey), refuses when
//! `pending.install_path` disagrees with its argv-resolved location,
//! writes the skip-list entry BEFORE restoring, restores per OS,
//! relaunches, and records the reason for `health().update.lastRollback`.

use super::{
    is_skipped, read_pending, PendingFile, APPROVAL_FILE, PENDING_FILE, PROGRESS_FILE,
    ROLLBACK_REASON_FILE, SKIPPED_FILE, UPDATE_OK_FILE,
};
use serde_json::{json, Value};
use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};

/// The argv flag that re-enters this binary in watchdog mode.
pub const WATCH_FLAG: &str = "--pylair-upwatch";
const STATE_DIR_ARG: &str = "--state-dir";
const SESSION_ARG: &str = "--session-id";
const INSTALL_PATH_ARG: &str = "--install-path";

const LOCK_FILE: &str = "upwatch.lock";
const LOG_FILE: &str = "upwatch.log";
/// Upwatch log cap: bounded logging into the state dir (§1).
const LOG_CAP_BYTES: u64 = 256 * 1024;
const LOG_KEEP_BYTES: u64 = 128 * 1024;
/// Poll cadence of the decision loop.
const POLL_MS: u64 = 400;

/// The flag argv the host appends when spawning the watchdog copy.
pub fn spawn_args(state_dir: &Path, session_id: &str, install_path: &Path) -> Vec<String> {
    vec![
        WATCH_FLAG.to_string(),
        STATE_DIR_ARG.to_string(),
        state_dir.to_string_lossy().to_string(),
        SESSION_ARG.to_string(),
        session_id.to_string(),
        INSTALL_PATH_ARG.to_string(),
        install_path.to_string_lossy().to_string(),
    ]
}

/// `app --pylair-upwatch …` → the watchdog's contract. `None` for a
/// normal app boot — the flag is the only trigger.
pub struct WatchArgs {
    pub state_dir: PathBuf,
    pub session_id: String,
    /// The install location the *host* resolved and passed on argv —
    /// the trusted copy `pending.install_path` must agree with (§2).
    pub install_path: PathBuf,
}

pub fn parse_args(argv: &[String]) -> Option<WatchArgs> {
    if !argv.iter().any(|a| a == WATCH_FLAG) {
        return None;
    }
    let pick = |flag: &str| -> Option<String> {
        argv.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
    };
    Some(WatchArgs {
        state_dir: PathBuf::from(pick(STATE_DIR_ARG)?),
        session_id: pick(SESSION_ARG)?,
        install_path: PathBuf::from(pick(INSTALL_PATH_ARG)?),
    })
}

// ---- the trusted channel ---------------------------------------------------

/// Which OS behaviour the watchdog acts like — an injectable seam so
/// tests exercise the Windows arms on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Posix,
    Windows,
}

/// Where the artifact lands back on rollback.
pub struct RestorePlan<'a> {
    /// The verified cached artifact (previous version's bundle).
    pub artifact: &'a Path,
    /// Trusted install location (argv-resolved, pending-agreed).
    pub install_path: &'a Path,
    /// The update state dir (for logging inside restore impls).
    pub state_dir: &'a Path,
    /// How long the Windows restore waits for the exe to reappear
    /// after NSIS /S exits (the installer forks).
    pub install_wait: std::time::Duration,
}

type RestoreFn = Box<dyn Fn(&RestorePlan) -> Result<(), String>>;
type RelaunchFn = Box<dyn Fn(&Path) -> Result<(), String>>;

/// Injectable OS/clock surface — the decision logic is pure Rust over
/// these hooks; production uses [`Hooks::default`], tests substitute.
#[doc(hidden)]
pub struct Hooks {
    pub now_ms: Box<dyn Fn() -> u64>,
    pub sleep: Box<dyn Fn(u64)>,
    pub pid_alive: Box<dyn Fn(u32) -> bool>,
    /// Per-OS restore; production impls copy/reinstall in place.
    pub restore: RestoreFn,
    /// Relaunch the restored install.
    pub relaunch: RelaunchFn,
    /// The NSIS exe-reappear wait (Windows restore).
    pub install_wait: std::time::Duration,
    pub platform: Platform,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            now_ms: Box::new(now_ms),
            sleep: Box::new(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
            pid_alive: Box::new(pid_alive),
            restore: Box::new(default_restore),
            relaunch: Box::new(default_relaunch),
            install_wait: std::time::Duration::from_secs(60),
            platform: platform(),
        }
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0): -1/ESRCH = dead, 0 or EPERM = alive.
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        alive
    }
}

/// Bounded log into the state dir (§1): appends one line, keeping the
/// file under [`LOG_CAP_BYTES`] by dropping the head.
fn log(dir: &Path, hooks: &Hooks, msg: &str) {
    let line = format!("[wd {}] {msg}\n", (hooks.now_ms)());
    eprint!("{line}");
    let path = dir.join(LOG_FILE);
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > LOG_CAP_BYTES {
            if let Ok(data) = fs::read(&path) {
                let keep = &data[data.len().saturating_sub(LOG_KEEP_BYTES as usize)..];
                let _ = fs::write(&path, keep);
            }
        }
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

// ---- markers the watchdog reads ---------------------------------------------

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn file_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Session-matched markers under `boots/` — returns `(count,
/// newest_mtime_ms)`. Only markers carrying this pending window's
/// session id count (a stale boot from an older generation is not
/// evidence for this window either way).
fn session_boots(dir: &Path, session_id: &str) -> (usize, u64) {
    let mut count = 0;
    let mut newest = 0;
    if let Ok(entries) = fs::read_dir(dir.join(super::BOOTS_DIR)) {
        for e in entries.flatten() {
            let Some(m) = read_json(&e.path()) else {
                continue;
            };
            if m.get("session_id").and_then(Value::as_str) == Some(session_id) {
                count += 1;
                newest = newest.max(file_ms(&e.path()));
            }
        }
    }
    (count, newest)
}

/// `ready-<to>.json` accepted only when version AND session match (§2
/// "the real readiness signal") — and, when the pending recorded the
/// artifact hash, the marker's `artifact_sha256` must equal it.
fn ready_marker(dir: &Path, p: &PendingFile) -> Option<Value> {
    let m = read_json(&dir.join(format!("ready-{}.json", p.to)))?;
    if m.get("session_id").and_then(Value::as_str) != Some(p.session_id.as_str()) {
        return None;
    }
    if let Some(want) = &p.to_sha256 {
        if m.get("artifact_sha256").and_then(Value::as_str) != Some(want.as_str()) {
            return None;
        }
    }
    Some(m)
}

/// `failed-<to>.json` — evidence of death, session-matched.
fn failed_marker(dir: &Path, p: &PendingFile) -> Option<Value> {
    let m = read_json(&dir.join(format!("failed-{}.json", p.to)))?;
    (m.get("session_id").and_then(Value::as_str) == Some(p.session_id.as_str())).then_some(m)
}

// ---- single-instance lock ----------------------------------------------------

/// One watchdog per pending window: `upwatch.lock` holds the holder's
/// pid. A live holder wins; a dead one is replaced. A lock held by our
/// own pid counts as ours (tests re-enter `run` in-process).
fn acquire_lock(dir: &Path, hooks: &Hooks) -> bool {
    let path = dir.join(LOCK_FILE);
    let me = std::process::id();
    if let Some(l) = read_json(&path) {
        let pid = l.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
        if pid != me && (hooks.pid_alive)(pid) {
            return false;
        }
    }
    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "pid": me,
            "at_ms": (hooks.now_ms)(),
        }))
        .unwrap_or_default(),
    );
    true
}

// ---- verification -------------------------------------------------------------

/// Re-verify the cached artifact before trusting it (§2): sha256
/// against the recorded digest AND minisign against the pubkey
/// compiled into this binary — never anything from the state dir.
fn verify_artifact(dir: &Path, p: &PendingFile, pubkey_b64: &str) -> Result<(), String> {
    let bytes =
        fs::read(&p.artifact).map_err(|e| format!("read cached artifact {}: {e}", p.artifact))?;
    let got = format!("{:x}", sha2::Sha256::digest(&bytes));
    if got != p.sha256 {
        return Err(format!(
            "cached artifact sha256 mismatch: {got} != recorded {}",
            p.sha256
        ));
    }
    let sig_text = {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(p.sig.trim())
            .map_err(|e| format!("cached sig is not base64: {e}"))?;
        String::from_utf8(raw).map_err(|e| format!("cached sig is not utf-8: {e}"))?
    };
    let sig = minisign_verify::Signature::decode(&sig_text)
        .map_err(|e| format!("cached sig decode: {e}"))?;
    let pubkey = minisign_verify::PublicKey::from_base64(pubkey_b64.trim())
        .map_err(|e| format!("updater pubkey decode: {e}"))?;
    pubkey
        .verify(&bytes, &sig, true)
        .map_err(|e| format!("cached artifact failed minisign verify: {e}"))?;
    let _ = dir;
    Ok(())
}

// ---- rollback -----------------------------------------------------------------

/// `{version, sha256}` on the skip list BEFORE restore+relaunch (§2:
/// the restored app re-checks immediately — without the entry on disk
/// it would be re-offered the build that just failed).
fn write_skip_entry(dir: &Path, p: &PendingFile) {
    if is_skipped(dir, &p.to, p.to_sha256.as_deref()) {
        return;
    }
    let path = dir.join(SKIPPED_FILE);
    let mut list: Vec<Value> = read_json(&path)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    list.push(json!({"version": p.to, "sha256": p.to_sha256}));
    if let Some(d) = path.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&list).unwrap_or_default(),
    );
}

fn write_reason(dir: &Path, p: &PendingFile, reason: &str, outcome: &str) {
    let _ = fs::write(
        dir.join(ROLLBACK_REASON_FILE),
        serde_json::to_string_pretty(&json!({
            "from": p.from,
            "to": p.to,
            "reason": reason,
            "outcome": outcome,
            "at": now_ms(),
        }))
        .unwrap_or_default(),
    );
}

fn write_approval(dir: &Path, p: &PendingFile, reason: &str) {
    let _ = fs::write(
        dir.join(APPROVAL_FILE),
        serde_json::to_string_pretty(&json!({
            "from": p.from,
            "to": p.to,
            "reason": reason,
            "at": now_ms(),
        }))
        .unwrap_or_default(),
    );
}

// ---- per-OS restore + relaunch -------------------------------------------------

/// The exe the relaunch spawns: the install path itself on Windows /
/// AppImage; the bundle's inner binary on macOS (`open` handles the
/// bundle dir too, but a direct exe spawn needs no PATH lookup).
#[cfg(target_os = "macos")]
fn relaunch_target(install_path: &Path) -> PathBuf {
    let macos_dir = install_path.join("Contents").join("MacOS");
    if let Ok(mut it) = fs::read_dir(&macos_dir) {
        if let Some(Ok(e)) = it.next() {
            return e.path();
        }
    }
    install_path.to_path_buf()
}

fn default_restore(plan: &RestorePlan) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        // The artifact IS the runnable file: overwrite the recorded
        // APPIMAGE path and restore the exec bit.
        fs::copy(plan.artifact, plan.install_path).map_err(|e| {
            format!(
                "copy {} -> {}: {e}",
                plan.artifact.display(),
                plan.install_path.display()
            )
        })?;
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(plan.install_path)
            .map_err(|e| e.to_string())?
            .permissions();
        perm.set_mode(0o755);
        fs::set_permissions(plan.install_path, perm).map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        // install_path is the .app bundle: swap it wholesale for the
        // cached .app.tar.gz expanded beside it.
        let parent = plan
            .install_path
            .parent()
            .ok_or_else(|| "install path has no parent".to_string())?;
        if plan.install_path.exists() {
            fs::remove_dir_all(plan.install_path)
                .map_err(|e| format!("remove {}: {e}", plan.install_path.display()))?;
        }
        let st = std::process::Command::new("tar")
            .arg("xzf")
            .arg(plan.artifact)
            .arg("-C")
            .arg(parent)
            .status()
            .map_err(|e| format!("tar spawn: {e}"))?;
        if !st.success() {
            return Err(format!("tar extract failed: {st}"));
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        // NSIS silent reinstall, then wait for the exe to reappear —
        // the installer forks, so process exit precedes the file
        // landing.
        let st = std::process::Command::new(plan.artifact)
            .arg("/S")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("installer spawn: {e}"))?;
        if !st.success() {
            return Err(format!("installer exited {st}"));
        }
        let deadline = std::time::Instant::now() + plan.install_wait;
        while !plan.install_path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if !plan.install_path.exists() {
            return Err(format!("{} never reappeared", plan.install_path.display()));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err("no restore path for this OS".to_string())
    }
}

fn default_relaunch(target: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let exe = relaunch_target(target);
    #[cfg(not(target_os = "macos"))]
    let exe = target;
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("relaunch {}: {e}", exe.display()))
}

// ---- the verdict --------------------------------------------------------------

/// How a pending window ended — the file contract records the same
/// outcome for `health().update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// `ready-<to>` matched: `update-ok` written, pending cleared.
    Confirmed,
    /// The previous version was verified, restored and relaunched.
    RolledBack { reason: String },
    /// Per-machine Windows install: rollback can't elevate — the next
    /// interactive launch prompts for an approved restore (§2).
    ApprovalPending { reason: String },
    /// Could not safely restore (verify/mismatch/restore failure) —
    /// the reason file records why; the install is left in place.
    Aborted { reason: String, detail: String },
}

/// Run the pending window to a verdict. Shared by [`entry`] (real
/// argv) and the integration tests (injected hooks).
#[doc(hidden)]
pub fn run(args: &WatchArgs, pubkey_b64: &str, hooks: &Hooks) -> Verdict {
    let dir = &args.state_dir;
    let Some(p) = read_pending(dir) else {
        log(dir, hooks, "no pending.json — nothing to watch");
        return Verdict::Confirmed;
    };
    if p.session_id != args.session_id {
        log(
            dir,
            hooks,
            &format!(
                "pending session {} != argv session {} — a newer window owns this state",
                p.session_id, args.session_id
            ),
        );
        return Verdict::Confirmed;
    }
    if !acquire_lock(dir, hooks) {
        log(
            dir,
            hooks,
            "another live watchdog holds upwatch.lock — exiting",
        );
        return Verdict::Confirmed;
    }
    log(
        dir,
        hooks,
        &format!(
            "watching v{} -> v{} (session {})",
            p.from, p.to, p.session_id
        ),
    );

    let window_start = p.at_ms.unwrap_or_else(|| (hooks.now_ms)());
    let reason = loop {
        (hooks.sleep)(POLL_MS);
        let now = (hooks.now_ms)();

        // §2 arm order: a proven app confirms; a dead one rolls back.
        if ready_marker(dir, &p).is_some() {
            let _ = fs::write(
                dir.join(UPDATE_OK_FILE),
                serde_json::to_string_pretty(&json!({
                    "version": p.to,
                    "session_id": p.session_id,
                    "at_ms": now,
                }))
                .unwrap_or_default(),
            );
            let _ = fs::remove_file(dir.join(PENDING_FILE));
            log(dir, hooks, &format!("v{} reported ready — confirmed", p.to));
            return Verdict::Confirmed;
        }
        if let Some(m) = failed_marker(dir, &p) {
            let component = m
                .get("component")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            break format!("readiness_failed:{component}");
        }
        let (boots, newest_boot) = session_boots(dir, &p.session_id);
        if boots as u64 >= p.knobs.max_boots {
            break format!("crash_loop:{boots} boots of v{} without ready", p.to);
        }
        // Silence: no ready, no failed, no progress and no new boot
        // for heartbeat_ms. Live progress keeps the window open until
        // the hard cap — the only wall clock (§2).
        let progress_ms = file_ms(&dir.join(PROGRESS_FILE));
        let last_life = window_start.max(progress_ms).max(newest_boot);
        if now.saturating_sub(last_life) > p.knobs.heartbeat_ms {
            break "no_sign_of_life".to_string();
        }
        if now.saturating_sub(window_start) > p.knobs.hard_cap_ms {
            break "hard_cap".to_string();
        }
    };
    log(dir, hooks, &format!("rollback triggered: {reason}"));

    // The install_path pending.json *claims* must equal the trusted
    // argv value the host resolved — a state-dir write must never
    // redirect the restore (§2).
    if Path::new(&p.install_path) != args.install_path {
        let detail = format!(
            "pending install_path {} != resolved {}",
            p.install_path,
            args.install_path.display()
        );
        write_reason(dir, &p, &reason, "aborted:install_path_mismatch");
        let _ = fs::remove_file(dir.join(PENDING_FILE));
        log(dir, hooks, &format!("aborting rollback: {detail}"));
        return Verdict::Aborted {
            reason,
            detail: "install_path_mismatch".to_string(),
        };
    }
    if let Err(e) = verify_artifact(dir, &p, pubkey_b64) {
        write_reason(dir, &p, &reason, &format!("verify_failed:{e}"));
        let _ = fs::remove_file(dir.join(PENDING_FILE));
        log(
            dir,
            hooks,
            &format!("cached artifact verification failed: {e}"),
        );
        return Verdict::Aborted {
            reason,
            detail: "verify_failed".to_string(),
        };
    }
    log(dir, hooks, "cached artifact verified (sha256 + minisign)");

    // §2: per-machine installs can't silently elevate for the
    // rollback — record the approval state; the next interactive
    // launch offers the restore.
    let mode = p.install_mode.as_deref().unwrap_or("current-user");
    if hooks.platform == Platform::Windows && mode != "current-user" {
        write_skip_entry(dir, &p);
        write_approval(dir, &p, &reason);
        write_reason(dir, &p, &reason, "rollback-pending-approval");
        let _ = fs::remove_file(dir.join(PENDING_FILE));
        log(
            dir,
            hooks,
            "per-machine install: rollback recorded for approval",
        );
        return Verdict::ApprovalPending { reason };
    }

    // Skip-list BEFORE restore+relaunch: the restored app re-checks
    // on boot and must not be re-offered this artifact (§2).
    write_skip_entry(dir, &p);
    let plan = RestorePlan {
        artifact: Path::new(&p.artifact),
        install_path: &args.install_path,
        state_dir: dir,
        install_wait: hooks.install_wait,
    };
    if let Err(e) = (hooks.restore)(&plan) {
        write_reason(dir, &p, &reason, &format!("restore_failed:{e}"));
        let _ = fs::remove_file(dir.join(PENDING_FILE));
        log(dir, hooks, &format!("restore failed: {e}"));
        return Verdict::Aborted {
            reason,
            detail: "restore_failed".to_string(),
        };
    }
    if let Err(e) = (hooks.relaunch)(&args.install_path) {
        // The restore landed; a relaunch failure leaves the app
        // installed but not started — still a completed rollback.
        log(
            dir,
            hooks,
            &format!("relaunch failed (restore landed): {e}"),
        );
    }
    write_reason(dir, &p, &reason, &format!("restored {}", p.from));
    let _ = fs::remove_file(dir.join(PENDING_FILE));
    log(dir, hooks, &format!("rolled back to {}", p.from));
    Verdict::RolledBack { reason }
}

// ---- argv entry ------------------------------------------------------------------

/// The app's hidden watchdog mode. Called at the top of the template's
/// `run()` with the embedded context — returns `None` for normal
/// boots, `Some(exit_code)` after a completed watchdog session.
///
/// `ctx` supplies the compiled-in `plugins.updater.pubkey` — the §2
/// trust anchor that must never come from the state dir.
#[cfg(feature = "updater")]
pub fn entry<R: tauri::Runtime>(ctx: &tauri::Context<R>) -> Option<i32> {
    let argv: Vec<String> = std::env::args().collect();
    let args = parse_args(&argv)?;
    let Some(pubkey) = ctx
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(Value::as_str)
    else {
        eprintln!("[pylair] update watchdog: no plugins.updater.pubkey in embedded config");
        return Some(2);
    };
    let verdict = run(&args, pubkey, &Hooks::default());
    let code = match verdict {
        Verdict::Aborted { .. } => 1,
        _ => 0,
    };
    Some(code)
}
