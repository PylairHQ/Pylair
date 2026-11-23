//! Binary blobs by URL (docs/decisions/0010-blob-urls.md): a per-generation
//! directory of host-owned files, a registry binding each blob to the
//! window that owns the task that produced it, and a `pylair-blob` custom
//! URI scheme serving those files to that window only.
//!
//! The runtime learns the per-generation dir from `PYLAIR_BLOB_DIR` (env
//! presence is the capability bit — same pattern as `PYLAIR_APP_DATA_DIR`).
//! The wire carries only `{"$blob": {"id","mime","size"}}`; bytes never
//! travel a frame. `blob.register` arrives ahead of the frame carrying
//! the tag (FIFO — registration always precedes use); `blob.release`
//! drops a handle early.
//!
//! Serving rules, in check order (decision 0010 §1-§2):
//! 1. `Origin` present → must equal an app origin, else 403 *before* any
//!    lookup. Absent `Origin` = an element/no-cors load — pixels only, no
//!    scripted readback, so the label check is the guard.
//! 2. Owner label — `ctx.webview_label()` must equal the blob's owner,
//!    which came from `TaskMeta.owner`, never from Python or JS input.
//! 3. Lookup → 404. Live re-stat of registered `(size, mtime)` → 410 on
//!    a `from_path` source rewritten in place.
//! 4. No `Range` → `PYLAIR_BLOB_MAX_FULL_BODY` cap → 416 + `Accept-Ranges`
//!    + a body telling the caller to issue ranges. `Range` → hand-rolled
//!    206 truncated to `PYLAIR_BLOB_MAX_RANGE_BYTES` (a short 206 is
//!    legal; media stacks re-request).
//!
//! Every response — errors included — carries `Accept-Ranges: bytes`,
//! `Access-Control-Allow-Origin` echoing the app origin exactly with
//! `Vary: Origin` (never `*`), `Cache-Control: no-store` and
//! `X-Content-Type-Options: nosniff` (a foreign page's element load could
//! otherwise pull a text/JSON blob in via script/link; nosniff makes the
//! engine refuse to execute it).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::Value;
use tauri::http::{self, StatusCode};
use tauri::{Manager, Runtime};

/// The URI scheme blob URLs are served from. Not bare `pylair` —
/// `pylair://state` etc. are event *topics*, and a distinct scheme keeps
/// template CSP readable (decision 0010 §1).
pub const SCHEME: &str = "pylair-blob";

/// Env the runtime reads for its blob dir (capability bit, decision 0010
/// §3). Set per spawn to `<blob_root>/gen-<n>` by the supervisor.
pub const BLOB_DIR_ENV: &str = "PYLAIR_BLOB_DIR";

/// Where `<app_cache>`-relative blob roots live: `<cache>/pylair-blobs/gen-N`.
const BLOB_ROOT_NAME: &str = "pylair-blobs";

/// Default `application/octet-stream` when a `blob.register` omits `mime`.
const DEFAULT_MIME: &str = "application/octet-stream";

/// The five blob knobs (decision 0010 §1/§6). Resolved from `PYLAIR_*` env
/// once, like `Limits`; `validated` clamps nonsense and reports warnings.
#[derive(Debug, Clone)]
pub struct BlobLimits {
    /// Max registered bytes for one blob. `PYLAIR_MAX_BLOB_BYTES`, 512 MiB.
    pub max_blob_bytes: u64,
    /// Max live bytes across a generation. `PYLAIR_MAX_BLOB_TOTAL`, 2 GiB.
    pub max_blob_total: u64,
    /// Max live handles per window. `PYLAIR_MAX_BLOBS_PER_WINDOW`, 256.
    pub max_blobs_per_window: usize,
    /// Max bytes a full-body (non-Range) response may hold in the host.
    /// `PYLAIR_BLOB_MAX_FULL_BODY`, 64 MiB.
    pub max_full_body: u64,
    /// Max bytes one range response may serve; longer asks get a short
    /// 206. `PYLAIR_BLOB_MAX_RANGE_BYTES`, 8 MiB.
    pub max_range_bytes: u64,
}

impl BlobLimits {
    pub fn from_env() -> Self {
        let env_u64 = |key: &str, default: u64| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        Self {
            max_blob_bytes: env_u64("PYLAIR_MAX_BLOB_BYTES", 512 * 1024 * 1024),
            max_blob_total: env_u64("PYLAIR_MAX_BLOB_TOTAL", 2 * 1024 * 1024 * 1024),
            max_blobs_per_window: std::env::var("PYLAIR_MAX_BLOBS_PER_WINDOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
            max_full_body: env_u64("PYLAIR_BLOB_MAX_FULL_BODY", 64 * 1024 * 1024),
            max_range_bytes: env_u64("PYLAIR_BLOB_MAX_RANGE_BYTES", 8 * 1024 * 1024),
        }
    }

    /// Clamp zero/absurd values; warnings mirror `Limits::validated`.
    fn validated(mut self) -> (Self, Vec<String>) {
        let mut warns = Vec::new();
        let mut clamp = |v: &mut u64, name: &str, lo: u64, hi: u64| {
            if *v < lo || *v > hi {
                warns.push(format!("{name}={v} outside {lo}..={hi} — clamped"));
                *v = (*v).clamp(lo, hi);
            }
        };
        clamp(&mut self.max_blob_bytes, "max_blob_bytes", 1, 1 << 40);
        clamp(&mut self.max_blob_total, "max_blob_total", 1, 1 << 40);
        clamp(&mut self.max_full_body, "max_full_body", 1, 1 << 40);
        clamp(&mut self.max_range_bytes, "max_range_bytes", 1, 1 << 40);
        if self.max_blobs_per_window < 1 {
            warns.push("max_blobs_per_window=0 — raised to 1".into());
            self.max_blobs_per_window = 1;
        }
        if self.max_blob_total < self.max_blob_bytes {
            warns.push(format!(
                "max_blob_total={} below per-blob cap {} — raised",
                self.max_blob_total, self.max_blob_bytes
            ));
            self.max_blob_total = self.max_blob_bytes;
        }
        (self, warns)
    }
}

/// One registered blob. `path` is canonicalized at registration and must
/// live inside the generation's blob dir — a `blob.register` naming a path
/// outside it is rejected, so the scheme can never serve arbitrary host
/// files no matter what the backend claims (defense in depth; Python is
/// trusted, but containment is free).
pub struct BlobEntry {
    pub path: PathBuf,
    pub mime: String,
    pub owner: String,
    /// Recorded at registration and re-stat-compared per request — an
    /// in-place rewrite of a `from_path` source makes them diverge → 410.
    pub size: u64,
    pub mtime: SystemTime,
}

/// Why a `blob.register` was refused. Surfaced on `pylair://stderr` — the
/// notification has no reply channel, so rejection is observability, not
/// an error back to Python (the `$blob` it described will 404 at fetch).
#[derive(Debug, PartialEq)]
pub enum Reject {
    /// `data.limit`-style name of the cap that bound (decision 0010 §6).
    Limit(&'static str),
    /// Malformed params, owner-less task, path outside the blob dir, or a
    /// duplicate/unsafe id.
    Bad(&'static str),
    /// The blob exists but belongs to another window.
    Foreign,
}

/// The shared registry: `blob.register`/`blob.release` notifications,
/// window-destroy drops and the scheme handler all go through it.
pub struct BlobRegistry {
    blobs: HashMap<String, BlobEntry>,
    /// Live registered bytes this generation (per-generation admission).
    gen_bytes: u64,
    /// Canonical generation dir for the path-containment check; `None`
    /// when `PYLAIR_BLOB_DIR` could not be resolved — registrations are
    /// then refused and `$blob` values the runtime still emits 404.
    dir: Option<PathBuf>,
    pub limits: BlobLimits,
}

/// Shared with `Handle` like `fair`/`health` (spawn() wires it).
pub type BlobRegistryRef = Arc<Mutex<BlobRegistry>>;

impl BlobRegistry {
    /// New registry; warnings for clamped limits are pushed to events by
    /// the caller (same pattern as `Limits::validated` in `spawn()`).
    pub fn new(limits: BlobLimits) -> (BlobRegistryRef, Vec<String>) {
        let (limits, warns) = limits.validated();
        (
            Arc::new(Mutex::new(Self {
                blobs: HashMap::new(),
                gen_bytes: 0,
                dir: None,
                limits,
            })),
            warns,
        )
    }

    /// A new generation owns a fresh dir: clear the old map and set the
    /// canonical containment root (already canonicalized by the caller).
    /// `None` means the capability is off for this generation.
    pub fn set_generation(&mut self, dir: Option<PathBuf>) {
        self.blobs.clear();
        self.gen_bytes = 0;
        self.dir = dir;
    }

    /// Register a `blob.register` notification's params against `owner`
    /// (resolved by the caller from `TaskMeta.owner` — never trusted from
    /// the payload). Records host-stat'd `(size, mtime)`, not the backend's
    /// claims.
    pub fn register(&mut self, params: &Value, owner: &str) -> Result<(), Reject> {
        let id = params["id"].as_str().unwrap_or("");
        if id.is_empty()
            || id.len() > 256
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Reject::Bad("bad blob id"));
        }
        if self.blobs.contains_key(id) {
            return Err(Reject::Bad("duplicate blob id"));
        }
        let Some(dir) = &self.dir else {
            return Err(Reject::Bad("no blob dir (PYLAIR_BLOB_DIR unset)"));
        };
        let path_raw = params["path"].as_str().unwrap_or("");
        let path = Path::new(path_raw)
            .canonicalize()
            .map_err(|_| Reject::Bad("path does not resolve"))?;
        if !path.starts_with(dir) {
            return Err(Reject::Bad("path outside the blob dir"));
        }
        let mime = params["mime"].as_str().unwrap_or(DEFAULT_MIME);
        if mime.is_empty()
            || !mime
                .bytes()
                .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'\t')
        {
            return Err(Reject::Bad("mime is not a valid header value"));
        }
        let meta = std::fs::metadata(&path).map_err(|_| Reject::Bad("path does not stat"))?;
        let (size, mtime) = (
            meta.len(),
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );

        if size > self.limits.max_blob_bytes {
            return Err(Reject::Limit("blob_bytes"));
        }
        if self.blobs.values().filter(|b| b.owner == owner).count()
            >= self.limits.max_blobs_per_window
        {
            return Err(Reject::Limit("blob_handles"));
        }
        if self.gen_bytes.saturating_add(size) > self.limits.max_blob_total {
            return Err(Reject::Limit("blob_total"));
        }
        // Per-window share of the generation pool (decision 0010 §6): the
        // fairness formula applied to blob bytes — every live owner keeps a
        // `total/live` minimum and may borrow the rest, so one window can't
        // drain the 2 GiB pool. `live` = owners currently holding blobs.
        let live = self
            .blobs
            .values()
            .map(|b| b.owner.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
            .max(1) as u64;
        let reserve = (self.limits.max_blob_total / live)
            .max(1)
            .min(self.limits.max_blob_total);
        let others_bytes: u64 = self
            .blobs
            .values()
            .filter(|b| b.owner != owner)
            .map(|b| b.size)
            .sum();
        let ceiling = reserve.max(self.limits.max_blob_total.saturating_sub(others_bytes));
        let owner_bytes: u64 = self
            .blobs
            .values()
            .filter(|b| b.owner == owner)
            .map(|b| b.size)
            .sum();
        if owner_bytes.saturating_add(size) > ceiling {
            return Err(Reject::Limit("blob_bytes_share"));
        }

        self.gen_bytes += size;
        self.blobs.insert(
            id.to_string(),
            BlobEntry {
                path,
                mime: mime.to_string(),
                owner: owner.to_string(),
                size,
                mtime,
            },
        );
        Ok(())
    }

    /// Drop one handle and unlink its file (best-effort — the registry
    /// entry dies regardless). Idempotent: unknown ids are a no-op.
    /// Returns false when nothing was released.
    pub fn release(&mut self, id: &str) -> bool {
        let Some(entry) = self.blobs.remove(id) else {
            return false;
        };
        self.gen_bytes = self.gen_bytes.saturating_sub(entry.size);
        let _ = std::fs::remove_file(&entry.path);
        true
    }

    /// `python_release_blob` and the release path for a known id: the
    /// caller must own it (owner-checked like every command, §4). Returns
    /// Ok on missing ids (idempotent), Err on a foreign window's blob.
    pub fn release_owned(&mut self, id: &str, owner: &str) -> Result<bool, Reject> {
        let owner_ok = match self.blobs.get(id) {
            None => return Ok(false),
            Some(b) => b.owner == owner,
        };
        if !owner_ok {
            return Err(Reject::Foreign);
        }
        Ok(self.release(id))
    }

    /// A destroyed window loses every blob it owned (decision 0010 §6:
    /// window close frees the window's live blobs).
    pub fn drop_owner(&mut self, owner: &str) {
        let ids: Vec<String> = self
            .blobs
            .iter()
            .filter(|(_, b)| b.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.release(&id);
        }
    }

    /// Test accessor — whether `id` is registered.
    #[doc(hidden)]
    pub fn has(&self, id: &str) -> bool {
        self.blobs.contains_key(id)
    }

    /// Test accessor — live handle count.
    #[doc(hidden)]
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Test accessor — no live handles.
    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }

    /// Generation end: the registry empties; the caller deletes the dir.
    pub fn end_generation(&mut self) {
        self.blobs.clear();
        self.gen_bytes = 0;
        self.dir = None;
    }

    /// Resolve `<app_cache>/pylair-blobs` as the blob root: resolve + mkdir
    /// is best-effort exactly like `PYLAIR_APP_DATA_DIR` (decision 0009 §2)
    /// — a failure leaves the capability off and logs, never crashes.
    /// Per-generation `gen-N` dirs are created under it at each spawn.
    pub fn resolve_root(app_cache_dir: Result<PathBuf, tauri::Error>) -> Option<PathBuf> {
        let root = match app_cache_dir {
            Ok(d) => dunce::simplified(&d).to_path_buf().join(BLOB_ROOT_NAME),
            Err(e) => {
                eprintln!("pylair: app_cache_dir() failed ({e}); PYLAIR_BLOB_DIR left unset");
                return None;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&root) {
            eprintln!(
                "pylair: could not create blob root {} ({e}); PYLAIR_BLOB_DIR left unset",
                root.display()
            );
            return None;
        }
        Some(root)
    }

    /// Startup sweep (decision 0010 §6): `gen-*` dirs under the root are
    /// crash residue — no generation owns them yet. Removes and counts.
    pub fn sweep(root: &Path) -> usize {
        let mut removed = 0;
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("gen-")
                    && std::fs::remove_dir_all(e.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// The generation dir + its canonical form for `<root>/gen-<n>`.
    /// `Ok(None)` when the root is unset; `Err` on a mkdir failure (the
    /// caller then leaves `PYLAIR_BLOB_DIR` unset for that generation).
    pub fn generation_dir(root: Option<&Path>, generation: u64) -> Option<(PathBuf, PathBuf)> {
        let dir = root?.join(format!("gen-{generation}"));
        std::fs::create_dir_all(&dir).ok()?;
        let canon = dir.canonicalize().unwrap_or(dir.clone());
        Some((dir, canon))
    }

    /// Serve one scheme request. Free of Tauri types beyond `http` so the
    /// whole decision tree is unit-testable; the adapter in `builder.rs`
    /// resolves `allowed` origins and the requesting webview's label.
    pub fn serve(
        &mut self,
        label: &str,
        allowed_origins: &[String],
        req: &http::Request<Vec<u8>>,
    ) -> http::Response<Vec<u8>> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let origin = req
            .headers()
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let acao = origin
            .clone()
            .filter(|o| allowed_origins.contains(o))
            .or_else(|| allowed_origins.first().cloned())
            .unwrap_or_default();

        let respond = |status: StatusCode, mime: &str, body: Vec<u8>, extra: &[(&str, String)]| {
            let mut b = http::Response::builder()
                .status(status)
                .header(http::header::CONTENT_TYPE, mime)
                .header(http::header::CONTENT_LENGTH, body.len().to_string())
                .header(http::header::ACCEPT_RANGES, "bytes")
                .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, &acao)
                .header(http::header::VARY, "Origin")
                .header(http::header::CACHE_CONTROL, "no-store")
                .header(http::header::X_CONTENT_TYPE_OPTIONS, "nosniff");
            for (k, v) in extra {
                b = b.header(*k, v);
            }
            b.body(body).unwrap()
        };

        // 1. Origin gate — before anything touches the map (§2).
        if let Some(o) = &origin {
            if !allowed_origins.contains(o) {
                return respond(
                    StatusCode::FORBIDDEN,
                    "text/plain; charset=utf-8",
                    b"foreign origin".to_vec(),
                    &[],
                );
            }
        }
        if method == http::Method::OPTIONS {
            return respond(
                StatusCode::NO_CONTENT,
                "text/plain; charset=utf-8",
                Vec::new(),
                &[(
                    http::header::ACCESS_CONTROL_ALLOW_HEADERS.as_str(),
                    "Range".into(),
                )],
            );
        }
        if method != http::Method::GET && method != http::Method::HEAD {
            return respond(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain; charset=utf-8",
                b"method not allowed".to_vec(),
                &[(http::header::ALLOW.as_str(), "GET, HEAD, OPTIONS".into())],
            );
        }

        // 2. The webview only ever names an id — never a path.
        let id = match path.strip_prefix("/b/") {
            Some(id)
                if !id.is_empty()
                    && id.len() <= 256
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
            {
                id.to_string()
            }
            _ => {
                return respond(
                    StatusCode::BAD_REQUEST,
                    "text/plain; charset=utf-8",
                    b"bad blob path".to_vec(),
                    &[],
                )
            }
        };

        // 3. Lookup → owner check (owner came from TaskMeta.owner at
        //    registration — never from the request).
        let Some(entry) = self.blobs.get(&id) else {
            return respond(
                StatusCode::NOT_FOUND,
                "text/plain; charset=utf-8",
                b"no such blob".to_vec(),
                &[],
            );
        };
        if entry.owner != label {
            return respond(
                StatusCode::FORBIDDEN,
                "text/plain; charset=utf-8",
                b"blob owned by another window".to_vec(),
                &[],
            );
        }
        let (path, mime, reg_size, reg_mtime) = (
            entry.path.clone(),
            entry.mime.clone(),
            entry.size,
            entry.mtime,
        );
        let total = reg_size;

        // 4. Registered-metadata verify: a `from_path` source rewritten in
        //    place changes size+mtime under the same inode → 410, never
        //    silently-stale bytes (decision 0010 §4).
        let stat_ok = std::fs::metadata(&path)
            .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
            .is_ok_and(|cur| cur == (reg_size, reg_mtime));
        if !stat_ok {
            return respond(
                StatusCode::GONE,
                "text/plain; charset=utf-8",
                b"blob source changed".to_vec(),
                &[],
            );
        }

        // 5. Serve. Range absent → full body under the cap only; a larger
        //        ask is refused with `Accept-Ranges` pointing the way.
        let range = req
            .headers()
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok());
        let Some(range) = range else {
            if total > self.limits.max_full_body {
                return respond(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "text/plain; charset=utf-8",
                    format!(
                        "full-body read refused: {total} bytes > {} cap; issue a Range request",
                        self.limits.max_full_body
                    )
                    .into_bytes(),
                    &[("Content-Range", format!("bytes */{total}"))],
                );
            }
            let body = if method == http::Method::HEAD {
                Vec::new()
            } else {
                match std::fs::read(&path) {
                    Ok(d) if d.len() as u64 == total => d,
                    _ => {
                        return respond(
                            StatusCode::GONE,
                            "text/plain; charset=utf-8",
                            b"blob source changed".to_vec(),
                            &[],
                        )
                    }
                }
            };
            let mut r = respond(StatusCode::OK, &mime, body, &[]);
            if method == http::Method::HEAD {
                // HEAD answers headers for the full object.
                r.headers_mut().insert(
                    http::header::CONTENT_LENGTH,
                    total.to_string().parse().unwrap(),
                );
            }
            return r;
        };

        let (start, end) = match parse_range(range, total) {
            Some(se) => se,
            None => {
                return respond(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "text/plain; charset=utf-8",
                    b"range unsatisfiable".to_vec(),
                    &[("Content-Range", format!("bytes */{total}"))],
                )
            }
        };
        // Truncate to the range cap: a short 206 is legal HTTP (§1) —
        // media stacks and blob.stream() re-request the remainder.
        let end = end.min(start + self.limits.max_range_bytes - 1);
        let body = if method == http::Method::HEAD {
            Vec::new()
        } else {
            match std::fs::read(&path) {
                Ok(d) if d.len() as u64 == total => d[start as usize..=end as usize].to_vec(),
                _ => {
                    return respond(
                        StatusCode::GONE,
                        "text/plain; charset=utf-8",
                        b"blob source changed".to_vec(),
                        &[],
                    )
                }
            }
        };
        let mut r = respond(
            StatusCode::PARTIAL_CONTENT,
            &mime,
            body,
            &[("Content-Range", format!("bytes {start}-{end}/{total}"))],
        );
        if method == http::Method::HEAD {
            r.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                (end - start + 1).to_string().parse().unwrap(),
            );
        }
        r
    }
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` — one range only; anything else
/// (multi-range, bad numbers, other units, start ≥ size) is unsatisfiable.
fn parse_range(range: &str, total: u64) -> Option<(u64, u64)> {
    let spec = range.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    if a.is_empty() {
        // Suffix form: the last n bytes.
        let n: u64 = b.parse().ok()?;
        if n == 0 || n > total {
            // n > total is legal (whole object); n == 0 is invalid.
            return if n > total {
                Some((0, total - 1))
            } else {
                None
            };
        }
        Some((total - n, total - 1))
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            total.saturating_sub(1)
        } else {
            b.parse::<u64>().ok()?
        };
        if start >= total || start > end {
            return None;
        }
        Some((start, end.min(total - 1)))
    }
}

/// The origins a scheme request may legitimately carry for `label`'s
/// window (decision 0010 §2). Non-Windows is `tauri://localhost`;
/// Windows/Android is `http(s)://tauri.localhost` — `https` only when that
/// window's config sets `useHttpsScheme`. Dev mode adds the dev-server
/// origin (`tauri dev` pages are `http://localhost:<port>`-origined).
/// An origin not in this set can only mean a foreign page — the
/// `http://127.0.0.1` page the spike navigated the owner window to.
fn app_origins<R: Runtime>(app: &tauri::AppHandle<R>, label: &str) -> Vec<String> {
    let mut v = Vec::with_capacity(2);
    if tauri::is_dev() {
        if let Some(u) = app.config().build.dev_url.as_ref() {
            v.push(origin_of(u.as_ref()));
        }
    }
    if cfg!(any(windows, target_os = "android")) {
        let https = app
            .config()
            .app
            .windows
            .iter()
            .find(|w| w.label == label)
            .is_some_and(|w| w.use_https_scheme);
        v.push(if https {
            "https://tauri.localhost".into()
        } else {
            "http://tauri.localhost".into()
        });
    } else {
        v.push("tauri://localhost".into());
    }
    v
}

/// `scheme://authority` of an absolute URL string — the Origin header's
/// exact form for http(s)/custom schemes (no path, no trailing slash).
fn origin_of(url: &str) -> String {
    match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            match rest.find('/') {
                Some(j) => url[..i + 3 + j].to_string(),
                None => url.trim_end_matches('/').to_string(),
            }
        }
        None => url.trim_end_matches('/').to_string(),
    }
}

/// The scheme-handler adapter: resolves the requesting webview's label
/// and the app origins it may claim, then hands the request to the
/// registry's decision tree. Runs on a thread per request (the same
/// shape as the spike and `asset://`) — bodies are `Vec<u8>` because
/// Tauri's responder takes `Cow<'static, [u8]>`; the caps in `BlobLimits`
/// bound what that costs.
pub fn serve_request<R: Runtime>(
    app: &tauri::AppHandle<R>,
    label: &str,
    req: &http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    let origins = app_origins(app, label);
    if let Some(rt) = app.try_state::<crate::registry::PyRuntime>() {
        let arc = rt.0.blobs();
        let mut reg = arc.lock().unwrap();
        reg.serve(label, &origins, req)
    } else {
        // No supervisor yet — nothing can be registered; the gates still
        // answer consistently (a 404 carries the same hardened headers).
        let (arc, _) = BlobRegistry::new(BlobLimits::from_env());
        let res = arc.lock().unwrap().serve(label, &origins, req);
        res
    }
}
