//! SPIKE ONLY (fn-scope, not for merge): per-function permissions through
//! Tauri command scopes on `pylair:allow-python-call` and
//! `pylair:allow-python-call-stream`.
//!
//! A capability attaches scope entries to those permissions, for example
//! `{ "identifier": "pylair:allow-python-call", "deny": [{ "name": "x_*" }] }`.
//! The command handlers read them through `CommandScope<FnScope>` and check
//! the requested function name before the supervisor is touched.

use crate::protocol::{self, ErrorObj};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use tauri::ipc::CommandScope;

/// One scope entry: an exact function name, or a prefix ending in a single
/// trailing `*`. Unknown keys are an error so a typo in a capability fails
/// closed instead of silently becoming "no entry".
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FnScope {
    pub name: String,
}

/// Exact match, or prefix match when the pattern ends in `*`. Only a single
/// trailing star is a glob; a star anywhere else is a literal character.
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Decide a call. No entries at all means everything is allowed. A deny
/// match always wins. When allow entries exist, one of them must match.
pub fn permitted(allow: &[&str], deny: &[&str], name: &str) -> bool {
    if deny.iter().any(|p| pattern_matches(p, name)) {
        return false;
    }
    allow.is_empty() || allow.iter().any(|p| pattern_matches(p, name))
}

/// Patterns that match none of the exposed function names, in input order.
pub fn unmatched_patterns<'a>(patterns: &[&'a str], exposed: &[&str]) -> Vec<&'a str> {
    patterns
        .iter()
        .copied()
        .filter(|p| !exposed.iter().any(|n| pattern_matches(p, n)))
        .collect()
}

/// The function names from the last handshake, once there has been one.
/// `None` means "not known yet", which is different from "exposes nothing".
#[derive(Default)]
pub struct ExposedNames(Mutex<Option<Vec<String>>>);

impl ExposedNames {
    /// Record the `exposes` manifest (a list of `{name, ...}` objects).
    pub(crate) fn set(&self, manifest: &Value) {
        let names = manifest
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        *self.0.lock().unwrap() = Some(names);
    }

    fn get(&self) -> Option<Vec<String>> {
        self.0.lock().unwrap().clone()
    }
}

/// What a command handler saw in its `CommandScope`, for the spike's
/// evidence tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

fn log() -> &'static Mutex<HashMap<(String, String), Seen>> {
    static LOG: OnceLock<Mutex<HashMap<(String, String), Seen>>> = OnceLock::new();
    LOG.get_or_init(Default::default)
}

/// The last scope a handler saw for `(window label, command)`.
#[doc(hidden)]
pub fn observed(window: &str, command: &str) -> Option<Seen> {
    log()
        .lock()
        .unwrap()
        .get(&(window.to_string(), command.to_string()))
        .cloned()
}

fn warning_lines() -> &'static Mutex<Vec<String>> {
    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LINES.get_or_init(Default::default)
}

/// Startup-warning prototype output: one line per window and pattern that
/// matched no exposed function.
#[doc(hidden)]
pub fn warnings() -> Vec<String> {
    warning_lines().lock().unwrap().clone()
}

/// Prototype of the "pattern matches nothing" warning. Tauri's public API
/// cannot enumerate the scopes of a capability at startup, because a
/// `CommandScope` only resolves inside an invoke from a webview. So this
/// runs on every call, and warns once per window and pattern, as soon as
/// both the scope and the exposes manifest are known.
fn warn_unmatched(window: &str, seen: &Seen, exposed: &ExposedNames) {
    static WARNED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let Some(names) = exposed.get() else { return };
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let patterns: Vec<&str> = seen
        .allow
        .iter()
        .chain(&seen.deny)
        .map(String::as_str)
        .collect();
    let warned = WARNED.get_or_init(Default::default);
    for p in unmatched_patterns(&patterns, &names) {
        if warned
            .lock()
            .unwrap()
            .insert((window.to_string(), p.to_string()))
        {
            let line = format!(
                "pylair: scope pattern \"{p}\" for window \"{window}\" matches no exposed function"
            );
            eprintln!("{line}");
            warning_lines().lock().unwrap().push(line);
        }
    }
}

/// The check both call commands run before touching the supervisor, the
/// registry or the channel.
pub(crate) fn check(
    scope: &CommandScope<FnScope>,
    command: &str,
    window: &str,
    fn_name: &str,
    exposed: &ExposedNames,
) -> Result<(), ErrorObj> {
    let seen = Seen {
        allow: scope.allows().iter().map(|s| s.name.clone()).collect(),
        deny: scope.denies().iter().map(|s| s.name.clone()).collect(),
    };
    warn_unmatched(window, &seen, exposed);
    let allow: Vec<&str> = seen.allow.iter().map(String::as_str).collect();
    let deny: Vec<&str> = seen.deny.iter().map(String::as_str).collect();
    let ok = permitted(&allow, &deny, fn_name);
    log()
        .lock()
        .unwrap()
        .insert((window.to_string(), command.to_string()), seen);
    if ok {
        return Ok(());
    }
    Err(ErrorObj {
        code: protocol::PERMISSION_DENIED,
        message: format!("function \"{fn_name}\" is not permitted for window \"{window}\""),
        data: Some(json!({ "fn": fn_name, "window": window })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_name_matches_only_itself() {
        assert!(pattern_matches("add", "add"));
        assert!(!pattern_matches("add", "add2"));
        assert!(!pattern_matches("add", "ad"));
        assert!(!pattern_matches("add", "Add"));
    }

    #[test]
    fn trailing_star_is_a_prefix_match() {
        assert!(pattern_matches("x_*", "x_foo"));
        assert!(pattern_matches("x_*", "x_"));
        assert!(!pattern_matches("x_*", "x"));
        assert!(!pattern_matches("x_*", "ax_foo"));
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("*", ""));
    }

    #[test]
    fn a_star_that_is_not_trailing_is_a_literal() {
        assert!(!pattern_matches("a*b", "axb"));
        assert!(pattern_matches("a*b", "a*b"));
        assert!(!pattern_matches("*x", "ax"));
        assert!(pattern_matches("*x", "*x"));
    }

    #[test]
    fn empty_pattern_matches_only_the_empty_name() {
        assert!(pattern_matches("", ""));
        assert!(!pattern_matches("", "a"));
    }

    #[test]
    fn no_entries_allows_everything() {
        assert!(permitted(&[], &[], "anything"));
        assert!(permitted(&[], &[], ""));
    }

    #[test]
    fn deny_match_denies() {
        assert!(!permitted(&[], &["x_*"], "x_secret"));
        assert!(permitted(&[], &["x_*"], "add"));
    }

    #[test]
    fn deny_beats_an_allow_that_also_matches() {
        assert!(!permitted(&["x_*"], &["x_secret"], "x_secret"));
        assert!(permitted(&["x_*"], &["x_secret"], "x_other"));
        assert!(!permitted(&["*"], &["*"], "add"));
    }

    #[test]
    fn allow_entries_present_but_no_match_denies() {
        assert!(!permitted(&["add"], &[], "counter"));
        assert!(permitted(&["add"], &[], "add"));
        assert!(permitted(&["add", "count*"], &[], "counter"));
    }

    #[test]
    fn unmatched_patterns_lists_patterns_matching_no_exposed_function() {
        let exposed = ["add", "counter", "spy"];
        assert_eq!(
            unmatched_patterns(&["add", "typo_*", "count*", "nope"], &exposed),
            vec!["typo_*", "nope"]
        );
        assert!(unmatched_patterns(&["*"], &exposed).is_empty());
        assert_eq!(unmatched_patterns(&["*"], &[]), vec!["*"]);
    }

    #[test]
    fn scope_entries_reject_unknown_keys_and_missing_names() {
        let ok: FnScope = serde_json::from_value(json!({"name": "x_*"})).unwrap();
        assert_eq!(ok.name, "x_*");
        assert!(serde_json::from_value::<FnScope>(json!({"nmae": "x"})).is_err());
        assert!(serde_json::from_value::<FnScope>(json!({"name": "x", "glob": true})).is_err());
    }

    #[test]
    fn exposed_names_distinguish_unknown_from_empty() {
        let e = ExposedNames::default();
        assert_eq!(e.get(), None);
        e.set(&json!([]));
        assert_eq!(e.get(), Some(vec![]));
        e.set(&json!([{"name": "add", "kind": "call"}, {"name": "counter"}]));
        assert_eq!(
            e.get(),
            Some(vec!["add".to_string(), "counter".to_string()])
        );
    }
}
