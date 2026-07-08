use std::process::Command;

use glass_core::{GlassError, Result};

/// A stateless `xcrun simctl <argv>` runner. Every call site passes the target device's UDID
/// positionally in `sub` (matching how `simctl` itself takes it), so this holds no per-device
/// state.
#[derive(Clone, Debug, Default)]
pub struct Simctl;

impl Simctl {
    /// A new runner. There is nothing to configure — `Simctl` is stateless.
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn program(&self) -> &'static str {
        "xcrun"
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
    fn program_is_xcrun_with_simctl_first_arg() {
        let s = Simctl::new();
        assert_eq!(s.program(), "xcrun");
        assert_eq!(
            s.full_args(&["help"]),
            vec!["simctl".to_string(), "help".to_string()]
        );
    }
}
