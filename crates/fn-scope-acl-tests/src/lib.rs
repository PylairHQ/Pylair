//! The real compiled ACL for the per-function permission tests: `build.rs`
//! runs tauri-build over `capabilities/*.json` and the plugin's permission
//! files, and `generate_context!` embeds the resolved result. Driving a mock
//! webview against this context goes through Tauri's real authority
//! (`Webview::on_message`), which the stock `mock_context` cannot do: its ACL
//! is empty and it rejects every plugin command.

pub fn context() -> tauri::Context<tauri::test::MockRuntime> {
    tauri::generate_context!()
}
