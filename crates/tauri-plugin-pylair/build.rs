// Declares the plugin's command ACL. Without a manifest Tauri has no
// permissions to grant for `plugin:pylair|*`, so every IPC call from the
// frontend is denied ("Plugin not found") regardless of capabilities.
const COMMANDS: &[&str] = &[
    "python_call",
    "python_call_stream",
    "python_grant_credit",
    "python_release_blob",
    "python_cancel",
    "python_retry",
    "python_state",
    "python_health",
    "python_shutdown",
    "python_selftest_report",
    "python_selftest_b_report",
    "python_bench_report",
    // Declared unconditionally so the permission files exist for
    // --with-updater scaffolds; on a feature-off build the commands are
    // absent and invoking them simply fails "command not found".
    "python_update_check",
    "python_update_download",
    "python_update_install",
    "python_update_relaunch",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
