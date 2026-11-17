//! SPIKE ONLY. The real compiled ACL for the tests: `build.rs` runs
//! tauri-build over `capabilities/*.json` and the plugin's permission files,
//! and `generate_context!` embeds the resolved result. Driving a mock
//! webview against this context goes through Tauri's real authority.

pub fn context() -> tauri::Context<tauri::test::MockRuntime> {
    tauri::generate_context!()
}
