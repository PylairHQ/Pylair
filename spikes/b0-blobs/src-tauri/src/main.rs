// Throwaway proof for docs/decisions/0010-blob-urls.md.
// Registers a `pylair` URI scheme that serves per-window-owned blobs
// from a directory on disk with manual Range/206, bounded response
// bodies (full-body and per-range caps), owner enforcement on
// ctx.webview_label(), and an Origin check limited to the app origin.
// Windows "main" (owner) and "other" run a browser-side battery; after
// both report, "main" is navigated to a foreign http://127.0.0.1 page —
// same label, foreign origin — which must be refused on scripted reads.
// Every request logs label + origin so attribution is visible.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write as _},
    net::TcpListener,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::Duration,
};

use tauri::{
    http::{self, StatusCode},
    Manager, WebviewUrl, WebviewWindowBuilder,
};

// Deliberately small so the fixtures exercise the caps: the PNG (10 KiB)
// clears FULL_BODY_CAP; the WAV (21 MiB) does not, and every media range
// is truncated to RANGE_CAP — proving short 206s are accepted.
const FULL_BODY_CAP: u64 = 1 << 20; // 1 MiB
const RANGE_CAP: u64 = 4 << 20; // 4 MiB

const APP_ORIGIN: &str = if cfg!(any(windows, target_os = "android")) {
    "http://tauri.localhost"
} else {
    "tauri://localhost"
};

struct Blob {
    path: PathBuf,
    mime: &'static str,
    owner: &'static str,
    /// from_path semantics: (size, mtime) recorded at registration;
    /// a mismatch at serve time means the source was rewritten in place.
    reg_size: u64,
    reg_mtime: std::time::SystemTime,
}

struct AppState {
    blobs: Mutex<HashMap<String, Blob>>,
    scheme_log: Mutex<Vec<String>>,
    reports: Mutex<Vec<serde_json::Value>>,
    out_dir: PathBuf,
    rogue_origin: String,
    done: AtomicBool,
}

/// 16-bit stereo 44.1kHz PCM WAV, `secs` of a 440 Hz sine.
fn wav_bytes(secs: f64) -> Vec<u8> {
    let rate = 44_100u32;
    let frames = (rate as f64 * secs) as u32;
    let block = 4u32;
    let data_len = frames * block;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
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

fn stat(p: &PathBuf) -> Option<(u64, std::time::SystemTime)> {
    let m = fs::metadata(p).ok()?;
    Some((m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)))
}

fn serve(label: &str, req: &http::Request<Vec<u8>>, st: &AppState) -> http::Response<Vec<u8>> {
    let path = req.uri().path().to_string();
    let range = req
        .headers()
        .get(http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let origin = req
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let respond = |st: &AppState, status: StatusCode, mime: &str, body: Vec<u8>, extra: &[(&str, String)]| {
        log_line(
            st,
            format!(
                "SCHEME label={label} origin={origin} method={} path={path} range={range} -> {}",
                req.method(),
                status.as_u16()
            ),
        );
        let mut b = http::Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, mime)
            .header(http::header::CONTENT_LENGTH, body.len().to_string())
            .header(http::header::ACCEPT_RANGES, "bytes")
            // Never *: echo exactly the app origin so credentialed CORS
            // reads from any other origin fail.
            .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, APP_ORIGIN)
            .header(http::header::VARY, "Origin");
        for (k, v) in extra {
            b = b.header(*k, v);
        }
        b.body(body).unwrap()
    };

    // Path validation — ids only, ever. The `zzdone` sentinel ends the
    // run; it must answer before the Origin gate since the rogue page
    // is foreign by design.
    let id = match path.strip_prefix("/b/") {
        Some(id) if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => id.to_string(),
        _ => return respond(st, StatusCode::BAD_REQUEST, "text/plain", b"bad blob path".to_vec(), &[]),
    };
    if id == "zzdone" {
        st.done.store(true, Ordering::SeqCst);
        return respond(st, StatusCode::NO_CONTENT, "text/plain", Vec::new(), &[]);
    }

    // 1. Origin gate: a foreign page loaded in ANY window may not read
    //    blobs. Absent Origin = an element/no-cors load — no scripted
    //    readback channel (canvas taints), so the label check is enough.
    if origin != "-" && origin != APP_ORIGIN {
        return respond(st, StatusCode::FORBIDDEN, "text/plain", b"foreign origin".to_vec(), &[]);
    }
    if req.method() == http::Method::OPTIONS {
        return respond(
            st,
            StatusCode::NO_CONTENT,
            "text/plain",
            Vec::new(),
            &[(http::header::ACCESS_CONTROL_ALLOW_HEADERS.as_str(), "Range".into())],
        );
    }

    // 2. Owner binding by task→window label.
    let blobs = st.blobs.lock().unwrap();
    let Some(blob) = blobs.get(&id) else {
        drop(blobs);
        return respond(st, StatusCode::NOT_FOUND, "text/plain", b"no such blob".to_vec(), &[]);
    };
    if label != blob.owner {
        drop(blobs);
        return respond(st, StatusCode::FORBIDDEN, "text/plain", b"blob owned by another window".to_vec(), &[]);
    }
    let (path, mime, reg_size, reg_mtime) = (blob.path.clone(), blob.mime, blob.reg_size, blob.reg_mtime);
    drop(blobs);

    // 4. Registered-metadata check: from_path blobs are hardlinks into
    //    the dir; an in-place rewrite of the source changes size+mtime
    //    under the same inode → 410 Gone, never silently-stale bytes.
    match stat(&path) {
        Some((sz, mt)) if sz == reg_size && mt == reg_mtime => {}
        _ => return respond(st, StatusCode::GONE, "text/plain", b"blob source changed".to_vec(), &[]),
    }

    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) => return respond(st, StatusCode::INTERNAL_SERVER_ERROR, "text/plain", format!("read: {e}").into_bytes(), &[]),
    };
    let total = data.len() as u64;

    if range == "-" {
        // 5. Full-body cap: a plain GET of a large blob would hold the
        //    whole thing in host memory — refuse and point at ranges.
        if total > FULL_BODY_CAP {
            return respond(
                st,
                StatusCode::RANGE_NOT_SATISFIABLE,
                "text/plain",
                format!("full-body read refused: {total} bytes > {FULL_BODY_CAP} cap; issue a Range request").into_bytes(),
                &[],
            );
        }
        return respond(st, StatusCode::OK, mime, data, &[]);
    }

    // 6. Range: serve at most RANGE_CAP bytes — a shorter 206 than
    //    requested is legal HTTP; media stacks re-request.
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
    let end = end.min(start + RANGE_CAP - 1); // truncate to the cap
    let slice = data[start as usize..=end as usize].to_vec();
    respond(
        st,
        StatusCode::PARTIAL_CONTENT,
        mime,
        slice,
        &[("Content-Range", format!("bytes {start}-{end}/{total}"))],
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

/// Rewrite the mutable-source blob's file — proves 410 on next read.
#[tauri::command]
fn b0_mutate(st: tauri::State<AppState>) {
    let blobs = st.blobs.lock().unwrap();
    if let Some(b) = blobs.get("mut_main") {
        fs::write(&b.path, b"shorter").unwrap();
    }
}

#[tauri::command]
fn b0_report(app: tauri::AppHandle, st: tauri::State<AppState>, report: serde_json::Value) {
    let done = {
        let mut reports = st.reports.lock().unwrap();
        reports.push(report);
        reports.len() >= 2
    };
    if done {
        // Navigate the OWNER window to a foreign origin — label stays
        // "main"; the Origin check (not the label) must now deny.
        let url = st.rogue_origin.clone();
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.navigate(url.parse().unwrap());
        }
        // Watchdog: the rogue page ends with a /b/zzdone beacon; if it
        // never arrives, finish anyway in 30 s.
        let app2 = app.clone();
        thread::spawn(move || {
            for _ in 0..300 {
                thread::sleep(Duration::from_millis(100));
                if app2.state::<AppState>().done.load(Ordering::SeqCst) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(500));
            finish(app2);
        });
    }
}

fn finish(app: tauri::AppHandle) {
    let st = app.state::<AppState>();
    let out = st.out_dir.join("results.json");
    let body = serde_json::json!({
        "reports": st.reports.lock().unwrap().clone(),
        "scheme_log": st.scheme_log.lock().unwrap().clone(),
    });
    let _ = fs::write(&out, serde_json::to_string_pretty(&body).unwrap());
    eprintln!("[b0] results written to {}", out.display());
    std::process::exit(0);
}

/// Minimal 127.0.0.1 server: one foreign-origin page + its worker.js.
/// The page's Origin header on scheme requests proves the gate works.
fn rogue_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().take(64) {
            let mut s = stream.unwrap();
            let mut line = String::new();
            if BufReader::new(s.try_clone().unwrap()).read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (mime, body) = match path.as_str() {
                "/worker.js" => ("text/javascript", ROGUE_WORKER.to_string()),
                "/" => ("text/html", ROGUE_PAGE.replace("__PORT__", &port.to_string())),
                _ => ("text/plain", "404".into()),
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

const ROGUE_WORKER: &str = r#"onmessage = async (e) => {
  try { const r = await fetch(e.data); postMessage({ status: r.status }); }
  catch (err) { postMessage({ status: -1, err: String(err) }); }
};"#;

const ROGUE_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><body><pre id="l">rogue origin page</pre><script>
// Loaded in window "main" but from a foreign origin: scripted reads of
// main's blobs must be denied by the Origin gate even though the
// webview label still says "main".
const B = (navigator.userAgent.includes("Windows") || navigator.userAgent.includes("Android"))
  ? "http://pylair.localhost" : "pylair://localhost";
const log = (m) => document.getElementById("l").textContent += "\n" + m;
(async () => {
  for (const t of [
    ["fetch", () => fetch(`${B}/b/img_main`).then(r => r.status)],
    ["fetch-range", () => fetch(`${B}/b/img_main`, { headers: { Range: "bytes=0-9" } }).then(r => r.status)],
    ["xhr", () => new Promise((res) => { const x = new XMLHttpRequest(); x.open("GET", `${B}/b/img_main`); x.onload = () => res(x.status); x.onerror = () => res("err"); x.send(); })],
    ["worker", () => new Promise((res) => { const w = new Worker("worker.js"); w.onmessage = (e) => res(e.data.status); w.onerror = () => res("err"); w.postMessage(`${B}/b/img_main`); })],
  ]) {
    try { log(t[0] + " -> " + await t[1]()); } catch (e) { log(t[0] + " -> " + e); }
  }
  // Element loads carry no Origin and no readback channel — served,
  // pixels only (documented in 0010 §2).
  const i = new Image(); i.onload = () => log("img loaded"); i.onerror = () => log("img denied"); i.src = `${B}/b/img_main`;
  setTimeout(() => fetch(`${B}/b/zzdone`).finally(() => {}), 1500);
})();
</script></body>"#;

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
        .invoke_handler(tauri::generate_handler![b0_blob_ids, b0_scheme_log, b0_report, b0_mutate])
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
            // ~21 MB — past the range cap so every media request must be
            // served as a short 206.
            fs::write(&wav, wav_bytes(120.0)).unwrap();
            // A "from_path"-style source: the registration records
            // size+mtime; b0_mutate rewrites it in place → next read 410.
            let mut_src = out_dir.join("mut_source.bin");
            fs::write(&mut_src, vec![7u8; 4096]).unwrap();

            let mut blobs = HashMap::new();
            let insert = |blobs: &mut HashMap<String, Blob>, id: &str, path: PathBuf, mime: &'static str, owner: &'static str| {
                let (reg_size, reg_mtime) = stat(&path).unwrap();
                blobs.insert(id.into(), Blob { path, mime, owner, reg_size, reg_mtime });
            };
            insert(&mut blobs, "img_main", png, "image/png", "main");
            insert(&mut blobs, "audio_main", wav, "audio/wav", "main");
            insert(&mut blobs, "img_other", png_b, "image/png", "other");
            insert(&mut blobs, "mut_main", mut_src, "application/octet-stream", "main");

            app.manage(AppState {
                blobs: Mutex::new(blobs),
                scheme_log: Mutex::new(Vec::new()),
                reports: Mutex::new(Vec::new()),
                out_dir,
                rogue_origin: rogue_server(),
                done: AtomicBool::new(false),
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
