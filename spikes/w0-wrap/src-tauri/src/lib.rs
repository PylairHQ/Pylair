//! W0 wrap spike: spawn the launcher under the real supervisor, navigate
//! the window to the app URL when `READY <url>` appears on stderr.
//! Throwaway - no plugin changes (that's what W0 is testing).

use tauri::{Emitter, Listener, Manager};
use tauri_plugin_pylair::{events, Registry, SpawnSpec};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_py = std::env::var("W0_APP").unwrap_or_else(|_| "fastapi".into());
    let spike_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // The venv interpreter; `python` alone would resolve to whatever PATH
    // finds, not the pinned env. Windows venvs keep it under Scripts\.
    let py = spike_dir
        .join(".venv")
        .join(if cfg!(windows) { "Scripts" } else { "bin" })
        .join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
    let pys = py.to_string_lossy().to_string();
    let cmd: Vec<String> = match app_py.as_str() {
        "gradio" => vec![pys.clone(), "apps/gradio_app.py".into()],
        "streamlit" => vec![
            pys.clone(),
            "-m".into(),
            "streamlit".into(),
            "run".into(),
            "apps/streamlit_app.py".into(),
            "--server.address".into(),
            "127.0.0.1".into(),
            "--server.port".into(),
            "{port}".into(),
            "--server.headless".into(),
            "true".into(),
            "--server.fileWatcherType".into(),
            "none".into(),
        ],
        "hung" => vec![pys.clone(), "apps/hung_server.py".into()],
        "orphan" => vec![pys.clone(), "apps/orphan_server.py".into()],
        _ => vec![pys.clone(), "apps/fastapi_app.py".into()],
    };

    // args = [launcher.py, --, cmd...]
    let mut spec = SpawnSpec {
        program: py,
        args: std::iter::once("launcher.py".into())
            .chain(std::iter::once("--".into()))
            .chain(cmd.into_iter())
            .collect(),
        ..Default::default()
    };
    spec.env = vec![
        ("PYTHONPATH".into(), ".".into()),
        ("PYTHONUTF8".into(), "1".into()),
    ];
    spec.cwd = Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));

    tauri::Builder::default()
        .plugin(tauri_plugin_pylair::Builder::new(spec).build())
        .setup(|app| {
            let win = app.get_webview_window("main").expect("main window");
            // Event scoping (P1-8): stderr events are emit_to'd only to
            // SUBSCRIBED windows. A wrap-mode page never calls a bridge
            // command, so nothing would subscribe "main" by itself. The
            // spike subscribes it host-side; JS in the page still gets no
            // IPC. A Rust global app.listen would still miss these events:
            // emit_to only reaches listeners attached to the window's own
            // target, which is why this listens on `win` not `app`.
            app.state::<Registry>().lock().unwrap().subscribe("main");
            let app_handle = app.handle().clone();
            win.clone().listen(events::STDERR, move |ev| {
                // payload() is a JSON string: decode it. On Windows the
                // stderr line ends \r, which JSON escapes to a literal
                // backslash-r. Leaving it in the URL makes Url::parse read
                // the backslash as a slash and navigate to /r (a 404).
                let line = serde_json::from_str::<String>(ev.payload())
                    .unwrap_or_else(|_| ev.payload().to_string());
                let line = line.trim_end();
                eprintln!("W0-EVENT: {line}");
                if let Some(url) = line.strip_prefix("READY ") {
                    let url = url.trim().to_string();
                    if let Ok(parsed) = url.parse::<url::Url>() {
                        eprintln!("W0-NAVIGATE {parsed}");
                        let _ = win.navigate(parsed);
                    }
                    let _ = app_handle.emit("w0://ready", url);
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running application");
}
