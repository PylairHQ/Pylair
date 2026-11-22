use super::commands::*;
use super::registry::*;
use super::spec::*;
use super::*;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

fn env_get(spec: &SpawnSpec, k: &str) -> Option<String> {
    spec.env
        .iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.clone())
}

#[test]
fn appdir_entries_are_scrubbed_from_child_paths() {
    let mut spec = SpawnSpec::default();
    scrub_appdir(
        &mut spec,
        fake_env(&[
            ("APPDIR", "/tmp/.mount_App123"),
            (
                "LD_LIBRARY_PATH",
                "/tmp/.mount_App123/usr/lib:/tmp/.mount_App123/lib:/usr/lib",
            ),
            ("PATH", "/tmp/.mount_App123/usr/bin:/usr/bin:/bin"),
        ]),
    );
    assert_eq!(
        env_get(&spec, "LD_LIBRARY_PATH").as_deref(),
        Some("/usr/lib")
    );
    assert_eq!(env_get(&spec, "PATH").as_deref(), Some("/usr/bin:/bin"));
    assert!(!spec.env_remove.iter().any(|k| k == "PATH"));
}

#[test]
fn appdir_only_entries_remove_the_var() {
    let mut spec = SpawnSpec::default();
    scrub_appdir(
        &mut spec,
        fake_env(&[("APPDIR", "/a"), ("LD_LIBRARY_PATH", "/a/usr/lib:/a/lib")]),
    );
    assert!(spec.env_remove.iter().any(|k| k == "LD_LIBRARY_PATH"));
    assert!(env_get(&spec, "LD_LIBRARY_PATH").is_none());
}

#[test]
fn sibling_prefix_is_not_scrubbed() {
    // /appdir2 is NOT under /appdir — a naive starts_with would eat it.
    let mut spec = SpawnSpec::default();
    scrub_appdir(
        &mut spec,
        fake_env(&[
            ("APPDIR", "/appdir"),
            ("LD_LIBRARY_PATH", "/appdir2/lib:/appdir/lib"),
        ]),
    );
    assert_eq!(
        env_get(&spec, "LD_LIBRARY_PATH").as_deref(),
        Some("/appdir2/lib")
    );
}

#[test]
fn no_appdir_is_a_noop() {
    let mut spec = SpawnSpec::default();
    scrub_appdir(&mut spec, fake_env(&[("LD_LIBRARY_PATH", "/x")]));
    assert!(spec.env.is_empty() && spec.env_remove.is_empty());
}

#[test]
fn origin_match_is_exact() {
    let app = "http://127.0.0.1:5000/";
    assert!(same_origin(app, "http://127.0.0.1:5000/path?q=1"));
    // Bypass attempts: evil subdomains and userinfo tricks.
    assert!(!same_origin(app, "http://127.0.0.1:5000.evil.com/"));
    assert!(!same_origin(app, "http://127.0.0.1@evil.com/"));
    assert!(!same_origin(app, "https://127.0.0.1:5000/")); // scheme differs
    assert!(!same_origin(app, "http://127.0.0.1:5001/")); // port differs
    assert!(!same_origin(app, "http://localhost:5000/")); // host differs
    assert!(!same_origin(app, "not a url"));
    // Opaque origins never match anything.
    assert!(!same_origin("about:blank", "about:blank"));
    // Default-port normalization: explicit :443 == https default.
    assert!(same_origin("https://a.example:443/", "https://a.example/"));
    // Case-insensitive host compare; trailing dot and IPv6 literals are
    // distinct origins (url-crate normalization decides, we don't guess).
    assert!(same_origin("http://EXAMPLE.com/", "http://example.com/"));
    assert!(!same_origin("http://localhost/", "http://localhost./"));
    assert!(same_origin("http://[::1]:8080/", "http://[::1]:8080/x"));
    // localhost and 127.0.0.1 are DIFFERENT origins — pinned so nobody
    // "fixes" the false negative into a false positive later.
    assert!(!same_origin("http://localhost/", "http://127.0.0.1/"));
}

#[test]
fn window_registry_routes_to_owners_and_subscribers() {
    let mut reg = WindowRegistry::default();
    reg.subscribe("main");
    reg.subscribe("panel");
    assert_eq!(reg.broadcast_targets().len(), 2);
    reg.drop_label("panel");
    assert_eq!(reg.broadcast_targets(), vec!["main".to_string()]);
}

/// F4 (rc.15): a failed emit_to evicts the label only when the window
/// is actually gone — a transient failure on a live window must keep
/// the subscription. get_webview_window is the oracle: a destroyed
/// window is removed from the manager and hits the same absent-label
/// path the "ghost" case covers (MockRuntime can't move a live window
/// to destroyed — its destroy() never runs the event loop's removal).
/// Same webview-link constraint as plugin_setup.rs — not on Windows.
#[cfg(not(windows))]
#[test]
fn failed_emit_evicts_only_gone_windows() {
    use tauri::test::{mock_context, noop_assets};
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    let app = tauri::test::mock_builder()
        .build(mock_context(noop_assets()))
        .expect("mock app builds");
    let reg = Registry::default();
    {
        let mut r = reg.lock().unwrap();
        r.subscribe("w1");
        r.subscribe("ghost");
    }
    let w1 = WebviewWindowBuilder::new(&app, "w1", WebviewUrl::default())
        .build()
        .unwrap();

    // A live window survives a transient emit failure.
    handle_emit_failure(app.handle(), &reg, "w1");
    assert!(
        reg.lock()
            .unwrap()
            .broadcast_targets()
            .contains(&"w1".to_string()),
        "live window lost its subscription on a transient emit failure"
    );

    // A label with no window is evicted, exactly as before — this is
    // the path a destroyed window takes (the manager drops it from
    // webview_windows() when WindowEvent::Destroyed is processed).
    handle_emit_failure(app.handle(), &reg, "ghost");
    assert!(
        !reg.lock()
            .unwrap()
            .broadcast_targets()
            .contains(&"ghost".to_string()),
        "emit failure on a missing window must still evict it"
    );
    let _ = w1; // live-window guard above keeps it; nothing else needed
}

#[test]
fn stderr_targets_only_intersects_the_allow_list() {
    let mut reg = WindowRegistry::default();
    reg.subscribe("main");
    reg.subscribe("win-b");
    reg.subscribe("console");
    // Default: main only — win-b and console are subscribed but get nothing.
    assert_eq!(
        reg.stderr_targets(&StderrDelivery::default()),
        vec!["main".to_string()]
    );
    // Explicit list: intersection with subscribers — "console" is
    // allowed and subscribed, "logs" is allowed but never invoked a
    // command, "win-b" is subscribed but not allowed.
    let d = StderrDelivery::Only(vec!["main".into(), "console".into(), "logs".into()]);
    let mut got = reg.stderr_targets(&d);
    got.sort();
    assert_eq!(got, vec!["console".to_string(), "main".to_string()]);
    // All: every subscriber, exactly the old broadcast.
    let mut got = reg.stderr_targets(&StderrDelivery::All);
    got.sort();
    assert_eq!(
        got,
        vec![
            "console".to_string(),
            "main".to_string(),
            "win-b".to_string()
        ]
    );
}

#[test]
fn stream_ack_carries_task_and_bridge_protocol() {
    // A new client reads this object to detect an old plugin (which
    // returned a bare task id) — the exact shape is the contract.
    let v = serde_json::to_value(StreamAck {
        task: 7,
        protocol: BRIDGE_PROTOCOL,
    })
    .unwrap();
    assert_eq!(v, json!({"task": 7, "protocol": 2}));
}

#[test]
fn release_spec_forbids_bytecode_writes() {
    // rc.15 H: the bundle ships precompiled pycs and the install dir
    // is often read-only — the runtime must never write bytecode.
    // Isolated workers and app subprocesses inherit os.environ, so
    // this one var covers the whole tree.
    let spec = release_spawn_spec(Path::new("/res"), "backend.app", "backend");
    assert!(
        spec.env
            .contains(&("PYTHONDONTWRITEBYTECODE".into(), "1".into())),
        "release spawn spec must set PYTHONDONTWRITEBYTECODE=1"
    );
    assert!(
        !spec
            .env_remove
            .iter()
            .any(|k| k == "PYTHONDONTWRITEBYTECODE"),
        "the var must survive into the child env"
    );
}

#[test]
fn bundled_resource_dir_prefers_resource_dir_then_falls_back() {
    // resource_dir() success wins outright.
    let dir = PathBuf::from(if cfg!(windows) { "C:/res" } else { "/res" });
    assert_eq!(bundled_resource_dir(Ok(dir.clone())).unwrap(), dir);
    // resource_dir() failure falls back to exe-relative resources —
    // with no bundle next to the test binary that also fails, and the
    // error must name the cause and the fix, not panic.
    let err = bundled_resource_dir(Err(tauri::Error::UnknownPath)).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("resource_dir()"),
        "error lost the cause: {msg}"
    );
    assert!(
        msg.contains("symlinked path"),
        "error not actionable: {msg}"
    );
}

#[test]
fn resources_near_exe_covers_each_bundle_layout() {
    let tmp = std::env::temp_dir().join(format!("pylair-res-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // macOS .app: <exe>/../Resources/python — expectations are
    // canonicalized too because the resolver returns real paths
    // (on Windows that means a \\?\ prefix).
    let mac = tmp.join("Fixture.app/Contents");
    std::fs::create_dir_all(mac.join("Resources/python")).unwrap();
    std::fs::create_dir_all(mac.join("MacOS")).unwrap();
    let exe = mac.join("MacOS/fixture");
    assert_eq!(
        resources_near_exe(&exe).unwrap(),
        mac.join("Resources").canonicalize().unwrap()
    );
    // deb/AppImage: <exe>/../lib/<stem>/python
    let deb = tmp.join("debroot/usr");
    std::fs::create_dir_all(deb.join("lib/myapp/python")).unwrap();
    std::fs::create_dir_all(deb.join("bin")).unwrap();
    assert_eq!(
        resources_near_exe(&deb.join("bin/myapp")).unwrap(),
        deb.join("lib/myapp").canonicalize().unwrap()
    );
    // NSIS: <exe dir>/python
    let nsis = tmp.join("nsisroot");
    std::fs::create_dir_all(nsis.join("python")).unwrap();
    assert_eq!(
        resources_near_exe(&nsis.join("app.exe")).unwrap(),
        nsis.canonicalize().unwrap()
    );
    // No layout at all → None, never a guessed path.
    let bare = tmp.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert!(resources_near_exe(&bare.join("app")).is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The rc.8 panic: launched through a symlinked path (`/tmp` →
/// `/private/tmp` on macOS, or any symlinked ancestor dir), Tauri
/// rejects the exe path — the plugin must still find the bundle.
/// Spawns the test binary through a symlinked directory and asserts
/// `resources_from_exe` resolves the CANONICAL bundle, proving the
/// fallback works in the exact condition that broke resource_dir().
#[cfg(unix)]
#[test]
fn launched_via_symlinked_dir_still_resolves_resources() {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    const CHILD_ENV: &str = "PYLAIR_LINKED_EXE_CHILD";
    if std::env::var(CHILD_ENV).is_ok() {
        // Child: report what the exe-relative resolver sees here.
        let exe = std::env::current_exe().unwrap();
        let res = resources_from_exe();
        println!(
            "exe={}\nres={}",
            exe.display(),
            res.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "NONE".into())
        );
        return;
    }

    let tmp = std::env::temp_dir().join(format!("pylair-linked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let real = tmp.join("realdir/Fixture.app/Contents");
    std::fs::create_dir_all(real.join("Resources/python")).unwrap();
    std::fs::create_dir_all(real.join("MacOS")).unwrap();
    let real_exe = real.join("MacOS/pylair-test-bin");
    std::fs::copy(std::env::current_exe().unwrap(), &real_exe).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(real_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The launch path's ancestor is a symlink — the macOS /tmp case.
    let linkdir = tmp.join("linkdir");
    symlink(tmp.join("realdir"), &linkdir).unwrap();
    let linked_exe = linkdir.join("Fixture.app/Contents/MacOS/pylair-test-bin");

    // --exact needs the module-qualified name: this is a unit test
    // inside `mod tests`, so its full name carries the `tests::`
    // prefix (unlike the tests/*.rs files' top-level names).
    let out = Command::new(linked_exe)
        .args([
            "tests::launched_via_symlinked_dir_still_resolves_resources",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn test binary through symlinked dir");
    assert!(
        out.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let res_line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("res="))
        .unwrap_or_else(|| panic!("child reported no res= line; stdout: {stdout}"));
    let want = real.join("Resources").canonicalize().unwrap();
    assert_eq!(
        PathBuf::from(res_line).canonicalize().unwrap(),
        want,
        "resources resolved to a non-canonical or wrong path (exe line: {})",
        stdout.lines().next().unwrap_or("?")
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- PYLAIR_APP_DATA_DIR (decision 0009 §2, model manager PR 1) ---------

#[test]
fn app_data_dir_env_pushes_resolved_dir_and_creates_it() {
    let tmp = std::env::temp_dir().join(format!("pylair-add-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut spec = SpawnSpec::default();
    set_app_data_dir_env(&mut spec, Ok(tmp.clone()), |_| None);
    assert_eq!(
        env_get(&spec, "PYLAIR_APP_DATA_DIR").as_deref(),
        Some(tmp.to_str().unwrap()),
        "the resolved dir must land in the child env"
    );
    assert!(tmp.is_dir(), "the dir is created if missing");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn app_data_dir_env_unset_on_resolve_failure() {
    // Best-effort, never fatal: a resolve failure logs and leaves the
    // variable absent — the spawn proceeds either way.
    let mut spec = SpawnSpec::default();
    set_app_data_dir_env(&mut spec, Err(tauri::Error::UnknownPath), |_| None);
    assert!(env_get(&spec, "PYLAIR_APP_DATA_DIR").is_none());
}

#[test]
fn app_data_dir_env_unset_when_dir_unwritable() {
    // create_dir_all failing is the same never-fatal path — simulate it
    // by pointing the data dir at a path under a file.
    let tmp = std::env::temp_dir().join(format!("pylair-adf-{}", std::process::id()));
    std::fs::write(&tmp, "a file, not a dir").unwrap();
    let mut spec = SpawnSpec::default();
    set_app_data_dir_env(&mut spec, Ok(tmp.join("subdir")), |_| None);
    assert!(env_get(&spec, "PYLAIR_APP_DATA_DIR").is_none());
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn app_data_dir_env_debug_override_keeps_inherited() {
    // The dev override precedent (debug builds only, same as the
    // interpreter/module overrides above app_spawn_spec): an inherited
    // PYLAIR_APP_DATA_DIR wins — nothing is pushed and the child simply
    // inherits it. cargo test runs debug, so cfg!(debug_assertions) is
    // live; the env map is injected, not the process environment.
    let mut spec = SpawnSpec::default();
    set_app_data_dir_env(
        &mut spec,
        Ok(PathBuf::from(if cfg!(windows) {
            "C:/resolved"
        } else {
            "/resolved"
        })),
        fake_env(&[("PYLAIR_APP_DATA_DIR", "/override")]),
    );
    assert!(
        env_get(&spec, "PYLAIR_APP_DATA_DIR").is_none(),
        "an inherited override must not be overwritten in debug builds"
    );
}

/// The variable has to reach the CHILD's environment, not just sit in the
/// spec — spawn a python that echoes it (the fake-runtime form of the
/// check testapp.py's env-flag fixtures make against a real runtime).
#[test]
fn app_data_dir_env_reaches_spawned_child() {
    fn python() -> PathBuf {
        if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
            return p.into();
        }
        for cand in ["python3", "python"] {
            let ok = std::process::Command::new(cand)
                .arg("-c")
                .arg("import sys;sys.exit(0)")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return cand.into();
            }
        }
        panic!("no python found for the app-data-dir echo test");
    }
    let tmp = std::env::temp_dir().join(format!("pylair-ade-{}", std::process::id()));
    let mut spec = SpawnSpec::default();
    set_app_data_dir_env(&mut spec, Ok(tmp.clone()), |_| None);
    let mut cmd = std::process::Command::new(python());
    cmd.arg("-c")
        .arg("import os;print(os.environ['PYLAIR_APP_DATA_DIR'])");
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    let out = cmd.output().expect("spawn python for the env echo");
    assert!(
        out.status.success(),
        "echo child failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        tmp.to_string_lossy(),
        "the spawned runtime saw a different PYLAIR_APP_DATA_DIR"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
