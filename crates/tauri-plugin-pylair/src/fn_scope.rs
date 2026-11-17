//! Per-function permissions: a capability can limit which `@expose`d
//! functions a window may call, through Tauri command scopes on
//! `pylair:allow-python-call` and `pylair:allow-python-call-stream`.
//!
//! ```json
//! { "identifier": "pylair:default", "deny": [{ "name": "admin_*" }] }
//! ```
//!
//! Both call commands read a `CommandScope<FnScope>` and run [`check`] before
//! the window is subscribed, a task is allocated or anything is sent, so a
//! denial touches no supervisor state. The rules (docs/security.md):
//!
//! - no entries at all: allowed (unchanged behaviour);
//! - a matching deny entry always denies;
//! - if any allow entry exists, one must match.
//!
//! Entries from every capability that covers the window are combined, so an
//! allow entry in any of them narrows that window.

use crate::protocol::{self, ErrorObj};
use serde_json::json;
use tauri::ipc::CommandScope;

/// One scope entry: an exact function name, or a prefix ending in a single
/// trailing `*`. Unknown keys are an error, so a typo in a capability fails
/// closed (Tauri refuses the call) instead of silently becoming "no entry".
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FnScope {
    pub name: String,
}

/// Exact match, or prefix match when the pattern ends in `*`. Only a single
/// trailing star is a glob; a star anywhere else is a literal character.
pub(crate) fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Decide a call. No entries at all means everything is allowed. A deny
/// match always wins. When allow entries exist, one of them must match.
pub(crate) fn permitted(allow: &[&str], deny: &[&str], name: &str) -> bool {
    if deny.iter().any(|p| pattern_matches(p, name)) {
        return false;
    }
    allow.is_empty() || allow.iter().any(|p| pattern_matches(p, name))
}

/// The error a denied call returns.
pub(crate) fn denied(window: &str, fn_name: &str) -> ErrorObj {
    ErrorObj {
        code: protocol::PERMISSION_DENIED,
        message: format!("function \"{fn_name}\" is not permitted for window \"{window}\""),
        data: Some(json!({ "fn": fn_name, "window": window })),
    }
}

/// The check both call commands run first.
pub(crate) fn check(
    scope: &CommandScope<FnScope>,
    window: &str,
    fn_name: &str,
) -> Result<(), ErrorObj> {
    let allow: Vec<&str> = scope.allows().iter().map(|s| s.name.as_str()).collect();
    let deny: Vec<&str> = scope.denies().iter().map(|s| s.name.as_str()).collect();
    if permitted(&allow, &deny, fn_name) {
        Ok(())
    } else {
        Err(denied(window, fn_name))
    }
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
    fn only_a_single_trailing_star_is_a_glob() {
        // "x**" is the prefix "x*" followed by a glob star: it matches names
        // starting with the literal "x*", not every name starting with "x".
        assert!(pattern_matches("x**", "x*"));
        assert!(pattern_matches("x**", "x*y"));
        assert!(!pattern_matches("x**", "xy"));
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
    fn scope_entries_reject_unknown_keys_and_missing_names() {
        let ok: FnScope = serde_json::from_value(json!({"name": "x_*"})).unwrap();
        assert_eq!(ok.name, "x_*");
        assert!(serde_json::from_value::<FnScope>(json!({"nmae": "x"})).is_err());
        assert!(serde_json::from_value::<FnScope>(json!({"name": "x", "glob": true})).is_err());
        assert!(serde_json::from_value::<FnScope>(json!({})).is_err());
        assert!(serde_json::from_value::<FnScope>(json!("x_*")).is_err());
    }

    #[test]
    fn the_denial_error_names_the_function_and_the_window() {
        let e = denied("win-b", "admin_reset");
        assert_eq!(e.code, -32050);
        assert_eq!(e.code, protocol::PERMISSION_DENIED);
        assert!(e.message.contains("admin_reset"), "{}", e.message);
        assert!(e.message.contains("win-b"), "{}", e.message);
        assert_eq!(
            e.data,
            Some(json!({"fn": "admin_reset", "window": "win-b"}))
        );
    }
}
