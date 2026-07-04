//! Shared, env-resolved paths for the on-box examples and the `#[ignore]d` tests, so neither
//! hardcodes a specific user or install location. Pure `std::env`/`std::path`, so it compiles and is
//! unit-tested on the Linux dev box (like [`crate::dpi`]); off Windows the lookups return temp/None.

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

#[cfg(test)]
mod tests {
    use super::*;

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
