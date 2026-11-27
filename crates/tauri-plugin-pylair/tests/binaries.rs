//! `PYLAIR_BINARIES` resolution (decision 0014 §2/§3) in the release_spec
//! style: temp resources trees and parameterized env closures, no real
//! binaries and no process-environment mutation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tauri_plugin_pylair::binaries::{
    load_manifest, map_to_json, parse_manifest, resolve_dev_entries, resolve_release_entries, Root,
    Shape,
};
use tauri_plugin_pylair::{
    release_spawn_spec, set_binaries_debug_env, strip_inherited_binaries, SpawnSpec,
};

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pylair-binspec-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn manifest(entries: &str) -> String {
    format!("{{\"version\":1,\"binaries\":{{{entries}}}}}")
}

fn file_entry(path: &str) -> String {
    format!("\"shape\":\"file\",\"root\":\"exe_dir\",\"path\":\"{path}\"")
}

fn unit_entry(path: &str) -> String {
    format!("\"shape\":\"unit\",\"root\":\"payload\",\"path\":\"{path}\"")
}

fn spec_map(spec: &SpawnSpec) -> Option<BTreeMap<String, String>> {
    let (_, json) = spec.env.iter().find(|(k, _)| k == "PYLAIR_BINARIES")?;
    Some(serde_json::from_str(json).expect("PYLAIR_BINARIES is JSON"))
}

#[test]
fn release_computes_map_from_resources_tree() {
    let res = tmp("release");
    write(&res.join("python/python.exe"), b"x");
    write(&res.join("site-packages/.keep"), b"");
    write(&res.join("backend/__init__.py"), b"");
    write(&res.join("binaries/llama/llama-server.exe"), b"fake-binary");
    write(
        &res.join("binaries/binaries.json"),
        manifest(&format!(
            "\"llama-server\":{{{}}}",
            unit_entry("llama/llama-server.exe")
        ))
        .as_bytes(),
    );

    let spec = release_spawn_spec(&res, "backend.app", "backend");
    let map = spec_map(&spec).expect("release spec sets PYLAIR_BINARIES");
    assert_eq!(map.len(), 1);
    let resolved = PathBuf::from(&map["llama-server"]);
    assert!(
        resolved.ends_with(Path::new("binaries").join("llama").join("llama-server.exe")),
        "unit resolves under <resources>/binaries, got {}",
        resolved.display()
    );
    assert!(resolved.exists());
    let _ = std::fs::remove_dir_all(&res);
}

#[test]
fn release_exe_dir_entries_resolve_against_exe_dir() {
    // File-shaped sidecars resolve against the directory holding the main
    // executable, not the resources payload. Parameterized here because a
    // test cannot control current_exe().
    let dir = tmp("exebase");
    let exe_dir = dir.join("exe");
    let payload = dir.join("payload");
    write(&exe_dir.join("ffmpeg-x86_64-pc-windows-msvc.exe"), b"fake");
    write(&payload.join("llama/llama-server.exe"), b"fake");
    let entries = parse_manifest(&manifest(&format!(
        "\"ffmpeg\":{{{}}},\"llama-server\":{{{}}}",
        file_entry("ffmpeg-x86_64-pc-windows-msvc.exe"),
        unit_entry("llama/llama-server.exe")
    )))
    .unwrap();
    let mut missing = vec![];
    let map = resolve_release_entries(&entries, Some(&exe_dir), &payload, |n| {
        missing.push(n.to_string())
    });
    assert!(missing.is_empty(), "nothing missing: {missing:?}");
    assert_eq!(
        map["ffmpeg"],
        exe_dir.join("ffmpeg-x86_64-pc-windows-msvc.exe")
    );
    assert_eq!(map["llama-server"], payload.join("llama/llama-server.exe"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_is_omitted_not_an_error() {
    let dir = tmp("missing");
    let payload = dir.join("payload");
    write(&payload.join("present/tool.exe"), b"fake");
    write(
        &payload.join("binaries.json"),
        manifest(&format!(
            "\"present\":{{{}}},\"gone\":{{{}}}",
            unit_entry("present/tool.exe"),
            unit_entry("gone/tool.exe")
        ))
        .as_bytes(),
    );
    let entries = load_manifest(&payload).unwrap().unwrap();
    let mut missing = vec![];
    let map = resolve_release_entries(&entries, None, &payload, |n| missing.push(n.to_string()));
    assert_eq!(map.len(), 1);
    assert!(map.contains_key("present"));
    assert_eq!(missing, vec!["gone"], "the missing name is logged once");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn absent_manifest_means_no_env_var() {
    let res = tmp("absent");
    write(&res.join("python/python.exe"), b"x");
    write(&res.join("site-packages/.keep"), b"");
    write(&res.join("backend/__init__.py"), b"");
    let spec = release_spawn_spec(&res, "backend.app", "backend");
    assert!(
        spec_map(&spec).is_none(),
        "no binaries.json means no PYLAIR_BINARIES"
    );
    let _ = std::fs::remove_dir_all(&res);
}

#[test]
fn release_strip_closes_inherited_channels_but_keeps_computed_map() {
    let mut spec = SpawnSpec::pylair(Path::new("python").into(), "backend.app");
    // The computed map is legitimate in release: env_remove fires BEFORE
    // spec.env applies, so the spec-carried value must survive stripping.
    spec.env.push((
        "PYLAIR_BINARIES".into(),
        "{\"ffmpeg\":\"/x/ffmpeg\"}".into(),
    ));
    strip_inherited_binaries(&mut spec);
    assert!(
        spec.env.iter().any(|(k, _)| k == "PYLAIR_BINARIES"),
        "the computed map is never stripped from the spec"
    );
    for key in ["PYLAIR_BINARIES", "PYLAIR_BINARIES_DIR"] {
        assert!(
            spec.env_remove.iter().any(|k| k == key),
            "inherited {key} must be env_remove'd"
        );
    }
    // Idempotent: a second generation must not duplicate the removals.
    strip_inherited_binaries(&mut spec);
    assert_eq!(
        spec.env_remove
            .iter()
            .filter(|k| *k == "PYLAIR_BINARIES")
            .count(),
        1
    );
}

#[test]
fn debug_inherited_map_wins_unchanged() {
    let dir = tmp("dbg-inherit");
    write(
        &dir.join("binaries.json"),
        manifest(&format!("\"tool\":{{{}}}", unit_entry("tool.exe"))).as_bytes(),
    );
    let mut spec = SpawnSpec::pylair(Path::new("python").into(), "backend.app");
    // Both variables inherited: PYLAIR_BINARIES wins outright, the dir
    // manifest is not even read (the entry's file does not exist, so a
    // read would yield an empty map and prove nothing either way).
    set_binaries_debug_env(&mut spec, |k| match k {
        "PYLAIR_BINARIES" => Some("{\"injected\":\"/fixture\"}".into()),
        "PYLAIR_BINARIES_DIR" => Some(dir.to_string_lossy().into_owned()),
        _ => None,
    });
    assert!(
        spec.env.iter().all(|(k, _)| k != "PYLAIR_BINARIES"),
        "nothing is pushed: the inherited value reaches the child as-is"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_dir_manifest_resolves_both_shapes_against_the_dir() {
    let dir = tmp("dbg-dir");
    write(&dir.join("ffmpeg.exe"), b"fake");
    write(&dir.join("llama/llama-server.exe"), b"fake");
    write(
        &dir.join("binaries.json"),
        manifest(&format!(
            "\"ffmpeg\":{{{}}},\"llama-server\":{{{}}}",
            file_entry("ffmpeg.exe"),
            unit_entry("llama/llama-server.exe")
        ))
        .as_bytes(),
    );
    let mut spec = SpawnSpec::pylair(Path::new("python").into(), "backend.app");
    set_binaries_debug_env(&mut spec, |k| {
        if k == "PYLAIR_BINARIES_DIR" {
            Some(dir.to_string_lossy().into_owned())
        } else {
            None
        }
    });
    let map = spec_map(&spec).expect("dir manifest produces a map");
    assert_eq!(map.len(), 2);
    // Both shapes resolve relative to the dir itself in dev.
    assert_eq!(PathBuf::from(&map["ffmpeg"]), dir.join("ffmpeg.exe"));
    assert_eq!(
        PathBuf::from(&map["llama-server"]),
        dir.join("llama/llama-server.exe")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_no_vars_sets_nothing() {
    let mut spec = SpawnSpec::pylair(Path::new("python").into(), "backend.app");
    set_binaries_debug_env(&mut spec, |_| None);
    assert!(spec_map(&spec).is_none());
}

#[test]
fn unknown_version_is_a_named_error() {
    let err = parse_manifest("{\"version\":2,\"binaries\":{}}").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("version"), "{msg}");
    assert!(msg.contains('2'), "{msg}");
    let err = parse_manifest("{\"binaries\":{}}").unwrap_err();
    assert!(err.to_string().contains("version"));
}

#[test]
fn absolute_paths_are_rejected_naming_the_entry() {
    for path in ["/abs/ffmpeg", "C:/tools/ffmpeg.exe"] {
        let err =
            parse_manifest(&manifest(&format!("\"ffmpeg\":{{{}}}", file_entry(path)))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ffmpeg"), "{msg} names the entry");
        assert!(msg.contains("relative"), "{msg}");
    }
}

#[test]
fn dotdot_segments_are_rejected_naming_the_entry() {
    let err = parse_manifest(&manifest(&format!(
        "\"ffmpeg\":{{{}}}",
        file_entry("../escape.exe")
    )))
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ffmpeg"), "{msg}");
    assert!(msg.contains(".."), "{msg}");
    // A backslash traversal is the same escape on Windows: rejected as a
    // contract violation before separators ever matter.
    let err = parse_manifest(&manifest(&format!(
        "\"ffmpeg\":{{{}}}",
        unit_entry("..\\\\escape.exe")
    )))
    .unwrap_err();
    assert!(err.to_string().contains("ffmpeg"));
}

#[test]
fn malformed_entries_are_named_errors() {
    for (json, want) in [
        (
            manifest(&format!(
                "\"ffmpeg\":{{{}}}",
                "\"shape\":\"x\",\"root\":\"exe_dir\",\"path\":\"a\""
            )),
            "shape",
        ),
        (
            manifest(&format!(
                "\"ffmpeg\":{{{}}}",
                "\"shape\":\"file\",\"root\":\"somewhere\",\"path\":\"a\""
            )),
            "root",
        ),
        (
            manifest(&format!(
                "\"ffmpeg\":{{{}}}",
                "\"shape\":\"file\",\"root\":\"exe_dir\""
            )),
            "path",
        ),
    ] {
        let err = parse_manifest(&json).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ffmpeg"), "{msg}");
        assert!(msg.contains(want), "{msg}");
    }
    assert!(parse_manifest("not json").is_err());
    assert!(parse_manifest("[]").is_err());
}

#[test]
fn map_json_is_compact_and_deterministic() {
    let mut map = BTreeMap::new();
    map.insert("b-tool".to_string(), PathBuf::from("/x/b"));
    map.insert("a-tool".to_string(), PathBuf::from("/x/a"));
    let json = map_to_json(&map);
    assert!(json.starts_with("{\"a-tool\""), "sorted keys: {json}");
    assert!(!json.contains(' '), "compact: {json}");
    let parsed: BTreeMap<String, String> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 2);
}

#[test]
fn entry_types_roundtrip_through_parse() {
    let entries = parse_manifest(&manifest(&format!(
        "\"ffmpeg\":{{{}}},\"llama\":{{{}}}",
        file_entry("ffmpeg.exe"),
        unit_entry("llama/llama-server.exe")
    )))
    .unwrap();
    assert_eq!(entries["ffmpeg"].shape, Shape::File);
    assert_eq!(entries["ffmpeg"].root, Root::ExeDir);
    assert_eq!(entries["llama"].shape, Shape::Unit);
    assert_eq!(entries["llama"].root, Root::Payload);
    assert_eq!(entries["llama"].path, "llama/llama-server.exe");
    // dev resolution shares the release resolver with a single base
    let dir = tmp("roundtrip");
    write(&dir.join("llama/llama-server.exe"), b"x");
    let map = resolve_dev_entries(&entries, &dir, |_| {});
    assert_eq!(map.len(), 1, "only the existing file resolves");
    let _ = std::fs::remove_dir_all(&dir);
}
