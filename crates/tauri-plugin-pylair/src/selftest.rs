use crate::spec::resources_from_exe;
use crate::{
    app_spawn_spec, release_spawn_spec, spawn_supervisor, RuntimeEvent, SpawnSpec, State,
    SupervisorConfig,
};
use serde_json::json;
use tauri::Runtime;

// ── bundled self-test ───────────────────────────────────────────────
// Release-path verification apps can ship: `pylair-self-test` resolves the
// bundled resources exactly as the packaged app sees them, spawns the real
// supervisor, makes one real call, prints, exits. Two entry points:
//   headless — before any AppHandle exists (no window, no Tauri loop);
//   gui      — inside setup(), exercising app.path().resource_dir() itself.

/// A GUI-subsystem exe has no console of its own — `println!`/`eprintln!`
/// (and panic reports) go nowhere. When the process was launched from a
/// terminal — the self-test CI legs — steal the parent's console so the
/// verdict still reaches it. Harmless no-op without a console host.
#[cfg(windows)]
pub(super) fn attach_parent_console() {
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

#[cfg(not(windows))]
pub(super) fn attach_parent_console() {}

async fn self_test_call(spec: SpawnSpec, origin: &str) -> i32 {
    // Print the interpreter actually being spawned — on a clean machine
    // this is the proof the bundled runtime is what's running, not a
    // system or project Python the app happened to find.
    eprintln!(
        "self-test({origin}): interpreter {}",
        spec.program.display()
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (h, actor) = spawn_supervisor(spec, SupervisorConfig::default(), tx);
    tokio::spawn(actor);
    // Surface the runtime's stderr and lifecycle: a self-test that only says
    // "never reached Ready" is undiagnosable in CI.
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                RuntimeEvent::Stderr(line) => eprintln!("[runtime stderr] {line}"),
                RuntimeEvent::StateChanged { from, to, detail } => {
                    eprintln!("[runtime state] {from:?} -> {to:?}: {detail}")
                }
                RuntimeEvent::Exposes(_) => {}
                RuntimeEvent::HealthChanged(_) => {}
                RuntimeEvent::AppChanged(_) => {}
            }
        }
    });
    let mut rx = h.subscribe_state();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while *rx.borrow() != State::Ready {
            if rx.changed().await.is_err() {
                break;
            }
        }
        *rx.borrow() == State::Ready
    })
    .await;
    match ready {
        Ok(true) => match h
            .call("hello", json!({"name": "self-test"}), None, Some(false))
            .await
        {
            Ok(v) => {
                println!("self-test({origin}) ok: {v}");
                h.shutdown().await;
                0
            }
            Err(e) => {
                eprintln!("self-test({origin}): call failed: {e:?}");
                3
            }
        },
        _ => {
            eprintln!(
                "self-test({origin}): never reached Ready (state: {:?})",
                h.state()
            );
            4
        }
    }
}

/// Headless: resources located relative to the executable — runs on a bare
/// machine with no display and no system Python. Returns an exit code.
pub fn self_test_headless(app_module: &str, source_dir: &str) -> i32 {
    attach_parent_console();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("self-test: cannot build tokio runtime: {e}");
            return 2;
        }
    };
    rt.block_on(async {
        match resources_from_exe() {
            Some(res) => {
                self_test_call(release_spawn_spec(&res, app_module, source_dir), "headless").await
            }
            None => {
                eprintln!("self-test: no bundled resources near the executable");
                2
            }
        }
    })
}

/// GUI: runs inside the app's setup() — exercises resource_dir() itself.
/// Spawn on the Tauri runtime; exit the app with the returned code.
pub async fn self_test_gui<R: Runtime>(
    app: &tauri::AppHandle<R>,
    app_module: &str,
    source_dir: &str,
) -> i32 {
    attach_parent_console();
    let spec = match app_spawn_spec(app, app_module, source_dir) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("self-test(gui): spawn spec failed: {e}");
            return 2;
        }
    };
    self_test_call(spec, "gui").await
}

/// True when the process should run the headless self-test instead of the app.
pub fn self_test_requested() -> bool {
    std::env::args().any(|a| a == "--self-test") || std::env::var("PYLAIR_SELF_TEST").is_ok()
}
