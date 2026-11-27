//! `PYLAIR_BINARIES` resolution (decision 0014 §2/§3): the host builds a
//! name -> absolute path map of the app's declared external binaries and
//! hands it to the runtime in the spawn environment, the same channel as
//! `PYLAIR_APP_DATA_DIR` (0009). The source of truth is a build-generated
//! manifest, never host-side guesswork:
//!
//! ```json
//! {"version": 1, "binaries": {"<name>": {
//!   "shape": "file" | "unit",
//!   "root": "exe_dir" | "payload",
//!   "path": "<relative path with forward slashes>"}}}
//! ```
//!
//! `shape` describes what the binary IS (a single externalBin sidecar
//! file, or one executable inside a unit tree); `root` is the resolution
//! key: `exe_dir` resolves relative to the directory holding the main
//! executable (Contents/MacOS on macOS, the install dir on Windows, the
//! AppImage mount on Linux), `payload` relative to the binaries payload
//! dir (`<resource_dir>/binaries/` in release). In dev both shapes
//! resolve against the dev binaries dir itself - one manifest format in
//! both modes, one resolution mechanism.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The env var carrying the compact-JSON name -> absolute path map.
pub const BINARIES_ENV: &str = "PYLAIR_BINARIES";
/// Dev-only env var naming the directory holding dev binaries plus the
/// manifest. Never honoured in release builds.
pub const BINARIES_DIR_ENV: &str = "PYLAIR_BINARIES_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    File,
    Unit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    ExeDir,
    Payload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub shape: Shape,
    pub root: Root,
    /// Relative path with forward slashes, validated at parse time.
    pub path: String,
}

/// Why a binaries.json manifest is rejected. Display strings name the
/// binary entry where one is responsible.
#[derive(Debug)]
#[non_exhaustive]
pub enum ManifestError {
    /// The file would not parse as JSON, or the top level is not an object.
    Json(String),
    /// `version` is missing or anything other than 1.
    Version(String),
    /// One entry is malformed; carries the binary name and the reason.
    Entry { name: String, reason: String },
    /// The manifest file exists but could not be read.
    Io(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "binaries.json is not valid JSON: {e}"),
            Self::Version(v) => write!(
                f,
                "binaries.json version {v} is not supported (this build reads version 1)"
            ),
            Self::Entry { name, reason } => write!(f, "binaries.json entry {name:?}: {reason}"),
            Self::Io(e) => write!(f, "binaries.json could not be read: {e}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Validate the `path` member: relative, forward slashes only, no `..`
/// segments, no drive-letter or UNC-absolute forms. Returns the reason for
/// rejection, or None when valid.
fn path_rejection(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("path is empty");
    }
    if path.contains('\\') {
        return Some("path must use forward slashes (found a backslash)");
    }
    if path.starts_with('/') {
        return Some("path must be relative (found a leading slash)");
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Some("path must be relative (found a drive letter)");
    }
    for seg in path.split('/') {
        if seg == ".." {
            return Some("path must not contain .. segments");
        }
        if seg.is_empty() {
            return Some("path contains an empty segment");
        }
    }
    None
}

/// Parse and validate a binaries.json document. Unknown versions, absolute
/// paths and `..` segments are hard errors naming the entry - a bad
/// manifest is a build bug and must never resolve a partial truth
/// silently.
pub fn parse_manifest(json: &str) -> Result<BTreeMap<String, Entry>, ManifestError> {
    let doc: serde_json::Value =
        serde_json::from_str(json).map_err(|e| ManifestError::Json(e.to_string()))?;
    let obj = doc
        .as_object()
        .ok_or_else(|| ManifestError::Json("top level is not an object".into()))?;
    let version = obj.get("version");
    if version != Some(&serde_json::Value::from(1)) {
        return Err(ManifestError::Version(
            version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<missing>".into()),
        ));
    }
    let bins = obj
        .get("binaries")
        .and_then(|b| b.as_object())
        .ok_or_else(|| ManifestError::Json("missing object member \"binaries\"".into()))?;
    let mut out = BTreeMap::new();
    for (name, v) in bins {
        let bad = |reason: &str| ManifestError::Entry {
            name: name.clone(),
            reason: reason.into(),
        };
        let e = v.as_object().ok_or_else(|| bad("entry is not an object"))?;
        let shape = match e.get("shape").and_then(|s| s.as_str()) {
            Some("file") => Shape::File,
            Some("unit") => Shape::Unit,
            other => {
                return Err(bad(&format!(
                    "shape must be \"file\" or \"unit\" (found {})",
                    other
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|| "<missing>".into())
                )))
            }
        };
        let root = match e.get("root").and_then(|s| s.as_str()) {
            Some("exe_dir") => Root::ExeDir,
            Some("payload") => Root::Payload,
            other => {
                return Err(bad(&format!(
                    "root must be \"exe_dir\" or \"payload\" (found {})",
                    other
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|| "<missing>".into())
                )))
            }
        };
        let path = e
            .get("path")
            .and_then(|s| s.as_str())
            .ok_or_else(|| bad("path must be a string"))?;
        if let Some(reason) = path_rejection(path) {
            return Err(bad(reason));
        }
        out.insert(
            name.clone(),
            Entry {
                shape,
                root,
                path: path.to_string(),
            },
        );
    }
    Ok(out)
}

/// Read `<dir>/binaries.json`. `Ok(None)` means the file is absent: the app
/// declares no binaries and no env var is set - an absent manifest is not
/// an error anywhere.
pub fn load_manifest(dir: &Path) -> Result<Option<BTreeMap<String, Entry>>, ManifestError> {
    let file = dir.join("binaries.json");
    let json = match std::fs::read_to_string(&file) {
        Ok(j) => j,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ManifestError::Io(format!("{}: {e}", file.display()))),
    };
    parse_manifest(&json).map(Some)
}

/// The shared resolver: only entries whose file exists go into the map;
/// each missing one is logged once by name through `log` and simply
/// omitted - never an error (a binary that failed to stage must not take
/// the app's other binaries down with it, and Python's `pylair.binary()`
/// error for a missing name is the honest surface).
fn resolve_entries<'a>(
    entries: &BTreeMap<String, Entry>,
    base_for: impl Fn(&Entry) -> &'a Path,
    log: &mut impl FnMut(&str),
) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for (name, entry) in entries {
        let resolved = base_for(entry).join(Path::new(&entry.path));
        if resolved.exists() {
            out.insert(name.clone(), resolved);
        } else {
            log(name);
        }
    }
    out
}

/// Release resolution (0014 §2): each entry resolves by its `root` -
/// `exe_dir` against the directory holding the main executable, `payload`
/// against `<resource_dir>/binaries/`. `exe_dir` is `None` only when
/// `current_exe()` itself failed, in which case exe-rooted entries drop
/// with the same missing-file log.
pub fn resolve_release_entries(
    entries: &BTreeMap<String, Entry>,
    exe_dir: Option<&Path>,
    payload_dir: &Path,
    mut log: impl FnMut(&str),
) -> BTreeMap<String, PathBuf> {
    resolve_entries(
        entries,
        |e| match (e.root, exe_dir) {
            (Root::Payload, _) => payload_dir,
            (Root::ExeDir, Some(d)) => d,
            // A vanished exe path must never resolve against the payload:
            // join onto a guaranteed-nonexistent base so the entry drops.
            (Root::ExeDir, None) => Path::new("__pylair_no_exe_dir__"),
        },
        &mut log,
    )
}

/// Dev resolution (0014 §3): both shapes resolve against the dev binaries
/// dir itself - the CLI lays dev binaries out under `.pylair/binaries/`
/// and the same manifest format drives both modes.
pub fn resolve_dev_entries(
    entries: &BTreeMap<String, Entry>,
    dir: &Path,
    mut log: impl FnMut(&str),
) -> BTreeMap<String, PathBuf> {
    resolve_entries(entries, |_| dir, &mut log)
}

/// Serialize the map as compact JSON for the env var. BTreeMap ordering
/// makes the output deterministic, which tests and logs both appreciate.
pub fn map_to_json(map: &BTreeMap<String, PathBuf>) -> String {
    let strings: BTreeMap<String, String> = map
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
        .collect();
    serde_json::to_string(&strings).unwrap_or_else(|_| "{}".into())
}
