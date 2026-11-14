//! Import hygiene for packaged spawns: a launched app's cwd lands first
//! on `sys.path` for `python -m`, so a `json.py` or `pylair/` in the
//! launch directory shadows the stdlib or the runtime itself.
//! `release_spawn_spec` must pin `PYTHONSAFEPATH=1` (Python ≥3.11 drops
//! cwd/`-m` script dir from sys.path) and cwd = the bundled resources
//! dir — plus scrub `PYTHONWARNINGS`/`PYTHONBREAKPOINT` like the other
//! inherited config vars.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::release_spawn_spec;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(20);

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        let ok = std::process::Command::new(cand)
            .arg("-c")
            .arg("import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return cand.into();
        }
    }
    panic!("no python >=3.12 found for supervisor tests");
}

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(10);
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.breaker_crashes = 10;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c
}

fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn tests_dir() -> PathBuf {
    repo_root().join("python/pylair/tests")
}

fn pylair_src() -> PathBuf {
    repo_root().join("python/pylair/src")
}

fn env_of<'a>(spec: &'a SpawnSpec, k: &str) -> Option<&'a str> {
    spec.env
        .iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
}

/// A "resources" dir whose own contents are hostile: `json.py` shadows
/// the stdlib (every `import json` in pylair hits it) and `pylair/`
/// shadows the runtime package itself. Both announce on import.
fn plant_shadows(dir: &std::path::Path) {
    std::fs::write(
        dir.join("json.py"),
        "import sys\nsys.stderr.write('SHADOW-json\\n'); sys.stderr.flush()\nraise RuntimeError('shadow json imported')\n",
    )
    .unwrap();
    let pkg = dir.join("pylair");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("__init__.py"),
        "import sys\nsys.stderr.write('SHADOW-pylair\\n'); sys.stderr.flush()\nraise RuntimeError('shadow pylair imported')\n",
    )
    .unwrap();
    std::fs::write(pkg.join("__main__.py"), "raise SystemExit(97)\n").unwrap();
}

#[test]
fn release_spec_pins_safepath_cwd_and_scrubs() {
    let res = tests_dir(); // any existing dir works as "resources" here
    let spec = release_spawn_spec(&res, "testapp", "backend");
    assert_eq!(
        env_of(&spec, "PYTHONSAFEPATH"),
        Some("1"),
        "release spec must set PYTHONSAFEPATH=1"
    );
    assert_eq!(
        spec.cwd.as_deref().map(|p| p.canonicalize().unwrap()),
        Some(res.canonicalize().unwrap()),
        "release spec must pin cwd to the bundled resources dir"
    );
    for k in ["PYTHONWARNINGS", "PYTHONBREAKPOINT"] {
        assert!(
            spec.env_remove.iter().any(|n| n == k),
            "env_remove must scrub {k}"
        );
    }
}

/// The spec the fix produces must survive a cwd full of shadowing
/// modules — and so must the isolated worker it spawns (workers inherit
/// env, so PYTHONSAFEPATH propagates).
#[tokio::test]
async fn hostile_cwd_does_not_shadow_runtime_or_worker() {
    let res = std::env::temp_dir().join(format!("pylair-evil-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&res);
    std::fs::create_dir_all(&res).unwrap();
    plant_shadows(&res);

    // The packaged spec: interpreter + PYTHONPATH are patched to the
    // repo's real python and source tree (a real bundle resolves them
    // under resources/, which is exactly what we're shadow-testing).
    let mut spec = release_spawn_spec(&res, "testapp", "backend");
    spec.program = python();
    // The hazard is "cwd contents on sys.path". The fix pins cwd to the
    // resources dir; before it, a packaged spawn inherited the caller's
    // cwd. Pin the hostile dir explicitly so both generations of the
    // spec face the same fire — only PYTHONSAFEPATH distinguishes them.
    spec.cwd = Some(res.clone());
    let sep = if cfg!(windows) { ";" } else { ":" };
    spec.env.retain(|(k, _)| k != "PYTHONPATH");
    spec.env.push((
        "PYTHONPATH".into(),
        format!("{}{sep}{}", pylair_src().display(), tests_dir().display()),
    ));

    let (tx, mut rx) = mpsc::channel(256);
    let h = spawn_h(spec, cfg(), tx);

    let mut stderr = Vec::new();
    let ready = timeout(T, async {
        loop {
            match rx.recv().await {
                Some(RuntimeEvent::Stderr(l)) => stderr.push(l),
                Some(RuntimeEvent::StateChanged {
                    to: State::Ready, ..
                }) => {
                    return true;
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        ready,
        "runtime never reached Ready under a shadowing cwd: {stderr:?}"
    );

    // An isolated worker spawns from the runtime in the same cwd — its
    // `python -m pylair._worker` would exec the shadow package without
    // the safepath env it inherits.
    let call = timeout(T, async {
        h.call("isolated_add", json!({"a": 1, "b": 2}), None, Some(false))
            .await
    })
    .await;
    match call {
        Ok(Ok(v)) => assert_eq!(v, Value::from(3)),
        other => panic!("isolated call failed under shadowing cwd: {other:?}"),
    }
    h.shutdown().await;

    assert!(
        !stderr.iter().any(|l| l.contains("SHADOW")),
        "a shadowing module was imported: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&res);
}
