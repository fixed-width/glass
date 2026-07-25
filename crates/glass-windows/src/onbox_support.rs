//! Shared, env-resolved paths for the on-box examples and the `#[ignore]d` tests, so neither
//! hardcodes a specific user or install location. Pure `std::env`/`std::path`, so it compiles and is
//! unit-tested on any host (like [`crate::dpi`]); off Windows the lookups return temp/None.

use std::path::Path;

/// A per-purpose scratch directory under the user's profile (`%USERPROFILE%`) — e.g. an isolated
/// Edge `--user-data-dir`. Falls back to the system temp dir if `USERPROFILE` is unset.
pub fn scratch_dir(name: &str) -> String {
    let base = std::env::var("USERPROFILE")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    format!("{}\\{}", base.trim_end_matches(['\\', '/']), name)
}

/// Locate `msedge.exe` via the standard per-machine install dirs (`%ProgramFiles(x86)%` then
/// `%ProgramFiles%`), returning the first that exists. `None` if Edge isn't installed (or off
/// Windows, where those vars are unset) — callers decide whether that's fatal.
pub fn locate_edge() -> Option<String> {
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(var) {
            let candidate = format!("{base}\\Microsoft\\Edge\\Application\\msedge.exe");
            if Path::new(&candidate).exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Pull Chromium's recorded shutdown disposition out of a `<user-data-dir>/Default/Preferences`
/// document: the string value of `"exit_type"`, which a Chromium-based browser sets to `"Crashed"`
/// while a session is in progress and rewrites to `"Normal"` only if it shuts down through its own
/// exit path. Reading it is how the on-box teardown test asserts that glass asked the app to close
/// rather than terminating it — the same flag that decides whether the next launch shows a
/// "Restore pages?" prompt.
///
/// A hand-rolled scan rather than a JSON dependency: the file is a large document and this needs
/// exactly one scalar from it. Returns `None` if the key is absent or its value is not a string.
pub fn exit_type_from_preferences(prefs: &str) -> Option<&str> {
    let after_key = prefs.split_once("\"exit_type\"")?.1;
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let value = after_colon.trim_start().strip_prefix('"')?;
    // Chromium writes these values as plain ASCII words, so no escape handling is needed.
    value.split_once('"').map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_type_reads_a_clean_shutdown() {
        let prefs = r#"{"profile":{"exit_type":"Normal","name":"Person 1"}}"#;
        assert_eq!(exit_type_from_preferences(prefs), Some("Normal"));
    }

    #[test]
    fn exit_type_reads_a_crash() {
        let prefs = r#"{"profile":{"exit_type":"Crashed"}}"#;
        assert_eq!(exit_type_from_preferences(prefs), Some("Crashed"));
    }

    #[test]
    fn exit_type_tolerates_pretty_printed_whitespace() {
        // Chromium writes compact JSON, but a hand-inspected/re-saved profile may not be — and a
        // parser that silently returned None there would make the teardown test pass vacuously.
        let prefs = "{\n  \"profile\": {\n    \"exit_type\" : \"Normal\"\n  }\n}";
        assert_eq!(exit_type_from_preferences(prefs), Some("Normal"));
    }

    #[test]
    fn exit_type_is_none_when_the_key_is_absent() {
        assert_eq!(exit_type_from_preferences(r#"{"profile":{}}"#), None);
    }

    #[test]
    fn exit_type_is_none_when_the_value_is_not_a_string() {
        assert_eq!(exit_type_from_preferences(r#"{"exit_type":3}"#), None);
    }

    #[test]
    fn scratch_dir_joins_name_under_a_base() {
        let p = scratch_dir("glass-probe");
        assert!(p.ends_with("\\glass-probe"), "got {p}");
        assert!(
            p.len() > "\\glass-probe".len(),
            "should have a base prefix: {p}"
        );
    }
}
