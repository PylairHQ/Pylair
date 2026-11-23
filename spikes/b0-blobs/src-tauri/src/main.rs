// Throwaway proof for docs/decisions/0010-blob-urls.md.
// Registers a `pylair` URI scheme that serves per-window-owned blobs
// from a directory on disk with manual Range/206 support, then lets two
// webviews (main, other) probe: element loads, fetch, XHR, Web Worker
// fetch, Range seeks, and cross-window reads (must 403).
// Every request logs ctx.webview_label() so we can see whether owner
// attribution survives each request type on this webview.

use std::{
    collections::HashMap, fs, path::PathBuf, sync::Mutex, thread, time::Duration,
};

use tauri::{
    http::{self, StatusCode},
    Manager, WebviewUrl, WebviewWindowBuilder,
};

struct Blob {
    path: PathBuf,
    mime: &'static str,
    owner: &'static str,
}

struct AppState {
    blobs: Mutex<HashMap<String, Blob>>,
    scheme_log: Mutex<Vec<String>>,
    reports: Mutex<Vec<serde_json::Value>>,
    out_dir: PathBuf,
}

/// 16-bit stereo 44.1kHz PCM WAV, `secs` of a 440 Hz sine — large enough
/// that <audio> seeks issue visible Range requests.
fn wav_bytes(secs: f64) -> Vec<u8> {
    let rate = 44_100u32;
    let frames = (rate as f64 * secs) as u32;
    let block = 4u32; // 2ch * 2B
    let data_len = frames * block;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&2u16.to_le_bytes()); // stereo
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * block).to_le_bytes());
    v.extend_from_slice(&(block as u16).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        let s = ((i as f64 / rate as f64) * 440.0 * std::f64::consts::TAU).sin();
        let sample = (s * 20_000.0) as i16;
        v.extend_from_slice(&sample.to_le_bytes());
        v.extend_from_slice(&sample.to_le_bytes());
    }
    v
}

fn log_line(st: &AppState, line: String) {
    eprintln!("[b0] {line}");
    st.scheme_log.lock().unwrap().push(line);
}

fn serve(label: &str, req: &http::Request<Vec<u8>>, st: &AppState) -> http::Response<Vec<u8>> {
    let path = req.uri().path().to_string();
    let range = req
        .headers()
        .get(http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let respond = |st: &AppState, status: StatusCode, mime: &str, body: Vec<u8>, extra: &[(&str, String)]| {
        log_line(
            st,
            format!(
                "SCHEME label={label} method={} path={path} range={range} -> {}",
                req.method(),
                status.as_u16()
            ),
        );
        let mut b = http::Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, mime)
            .header(http::header::CONTENT_LENGTH, body.len().to_string())
            .header(http::header::ACCEPT_RANGES, "bytes")
            // Blob URLs are cross-origin to the app origin on every
            // platform; fetch/XHR/workers need ACAO or they fail CORS.
            .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
        for (k, v) in extra {
            b = b.header(*k, v);
        }
        b.body(body).unwrap()
    };

    let id = match path.strip_prefix("/b/") {
        Some(id) if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => id.to_string(),
        _ => return respond(st, StatusCode::BAD_REQUEST, "text/plain", b"bad blob path".to_vec(), &[]),
    };

    let blobs = st.blobs.lock().unwrap();
    let Some(blob) = blobs.get(&id) else {
        drop(blobs);
        return respond(st, StatusCode::NOT_FOUND, "text/plain", b"no such blob".to_vec(), &[]);
    };
    if label != blob.owner {
        drop(blobs);
        return respond(st, StatusCode::FORBIDDEN, "text/plain", b"blob owned by another window".to_vec(), &[]);
    }
    let (path, mime) = (blob.path.clone(), blob.mime);
    drop(blobs);

    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) => return respond(st, StatusCode::INTERNAL_SERVER_ERROR, "text/plain", format!("read: {e}").into_bytes(), &[]),
    };
    let total = data.len() as u64;

    if range == "-" {
        return respond(st, StatusCode::OK, mime, data, &[]);
    }
    // bytes=start-end | bytes=start- | bytes=-suffix
    let Some(spec) = range.strip_prefix("bytes=") else {
        return respond(st, StatusCode::RANGE_NOT_SATISFIABLE, "text/plain", b"unsupported range unit".to_vec(), &[]);
    };
    let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
    let (start, end) = if a.is_empty() {
        let suffix: u64 = b.parse().unwrap_or(0);
        (total.saturating_sub(suffix), total.saturating_sub(1))
    } else {
        let s: u64 = a.parse().unwrap_or(u64::MAX);
        let e = if b.is_empty() { total - 1 } else { b.parse::<u64>().unwrap_or(u64::MAX) };
        (s, e.min(total - 1))
    };
    if start > end || start >= total {
        return respond(st, StatusCode::RANGE_NOT_SATISFIABLE, "text/plain", b"range unsatisfiable".to_vec(), &[]);
    }
    let slice = data[start as usize..=end as usize].to_vec();
    respond(
        st,
        StatusCode::PARTIAL_CONTENT,
        mime,
        slice,
        &[(
            "Content-Range",
            format!("bytes {start}-{end}/{total}"),
        )],
    )
}

#[tauri::command]
fn b0_blob_ids(st: tauri::State<AppState>) -> serde_json::Value {
    let blobs = st.blobs.lock().unwrap();
    blobs
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!({"owner": v.owner, "mime": v.mime})))
        .collect()
}

#[tauri::command]
fn b0_scheme_log(st: tauri::State<AppState>) -> Vec<String> {
    st.scheme_log.lock().unwrap().clone()
}

#[tauri::command]
fn b0_report(st: tauri::State<AppState>, report: serde_json::Value) {
    let done = {
        let mut reports = st.reports.lock().unwrap();
        reports.push(report);
        reports.len() >= 2
    };
    if done {
        let out = st.out_dir.join("results.json");
        let body = serde_json::json!({
            "reports": st.reports.lock().unwrap().clone(),
            "scheme_log": st.scheme_log.lock().unwrap().clone(),
        });
        fs::write(&out, serde_json::to_string_pretty(&body).unwrap()).unwrap();
        eprintln!("[b0] results written to {}", out.display());
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(400));
            std::process::exit(0);
        });
    }
}

fn main() {
    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("pylair", |ctx, req, responder| {
            let app = ctx.app_handle().clone();
            let label = ctx.webview_label().to_string();
            thread::spawn(move || {
                let st = app.state::<AppState>();
                responder.respond(serve(&label, &req, &st));
            });
        })
        .invoke_handler(tauri::generate_handler![b0_blob_ids, b0_scheme_log, b0_report])
        .setup(|app| {
            let out_dir = std::env::var("PYLAIR_B0_OUT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::current_dir().unwrap().join("out"));
            let blob_dir = out_dir.join("blobs");
            fs::create_dir_all(&blob_dir).unwrap();

            let png = blob_dir.join("img_main.png");
            fs::write(&png, include_bytes!("../fixture.png")).unwrap();
            let png_b = blob_dir.join("img_other.png");
            fs::write(&png_b, include_bytes!("../fixture.png")).unwrap();
            let wav = blob_dir.join("audio_main.wav");
            // ~21 MB — past Chromium's media buffer so seeking past the
            // buffered head must issue a second Range request.
            fs::write(&wav, wav_bytes(120.0)).unwrap();

            let mut blobs = HashMap::new();
            blobs.insert(
                "img_main".into(),
                Blob { path: png, mime: "image/png", owner: "main" },
            );
            blobs.insert(
                "audio_main".into(),
                Blob { path: wav, mime: "audio/wav", owner: "main" },
            );
            blobs.insert(
                "img_other".into(),
                Blob { path: png_b, mime: "image/png", owner: "other" },
            );

            app.manage(AppState {
                blobs: Mutex::new(blobs),
                scheme_log: Mutex::new(Vec::new()),
                reports: Mutex::new(Vec::new()),
                out_dir,
            });

            for (label, page) in [("main", "index.html"), ("other", "other.html")] {
                WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
                    .title(format!("b0 {label}"))
                    .inner_size(800.0, 600.0)
                    .build()?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("b0-blobs run failed");
}
