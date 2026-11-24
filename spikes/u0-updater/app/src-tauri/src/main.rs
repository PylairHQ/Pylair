// U0 spike app: a scaffold-shaped Tauri app (backend/ + python/ resources
// mirroring the create-pylair bundle layout) wired to tauri-plugin-updater.
//
// Self-test flow, enabled by PYLAIR_UPDATE_TEST=1 and driven entirely from
// Rust so the webview stays a static page:
//   boot marker -> resource dump -> (BROKEN => exit 3) -> ready marker
//   check() -> skip-list gate -> cache current artifact -> pending.json
//   -> spawn watchdog detached -> download_and_install -> restart
// All observability goes to $PYLAIR_STATE_DIR files the CI harness reads.

use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn state_dir() -> PathBuf {
    // --state-dir argv beats env: on Windows the NSIS installer relaunches
    // the app under the explorer environment (PYLAIR_* env is lost), but
    // the updater forwards the app's own argv via /ARGS.
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|w| w[0] == "--state-dir")
        .map(|w| PathBuf::from(&w[1]))
        .or_else(|| std::env::var("PYLAIR_STATE_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join("u0-updater-state"))
}

fn ts() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn log(sd: &Path, msg: &str) {
    let line = format!("[app {}] {}\n", ts(), msg);
    eprint!("{}", line);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sd.join("log.txt"))
        .map(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

fn resource_marker(handle: &tauri::AppHandle, rel: &str) -> String {
    use tauri::Manager;
    let dir = match handle.path().resource_dir() {
        Ok(d) => d,
        Err(e) => return format!("resource_dir error: {e}"),
    };
    let p = dir.join(rel);
    match fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) => format!("{}: {e}", p.display()),
    }
}

#[derive(Deserialize)]
struct SkipEntry {
    version: String,
    sha256: String,
}

fn load_skips(sd: &Path) -> Vec<SkipEntry> {
    fs::read_to_string(sd.join("skipped.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// Fetch latest.json ourselves to read the artifact sha256 we add alongside
// the stock manifest (skip list is keyed on version + artifact hash).
fn fetch_remote_identity(endpoint: &str) -> Option<(String, String)> {
    let body: serde_json::Value = ureq::get(endpoint).call().ok()?.into_json().ok()?;
    let plat = body["platforms"].as_object()?;
    let (_k, v) = plat.iter().next()?;
    Some((
        body["version"].as_str()?.to_string(),
        v["sha256"].as_str()?.to_string(),
    ))
}

fn spawn_watchdog(sd: &Path) {
    let bin = std::env::var("PYLAIR_WATCHDOG_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe()
                .unwrap_or_default()
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(if cfg!(windows) { "upwatch.exe" } else { "upwatch" })
        });
    log(sd, &format!("spawning watchdog {}", bin.display()));
    match Command::new(&bin)
        .arg("--state")
        .arg(sd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => log(sd, &format!("watchdog pid {}", c.id())),
        Err(e) => log(sd, &format!("watchdog spawn failed: {e}")),
    }
}

async fn update_flow(handle: tauri::AppHandle, sd: PathBuf, endpoint: String) {
    use tauri_plugin_updater::UpdaterExt;
    log(&sd, "update check start");
    let updater = match handle.updater() {
        Ok(u) => u,
        Err(e) => return log(&sd, &format!("updater init failed: {e}")),
    };
    let remote = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => return log(&sd, "check: up to date"),
        Err(e) => return log(&sd, &format!("check failed: {e}")),
    };
    let (rver, rsha) = fetch_remote_identity(&endpoint).unwrap_or_default();
    log(
        &sd,
        &format!(
            "check: remote {} sha {} -> {}",
            remote.version, rsha, remote.download_url
        ),
    );
    if load_skips(&sd)
        .iter()
        .any(|s| s.version == rver && s.sha256 == rsha)
    {
        return log(&sd, &format!("skip: {rver} on skip list"));
    }

    // Cache the CURRENT version's artifact before installing: fetch its
    // manifest (rollback-<from>.json) from the same channel and download
    // the artifact the way production would keep it from install time.
    let cache = sd.join("cache");
    let _ = fs::create_dir_all(&cache);
    let from = env!("CARGO_PKG_VERSION").to_string();
    let base = endpoint.trim_end_matches('/').trim_end_matches("latest.json");
    let rurl = format!("{}/rollback-{}.json", base, from);
    let rb = ureq::get(&rurl)
        .call()
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok());
    let mut artifact = String::new();
    let mut art_sha = String::new();
    let mut art_sig = String::new();
    if let Some(m) = rb {
        if let Some((_k, v)) = m["platforms"].as_object().and_then(|p| p.iter().next()) {
            if let (Some(url), Some(sha), Some(sig)) = (
                v["url"].as_str(),
                v["sha256"].as_str(),
                v["signature"].as_str(),
            ) {
                let dest = cache.join(url.rsplit('/').next().unwrap_or("artifact.bin"));
                match ureq::get(url).call() {
                    Ok(resp) => {
                        let mut buf = Vec::new();
                        use std::io::Read;
                        if resp.into_reader().read_to_end(&mut buf).is_ok()
                            && fs::write(&dest, &buf).is_ok()
                        {
                            artifact = dest.display().to_string();
                            art_sha = sha.to_string();
                            art_sig = sig.to_string();
                            log(
                                &sd,
                                &format!(
                                    "cached rollback artifact {} ({} bytes)",
                                    dest.display(),
                                    buf.len()
                                ),
                            );
                        }
                    }
                    Err(e) => log(&sd, &format!("rollback artifact fetch failed: {e}")),
                }
            }
        }
    }
    if artifact.is_empty() {
        return log(&sd, "abort: no cached rollback artifact, refusing to update");
    }

    let pending = serde_json::json!({
        "from": from,
        "to": remote.version,
        "to_sha256": rsha,
        "deadline_ms": std::env::var("PYLAIR_ROLLBACK_DEADLINE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000u64),
        "artifact": artifact,
        "artifact_sha256": art_sha,
        "sig": art_sig,
        "install_path": std::env::var("APPIMAGE").ok()
            .or_else(|| std::env::current_exe().ok().map(|p| p.display().to_string()))
            .unwrap_or_default(),
    });
    let _ = fs::write(
        sd.join("pending.json"),
        serde_json::to_string_pretty(&pending).unwrap(),
    );
    log(&sd, &format!("pending {} -> {}", from, remote.version));

    spawn_watchdog(&sd);

    let t0 = std::time::Instant::now();
    match remote.download_and_install(|_got, _total| {}, || {}).await {
        Ok(()) => {
            log(
                &sd,
                &format!("install ok in {}ms, restarting", t0.elapsed().as_millis()),
            );
            handle.restart();
        }
        Err(e) => log(&sd, &format!("install failed: {e}")),
    }
}

fn main() {
    let sd = state_dir();
    let _ = fs::create_dir_all(&sd);
    let ver = env!("CARGO_PKG_VERSION");
    let pid = std::process::id();
    let _ = fs::write(sd.join(format!("boot-{ver}-{pid}")), "");

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            let handle = app.handle().clone();
            let sd = sd.clone();

            let backend = resource_marker(&handle, "backend/app.py");
            let runtime = resource_marker(&handle, "python/pylair-runtime.txt");
            let _ = fs::write(
                sd.join("backend-marker.txt"),
                format!(
                    "--- backend/app.py ---\n{backend}--- python/pylair-runtime.txt ---\n{runtime}"
                ),
            );
            log(&sd, &format!("boot v{ver} pid {pid}"));

            if backend.contains("BROKEN = True") {
                log(&sd, "backend marked BROKEN -> on_startup failure (exit 3)");
                std::process::exit(3);
            }

            let ready_delay: u64 = std::env::var("PYLAIR_READY_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_000);
            let exit_after: Option<u64> = std::env::var("PYLAIR_EXIT_AFTER_READY_MS")
                .ok()
                .and_then(|v| v.parse().ok());
            let sd_ready = sd.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ready_delay));
                let _ = fs::write(sd_ready.join(format!("ready-{ver}")), "");
                log(&sd_ready, &format!("ready v{ver}"));
                if let Some(ms) = exit_after {
                    std::thread::sleep(Duration::from_millis(ms));
                    std::process::exit(0);
                }
            });

            if std::env::var("PYLAIR_UPDATE_TEST").as_deref() == Ok("1") {
                let sd_u = sd.clone();
                let endpoint = std::env::var("PYLAIR_UPDATE_ENDPOINT")
                    .unwrap_or_else(|_| "http://127.0.0.1:8377/latest.json".into());
                tauri::async_runtime::spawn(update_flow(handle, sd_u, endpoint));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run");
}
