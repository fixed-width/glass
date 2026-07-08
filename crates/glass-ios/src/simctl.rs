use std::process::Command;

use glass_core::{GlassError, Result};

/// Typed wrapper over `xcrun simctl`. Pure arg construction; a thin runner.
#[derive(Clone, Debug, Default)]
pub struct Simctl {
    udid: Option<String>,
}

impl Simctl {
    /// A wrapper with no device bound yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a resolved device UDID (callers pass it positionally where a subcommand wants it).
    pub fn bind(mut self, udid: impl Into<String>) -> Self {
        self.udid = Some(udid.into());
        self
    }

    /// The bound device UDID, if one has been set.
    pub fn udid(&self) -> Option<&str> {
        self.udid.as_deref()
    }

    pub(crate) fn program(&self) -> &'static str {
        "xcrun"
    }

    /// The simctl subcommand args, unchanged (kept as a seam for tests/consistency).
    pub fn args_for(&self, sub: &[&str]) -> Vec<String> {
        sub.iter().map(|s| s.to_string()).collect()
    }

    /// Full argv passed to `xcrun`: `simctl <sub...>`.
    pub(crate) fn full_args(&self, sub: &[&str]) -> Vec<String> {
        let mut v = vec!["simctl".to_string()];
        v.extend(sub.iter().map(|s| s.to_string()));
        v
    }

    /// Run `xcrun simctl <sub...>` and return captured stdout as lossy UTF-8 text.
    pub fn run(&self, sub: &[&str]) -> Result<String> {
        let out = self.output(sub)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Run `xcrun simctl <sub...>` and return captured stdout as raw bytes.
    pub fn run_bytes(&self, sub: &[&str]) -> Result<Vec<u8>> {
        self.output(sub)
    }

    fn output(&self, sub: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new(self.program())
            .args(self.full_args(sub))
            .output()
            .map_err(|e| GlassError::Backend(format!("failed to run xcrun simctl: {e}")))?;
        if !out.status.success() {
            return Err(GlassError::Backend(format!(
                "simctl {:?} failed: {}",
                sub,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_prefix_is_xcrun_simctl_free() {
        // args_for returns only the simctl subcommand args; the runner prepends `simctl`.
        let s = Simctl::new();
        assert_eq!(
            s.args_for(&["list", "devices", "available", "--json"]),
            vec![
                "list".to_string(),
                "devices".to_string(),
                "available".to_string(),
                "--json".to_string()
            ]
        );
    }

    #[test]
    fn program_is_xcrun_with_simctl_first_arg() {
        let s = Simctl::new();
        assert_eq!(s.program(), "xcrun");
        assert_eq!(
            s.full_args(&["help"]),
            vec!["simctl".to_string(), "help".to_string()]
        );
    }
}
