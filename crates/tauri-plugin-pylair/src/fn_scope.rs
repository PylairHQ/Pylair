//! SPIKE ONLY (fn-scope, not for merge): per-function permissions through
//! Tauri command scopes on `pylair:allow-python-call` and
//! `pylair:allow-python-call-stream`.
//!
//! A capability attaches scope entries to those permissions, for example
//! `{ "identifier": "pylair:allow-python-call", "deny": [{ "name": "x_*" }] }`.
//! The command handlers read them through `CommandScope<FnScope>` and check
//! the requested function name before the supervisor is touched.

use crate::protocol::{self, ErrorObj};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::ipc::CommandScope;

/// One scope entry: an exact function name, or a prefix ending in a single
/// trailing `*`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FnScope {
    pub name: String,
}

/// Exact match, or prefix match when the pattern ends in `*`. Only a single
/// trailing star is a glob; a star anywhere else is a literal character.
pub fn pattern_matches(_pattern: &str, _name: &str) -> bool {
    false
}

/// Decide a call. No entries at all means everything is allowed. A deny
/// match always wins. When allow entries exist, one of them must match.
pub fn permitted(_allow: &[&str], _deny: &[&str], _name: &str) -> bool {
    true
}

/// Patterns that match none of the exposed function names, in input order.
pub fn unmatched_patterns<'a>(_patterns: &[&'a str], _exposed: &[&str]) -> Vec<&'a str> {
    Vec::new()
}

/// What a command handler saw in its `CommandScope`, for the spike's
/// evidence tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// The last scope a handler saw for `(window label, command)`.
#[doc(hidden)]
pub fn observed(_window: &str, _command: &str) -> Option<Seen> {
    None
}

/// Startup-warning prototype output: one line per window and pattern that
/// matched no exposed function.
#[doc(hidden)]
pub fn warnings() -> Vec<String> {
    Vec::new()
}

#[allow(dead_code)]
type Log = Mutex<HashMap<(String, String), Seen>>;
#[allow(dead_code)]
fn _unused(_: &CommandScope<FnScope>) -> Result<(), ErrorObj> {
    let _ = (protocol::PERMISSION_DENIED, json!(null));
    Ok(())
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
}
