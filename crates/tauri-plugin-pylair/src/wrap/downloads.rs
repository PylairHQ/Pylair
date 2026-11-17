//! Downloads from the wrapped page (docs/decisions/0003-wrap-mode.md, UX
//! decision 1): a native Save As dialog by default, or straight into the
//! user's Downloads folder (`[tool.pylair.wrap] downloads = "downloads"`),
//! and feedback after saving either way.
//!
//! The decisions (safe file name, never overwrite, the feedback text) are in
//! [`super::policy`]; this file is the webview glue.

use std::sync::Arc;

use tauri::webview::DownloadEvent;
use tauri::{Manager, Runtime, Webview};

use super::policy::{self, DownloadMode};
use super::Ui;

/// The webview's download hook. Returning `false` cancels the download.
///
/// Called on the UI thread. A Save As dialog is modal and brief; the wry
/// handler is synchronous by design (the answer is the destination).
pub(super) fn handle<R: Runtime>(
    ui: &Arc<Ui<R>>,
    webview: Webview<R>,
    event: DownloadEvent<'_>,
) -> bool {
    match event {
        DownloadEvent::Requested { destination, .. } => {
            let name = policy::suggested_name(destination);
            match ui.downloads {
                DownloadMode::Ask => {
                    let mut dialog = rfd::FileDialog::new()
                        .set_title("Save as")
                        .set_file_name(&name);
                    if let Ok(dir) = webview.app_handle().path().download_dir() {
                        dialog = dialog.set_directory(dir);
                    }
                    match dialog.save_file() {
                        Some(path) => {
                            *destination = path;
                            true
                        }
                        // The user cancelled the dialog: no download.
                        None => false,
                    }
                }
                DownloadMode::Downloads => {
                    let Ok(dir) = webview.app_handle().path().download_dir() else {
                        toast(&webview, "Download failed: no Downloads folder");
                        return false;
                    };
                    if std::fs::create_dir_all(&dir).is_err() {
                        toast(
                            &webview,
                            "Download failed: cannot create the Downloads folder",
                        );
                        return false;
                    }
                    *destination = policy::unique_in(&dir, &name, |p| p.exists());
                    true
                }
            }
        }
        DownloadEvent::Finished { path, success, .. } => {
            let shown = path.unwrap_or_default();
            *ui.last_download.lock().unwrap() = Some((shown.clone(), success));
            toast(&webview, &policy::download_feedback(&shown, success));
            true
        }
        // DownloadEvent is non-exhaustive.
        _ => true,
    }
}

/// A small non-modal note in the corner of the page for a few seconds. The
/// text goes through a JSON string, never into the script as code.
fn toast<R: Runtime>(webview: &Webview<R>, text: &str) {
    let msg = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let js = format!(
        "(function(){{var m={msg};var d=document.createElement('div');d.textContent=m;\
         d.setAttribute('role','status');\
         d.style.cssText='position:fixed;right:16px;bottom:16px;max-width:60vw;padding:10px 14px;\
         background:#222;color:#fff;font:14px system-ui,sans-serif;border-radius:6px;\
         box-shadow:0 2px 8px rgba(0,0,0,.4);z-index:2147483647;word-break:break-all';\
         (document.body||document.documentElement).appendChild(d);\
         setTimeout(function(){{d.remove()}},6000)}})()"
    );
    let _ = webview.eval(&js);
}
