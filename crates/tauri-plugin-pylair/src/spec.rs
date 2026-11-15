use crate::SpawnSpec;
use std::path::{Path, PathBuf};
use tauri::{Manager, Runtime};

/// The interpreter spec for a packaged app, given the directory holding
/// `python/`, `site-packages/` and the app's source dir. Kept here (not in
/// app code) so path/env fixes reach every app via a plugin update.
pub fn release_spawn_spec(resources: &Path, app_module: &str, source_dir: &str) -> SpawnSpec {
    // Tauri's resource_dir is canonicalized, so on Windows it arrives as a
    // \\?\ verbatim path — Python code reading sys.executable or sys.path
    // can trip on the prefix. Simplify it back when safe (no-op elsewhere).
    let resources = dunce::simplified(resources);
    let python = resources.join("python").join(if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    });
    let sep = if cfg!(windows) { ";" } else { ":" };
    // Import root = PARENT of the source dir: "backend.app" resolves from
    // resources/, not resources/backend/ (mirrors cli `importRoot`).
    let import_root = resources.join(Path::new(source_dir).parent().unwrap_or(Path::new("")));
    let pythonpath = format!(
        "{}{}{}",
        import_root.display(),
        sep,
        resources.join("site-packages").display()
    );
    let mut spec = SpawnSpec::pylair(python, app_module);
    spec.env.push(("PYTHONPATH".into(), pythonpath));
    spec.env.push(("PYTHONUTF8".into(), "1".into()));
    spec.env.push(("PYTHONNOUSERSITE".into(), "1".into()));
    // `python -m` puts the child's cwd first on sys.path — a json.py,
    // asyncio/ or pylair/ in the directory the user launched from would
    // shadow the stdlib or the runtime itself (isolated workers inherit
    // this env, so they are covered too). PYTHONSAFEPATH (3.11+, and our
    // bundled runtime is 3.12) removes the implicit entry; PYTHONPATH
    // above still resolves the app module and bundled site-packages.
    spec.env.push(("PYTHONSAFEPATH".into(), "1".into()));
    // The bundle is precompiled at build time and the install dir is
    // often read-only (per-machine installs, .app bundles): never write
    // bytecode at launch. Isolated workers inherit os.environ, so they
    // are covered too — same for subprocesses the app itself spawns,
    // unless that call passes its own env. Dev mode is untouched:
    // __pycache__ in the source tree is normal and useful there.
    spec.env
        .push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    // Pin cwd to the bundle rather than whatever dir launched the app —
    // deterministic for any code that opens relative paths, and empty
    // of user-writable shadow candidates.
    spec.cwd = Some(resources.to_path_buf());
    // The packaged interpreter must not inherit Python configuration from
    // whatever launched the app: an AppImage's AppRun exports PYTHONHOME
    // (pointing at the AppDir), which breaks the bundled stdlib lookup.
    // PYTHONBREAKPOINT names a module imported by breakpoint() — a launch
    // env var must not get code executed inside the runtime.
    spec.env_remove.extend(
        [
            "PYTHONHOME",
            "PYTHONSTARTUP",
            "PYTHONEXECUTABLE",
            "PYTHONUSERBASE",
            "PYTHONINSPECT",
            "PYTHONPLATLIBDIR",
            "PYTHONWARNINGS",
            "PYTHONBREAKPOINT",
        ]
        .map(String::from),
    );
    scrub_appdir(&mut spec, |k| std::env::var(k).ok());
    spec
}

/// An AppImage's AppRun exports `APPDIR` and prepends its own dirs to
/// `LD_LIBRARY_PATH` and `PATH`. A bundled Python inheriting those can load
/// the AppImage's `libstdc++`/`libssl` instead of the system's and break
/// native wheels (numpy, torch): drop every path entry under `APPDIR`.
/// Removing the var entirely is the fallback when nothing else remains.
pub(super) fn scrub_appdir(spec: &mut SpawnSpec, env: impl Fn(&str) -> Option<String>) {
    let Some(appdir) = env("APPDIR") else { return };
    let appdir = appdir.trim_end_matches('/');
    if appdir.is_empty() {
        return;
    }
    for name in ["LD_LIBRARY_PATH", "PATH"] {
        let Some(value) = env(name) else { continue };
        let kept: Vec<&str> = value
            .split(':')
            .filter(|e| {
                let e = e.trim_end_matches('/');
                !(e == appdir || e.strip_prefix(appdir).is_some_and(|r| r.starts_with('/')))
            })
            .collect();
        if kept.is_empty() {
            spec.env_remove.push(name.into());
        } else {
            spec.env.push((name.into(), kept.join(":")));
        }
    }
}

/// Why a spawn spec could not be computed. Surfaced through plugin setup
/// as a startup error instead of a panic.
#[derive(Debug)]
#[non_exhaustive]
pub enum SpawnSpecError {
    /// `app.path().resource_dir()` failed AND no bundled resources were
    /// found next to the executable either. Carries Tauri's error — on
    /// macOS it names the symlinked ancestor Tauri refuses to run under.
    ResourceDir(String),
}

impl std::fmt::Display for SpawnSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResourceDir(e) => write!(
                f,
                "resource_dir() failed ({e}) and no bundled resources were \
                 found next to the executable. On macOS this happens when \
                 the app is launched through a symlinked path (e.g. /tmp \
                 -> /private/tmp): launch the app's real path instead."
            ),
        }
    }
}

impl std::error::Error for SpawnSpecError {}

/// Dev-or-release spec: `pylair dev` exports PYLAIR_APP_MODULE / PYLAIR_PYTHON
/// / PYLAIR_PYTHONPATH / PYLAIR_RESTART_TRIGGER → the project venv; a packaged
/// build resolves `resource_dir`. Apps use this as the plugin's spec fn.
pub fn app_spawn_spec<R: Runtime>(
    app: &tauri::AppHandle<R>,
    app_module: &str,
    source_dir: &str,
) -> Result<SpawnSpec, SpawnSpecError> {
    // Development overrides are honored in debug builds only: a packaged app
    // must never let the launching environment redirect which interpreter or
    // module it runs.
    if cfg!(debug_assertions) {
        if let Ok(module) = std::env::var("PYLAIR_APP_MODULE") {
            let program = std::env::var("PYLAIR_PYTHON").unwrap_or_else(|_| "python3".into());
            let mut spec = SpawnSpec::pylair(PathBuf::from(program), &module);
            if let Ok(pp) = std::env::var("PYLAIR_PYTHONPATH") {
                spec.env.push(("PYTHONPATH".into(), pp));
            }
            // NB: PYLAIR_RESTART_TRIGGER is read by the supervisor from its own
            // env (inherited from `pylair dev`) — not forwarded to the child.
            return Ok(spec);
        }
    }
    let res = bundled_resource_dir(app.path().resource_dir())?;
    Ok(release_spawn_spec(&res, app_module, source_dir))
}

/// `resource_dir()` first, exe-relative fallback second. Tauri rejects an
/// exe path containing a symlinked ancestor on macOS (the StartingBinary
/// hardening behind `current_exe`) — a launch through `/tmp` errors out
/// even though the bundle is perfectly readable. The fallback resolves
/// resources relative to the canonicalized executable, so that launch
/// still works; if both fail the error names the cause and the fix.
pub(super) fn bundled_resource_dir(
    resource_dir: Result<PathBuf, tauri::Error>,
) -> Result<PathBuf, SpawnSpecError> {
    match resource_dir {
        Ok(dir) => Ok(dir),
        Err(e) => resources_from_exe().ok_or_else(|| SpawnSpecError::ResourceDir(e.to_string())),
    }
}

pub(super) fn resources_from_exe() -> Option<PathBuf> {
    // Canonicalize first: a launch through a symlinked path (e.g. /tmp on
    // macOS → /private/tmp) is exactly the case Tauri's resource_dir()
    // rejects; resolving to the real exe path is what makes this fallback
    // succeed where resource_dir() failed.
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    resources_near_exe(&exe)
}

pub(super) fn resources_near_exe(exe: &std::path::Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_string_lossy();
    [
        dir.join("..").join("lib").join(stem.as_ref()), // deb/AppImage usr/lib
        dir.join("..").join("Resources"),               // macOS .app
        dir.to_path_buf(),                              // NSIS install root
    ]
    .into_iter()
    .find(|p| p.join("python").exists())
    // Normalize `..` away so callers get the real bundle path.
    .and_then(|p| p.canonicalize().ok())
}
