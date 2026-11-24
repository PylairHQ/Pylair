// U0 spike watchdog: readiness-gated rollback.
//
// Spawned detached by the app just before it installs an update. Reads
// $state/pending.json, then waits for the new version's `ready-<to>`
// marker within the deadline. On timeout or a boot loop (>= MAX_BOOTS
// boot markers with no ready) it re-verifies the cached old artifact
// (sha256 + recorded sig), restores it per OS, appends the failed
// version to skipped.json, relaunches, and writes rollback-reason.txt.
//
// State layout ($PYLAIR_STATE_DIR):
//   pending.json   written by the app before install
//   boot-<ver>-<pid>, ready-<ver>   written by the app
//   skipped.json   [{version, sha256}] read by the app, appended here
//   rollback-reason.txt / update-ok-<ver>   written here for the harness

use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
struct Pending {
    from: String,
    to: String,
    to_sha256: String,
    deadline_ms: u64,
    artifact: String,
    artifact_sha256: String,
    sig: String,
    install_path: String,
}

fn ts() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn log(sd: &Path, msg: &str) {
    let line = format!("[wd {}] {}\n", ts(), msg);
    eprint!("{}", line);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sd.join("log.txt"))
        .map(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

fn sha256_file(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let data = fs::read(p).unwrap_or_default();
    let d = Sha256::digest(&data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn boot_count(sd: &Path, ver: &str) -> usize {
    fs::read_dir(sd)
        .map(|it| {
            it.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("boot-{ver}-"))
                })
                .count()
        })
        .unwrap_or(0)
}

// Restore the cached artifact and relaunch. Per-OS install semantics:
//   linux   the artifact IS the runnable file -> copy over install_path
//   macos   the artifact is <name>.app.tar.gz -> rm + extract over parent
//   windows the artifact is the NSIS -setup.exe -> run it /S, then relaunch
fn restore(sd: &Path, p: &Pending) -> Result<(), String> {
    let inst = PathBuf::from(&p.install_path);
    let art = PathBuf::from(&p.artifact);
    #[cfg(target_os = "linux")]
    {
        fs::copy(&art, &inst).map_err(|e| format!("copy {} -> {}: {e}", art.display(), inst.display()))?;
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(&inst).map_err(|e| e.to_string())?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&inst, perm).map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        // install_path = <bundle>/Contents/MacOS/<bin>; bundle = 3 levels up
        let bundle = inst
            .ancestors()
            .nth(3)
            .ok_or_else(|| format!("no bundle for {}", inst.display()))?
            .to_path_buf();
        let parent = bundle
            .parent()
            .ok_or_else(|| "no parent".to_string())?
            .to_path_buf();
        fs::remove_dir_all(&bundle).map_err(|e| format!("rm {}: {e}", bundle.display()))?;
        let st = Command::new("tar")
            .args(["xzf"])
            .arg(&art)
            .arg("-C")
            .arg(&parent)
            .status()
            .map_err(|e| e.to_string())?;
        if !st.success() {
            return Err(format!("tar extract failed: {st}"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        // NSIS silent reinstall; the installer rewrites install_path.
        let st = Command::new(&art)
            .arg("/S")
            .status()
            .map_err(|e| format!("installer spawn: {e}"))?;
        if !st.success() {
            return Err(format!("installer exited {st}"));
        }
        // Wait for the exe to reappear (NSIS forks; give it a minute).
        let w = Instant::now();
        while !inst.exists() && w.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(500));
        }
        if !inst.exists() {
            return Err(format!("{} never reappeared", inst.display()));
        }
    }
    Command::new(&inst)
        .arg("--state-dir")
        .arg(sd)
        .envs(env::vars())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("relaunch {}: {e}", inst.display()))?;
    log(sd, &format!("relaunched {}", inst.display()));
    Ok(())
}

fn add_skip(sd: &Path, p: &Pending) {
    let path = sd.join("skipped.json");
    let mut list: Vec<serde_json::Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    list.push(serde_json::json!({"version": p.to, "sha256": p.to_sha256}));
    let _ = fs::write(&path, serde_json::to_string_pretty(&list).unwrap());
    log(sd, &format!("skipped.json += {} ({})", p.to, p.to_sha256));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let sd = args
        .windows(2)
        .find(|w| w[0] == "--state")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from(env::var("PYLAIR_STATE_DIR").unwrap_or_default()));
    let p: Pending = serde_json::from_str(
        &fs::read_to_string(sd.join("pending.json")).expect("pending.json"),
    )
    .expect("parse pending.json");
    log(&sd, &format!("watchdog start {} -> {}", p.from, p.to));

    let max_boots: usize = env::var("PYLAIR_MAX_BOOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let deadline = Instant::now() + Duration::from_millis(p.deadline_ms);
    let mut reason: Option<String> = None;
    while Instant::now() < deadline {
        if sd.join(format!("ready-{}", p.to)).exists() {
            let _ = fs::write(sd.join(format!("update-ok-{}", p.to)), "");
            let _ = fs::remove_file(sd.join("pending.json"));
            log(&sd, &format!("update ok: v{} reported ready", p.to));
            return;
        }
        let boots = boot_count(&sd, &p.to);
        if boots >= max_boots {
            reason = Some(format!("crash_loop:{boots} boots of v{} without ready", p.to));
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let reason = reason.unwrap_or_else(|| {
        format!("deadline:{}ms without ready-{}", p.deadline_ms, p.to)
    });
    log(&sd, &format!("rollback triggered: {reason}"));

    // Re-verify the cached artifact before trusting it (spike: sha256 +
    // recorded minisign blob; production would run Update::verify /
    // minisign against the embedded pubkey).
    let art = PathBuf::from(&p.artifact);
    let got = sha256_file(&art);
    if got != p.artifact_sha256 || p.sig.is_empty() {
        log(&sd, "cached artifact FAILED verification; not restoring");
        let _ = fs::write(sd.join("rollback-reason.txt"), format!("{reason}\nverify_failed\n"));
        return;
    }
    log(&sd, "cached artifact verified (sha256 + sig present)");

    // Skip-list entry first: the relaunched app re-checks immediately and
    // must not be re-offered the build that just failed readiness.
    add_skip(&sd, &p);
    if let Err(e) = restore(&sd, &p) {
        log(&sd, &format!("restore failed: {e}"));
        let _ = fs::write(sd.join("rollback-reason.txt"), format!("{reason}\nrestore_failed: {e}\n"));
        return;
    }
    let _ = fs::write(sd.join("rollback-reason.txt"), format!("{reason}\nrestored {}\n", p.from));
    let _ = fs::remove_file(sd.join("pending.json"));
    log(&sd, &format!("rolled back to {}", p.from));
}
