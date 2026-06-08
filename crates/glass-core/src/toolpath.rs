//! Resolve external-tool binaries from `GLASS_*` env overrides, with built-in defaults.
//!
//! glass shells out to a few third-party programs (bubblewrap, Xvfb, sway, the build
//! shell). Each is resolved through here so a user can point glass at a binary in a
//! non-standard location via an environment variable, with no code change. An unset or
//! blank override always falls back to the built-in default.

/// Pure core of [`tool_path`]: an unset (`None`) or blank override yields `default`.
fn pick(override_value: Option<&str>, default: &str) -> String {
    match override_value {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => default.to_string(),
    }
}

/// Resolve an external tool's binary: the value of env var `env_key` when set and
/// non-blank, otherwise `default` (a bare name resolved via `PATH`, or an explicit path).
pub fn tool_path(env_key: &str, default: &str) -> String {
    pick(std::env::var(env_key).ok().as_deref(), default)
}

#[cfg(test)]
mod tests {
    use super::pick;

    #[test]
    fn unset_uses_default() {
        assert_eq!(pick(None, "bwrap"), "bwrap");
    }

    #[test]
    fn blank_uses_default() {
        assert_eq!(pick(Some(""), "Xvfb"), "Xvfb");
        assert_eq!(pick(Some("   "), "Xvfb"), "Xvfb");
    }

    #[test]
    fn set_value_overrides_default() {
        assert_eq!(pick(Some("/opt/bin/bwrap"), "bwrap"), "/opt/bin/bwrap");
        assert_eq!(pick(Some("my-sway"), "sway"), "my-sway");
    }
}
