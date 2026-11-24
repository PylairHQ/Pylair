//! App wiring. Wrap mode needs no code of your own: the plugin reads the
//! config `pylair dev` / `pylair build` wrote, starts your server under
//! supervision, builds the window (the local starting page, then your app,
//! and a restarting page when the server stops), locks navigation to your
//! app, and handles downloads.
//!
//! Installed self-test: set PYLAIR_SELF_TEST_WRAP=<text your page contains>
//! and the app checks that the window reached your app, that your page
//! contains the text, and that the page has no access to Tauri, then exits
//! 0 on success.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_pylair::Builder::wrap().build())
        .run(tauri::generate_context!())
        .expect("error while running application");
}
