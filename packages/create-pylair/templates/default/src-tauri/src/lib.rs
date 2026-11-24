//! App wiring. The interpreter spec and the bundled self-test live in the
//! plugin — fixes there reach every app via a plugin update.
//!
//! `--self-test` / PYLAIR_SELF_TEST: headless release-path check (no window).
//! PYLAIR_SELF_TEST_GUI: same check inside setup(), through resource_dir().
//! PYLAIR_SELF_TEST_WINDOW_CLOSE: open the window, close it after a beat —
//! exercises WindowEvent::Destroyed on the main thread (the rc.3 crash
//! path) and exits 0 only if teardown stays panic-free.
//!
//! Lifecycle tests (scripts/dogfood-windows.ps1) need no runtime-start
//! hook: the plugin spawns the Python child eagerly in setup(), so
//! polling for python under the install dir is the positive control.

use tauri::Manager;
use tauri_plugin_pylair::{app_spawn_spec, self_test_gui, self_test_headless, self_test_requested};

const APP_MODULE: &str = "backend.app";
const SOURCE_DIR: &str = "backend";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if self_test_requested() {
        std::process::exit(self_test_headless(APP_MODULE, SOURCE_DIR));
    }
    let gui_test = std::env::var("PYLAIR_SELF_TEST_GUI").is_ok();
    let close_test = std::env::var("PYLAIR_SELF_TEST_WINDOW_CLOSE").is_ok();
    tauri::Builder::default()
        .plugin(
            tauri_plugin_pylair::Builder::new_with(|app| {
                app_spawn_spec(app, APP_MODULE, SOURCE_DIR)
            })
            .build(),
        )
        .setup(move |app| {
            if gui_test {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let code = self_test_gui(&handle, APP_MODULE, SOURCE_DIR).await;
                    handle.exit(code);
                });
            }
            if close_test {
                // Window::close dispatches to the main loop internally —
                // safe from this bare thread. The Destroyed arm then runs
                // on the main thread; a panic there fails the process.
                if let Some(w) = app.get_webview_window("main") {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        let _ = w.close();
                    });
                    // Watchdog: a panic is instant, but a swallowed close
                    // (an OS keeping the app resident) must fail loudly
                    // instead of hanging the CI leg.
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_secs(60));
                        std::process::exit(5);
                    });
                } else {
                    // Close mode armed but no window — fail now, don't hang.
                    std::process::exit(4);
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running application");
}
