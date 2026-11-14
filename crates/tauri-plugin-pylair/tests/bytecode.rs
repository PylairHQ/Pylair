//! rc.15 PR H: the release spawn env forbids runtime bytecode writes.
//!
//! `release_spawn_spec` sets PYTHONDONTWRITEBYTECODE=1 because the bundle
//! ships precompiled pycs and the install dir may be read-only. These
//! tests prove the flag does its job against a real interpreter — the
//! property a read-only install relies on — and that the spec that
//! carries it is the one the packaged app uses.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tauri_plugin_pylair::release_spawn_spec;

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        let ok = Command::new(cand)
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

/// A writable module dir: `libhelper/mod.py` inside a temp root. The
/// read-only property under test is not "can Python write there" — it
/// is "with the release env, does Python *choose* not to". On Windows a
/// directory's read-only bit does not block writes anyway, so the env
/// flag is the only portable enforcement.
fn module_tree() -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("pylair-pycenv-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let pkg = root.join("libhelper");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("__init__.py"), "\n").unwrap();
    fs::write(pkg.join("mod.py"), "VALUE = 1\n").unwrap();
    (root, pkg)
}

fn import_with_env(root: &PathBuf, extra: &[(&str, &str)]) {
    let mut cmd = Command::new(python());
    cmd.arg("-c")
        .arg("import libhelper.mod")
        .env("PYTHONPATH", root)
        .env("PYTHONNOUSERSITE", "1")
        // The control import must write __pycache__ next to the module, so
        // neither of these may leak in from the developer's shell:
        // DONTWRITEBYTECODE would suppress the write, PYCACHEPREFIX would
        // redirect it. The release flag under test is passed via `extra`.
        .env_remove("PYTHONDONTWRITEBYTECODE")
        .env_remove("PYTHONPYCACHEPREFIX");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn release_env_suppresses_pycache_writes() {
    let (root, pkg) = module_tree();

    // Control: without the flag, importing writes __pycache__.
    import_with_env(&root, &[]);
    assert!(
        pkg.join("__pycache__").exists(),
        "control import should have written __pycache__"
    );
    fs::remove_dir_all(pkg.join("__pycache__")).unwrap();

    // The release spec's env, applied verbatim: no __pycache__.
    let spec = release_spawn_spec(std::path::Path::new("/res"), "backend.app", "backend");
    let kv: Vec<(String, String)> = spec
        .env
        .iter()
        .filter(|(k, _)| k == "PYTHONDONTWRITEBYTECODE")
        .cloned()
        .collect();
    assert_eq!(kv.len(), 1, "release spec must carry exactly one flag");
    import_with_env(&root, &[(&kv[0].0, &kv[0].1)]);
    assert!(
        !pkg.join("__pycache__").exists(),
        "PYTHONDONTWRITEBYTECODE=1 must suppress __pycache__ writes"
    );

    let _ = fs::remove_dir_all(&root);
}
