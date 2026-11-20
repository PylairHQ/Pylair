// Declares the plugin's command ACL. Without a manifest Tauri has no
// permissions to grant for `plugin:pylair|*`, so every IPC call from the
// frontend is denied ("Plugin not found") regardless of capabilities.
const COMMANDS: &[&str] = &[
    "python_call",
    "python_call_stream",
    "python_grant_credit",
    "python_cancel",
    "python_retry",
    "python_state",
    "python_health",
    "python_shutdown",
    "python_selftest_report",
    "python_selftest_b_report",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
