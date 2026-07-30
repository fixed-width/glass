use std::process::Command;
use std::time::Duration;

use glass_core::{GlassError, Result, run_bounded};

/// What a `simctl` invocation is doing, which is what decides how long it may take.
///
/// `BootStatus` is the outlier: `simctl bootstatus <udid> -b` blocks *by design* until the
/// simulator finishes booting — minutes on a cold machine — so it is bounded generously rather
/// than left unbounded, because a boot that never completes must still end as an error.
///
/// Budgets are ~4x the slowest healthy run measured on the dogfood simulator, floored at 10s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimctlOp {
    /// `bootstatus -b` — waits out the whole boot.
    BootStatus,
    /// `launch` / `install` / `boot` / `shutdown` — device lifecycle.
    Lifecycle,
    /// `io <udid> screenshot` — encodes a frame.
    Screenshot,
    /// Everything else: `list`, `pbpaste`, `terminate`, `spawn`.
    Query,
}

impl SimctlOp {
    /// The deadline for this kind of call.
    pub fn budget(self) -> Duration {
        match self {
            Self::BootStatus => Duration::from_secs(180),
            Self::Lifecycle => Duration::from_secs(60),
            Self::Screenshot => Duration::from_secs(15),
            Self::Query => Duration::from_secs(10),
        }
    }

    /// Classify a `simctl` subcommand argv, so no call site passes its own budget and none can pick
    /// a longer one than its work needs. An unrecognized call is [`SimctlOp::Query`] — the SHORT
    /// budget, so a future caller that forgets to extend this fails fast.
    pub fn for_sub(sub: &[&str]) -> Self {
        match sub.first().copied() {
            Some("bootstatus") => Self::BootStatus,
            Some("launch" | "install" | "boot" | "shutdown" | "erase") => Self::Lifecycle,
            Some("io") => Self::Screenshot,
            _ => Self::Query,
        }
    }

    /// Operation name for the timeout error, prefixed so a reader knows which tool hung.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::BootStatus => "simctl:bootstatus",
            Self::Lifecycle => "simctl:lifecycle",
            Self::Screenshot => "simctl:io screenshot",
            Self::Query => "simctl:query",
        }
    }
}

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
        let op = SimctlOp::for_sub(sub);
        let mut cmd = Command::new(self.program());
        cmd.args(self.full_args(sub));
        let out = run_bounded(&mut cmd, op.budget(), op.label())?;
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

    /// Live proof that a wedged simctl call ends instead of hanging. Ignored by default:
    ///   GLASS_IOS_UDID=<booted udid> \
    ///     cargo test -p glass-ios --lib -- --ignored --nocapture a_spawned_command
    ///
    /// `simctl spawn <udid> sleep` is a real call into a real simulator that never answers, which
    /// is what the budget tests above cannot exercise. In-crate because `simctl` is a private
    /// module, and a test is not a reason to widen the crate's public surface.
    #[test]
    #[ignore = "requires a booted simulator + GLASS_IOS_UDID"]
    fn a_spawned_command_that_never_answers_dies_at_its_budget() {
        let udid = std::env::var("GLASS_IOS_UDID").expect("GLASS_IOS_UDID");
        let budget = SimctlOp::Query.budget();

        let started = std::time::Instant::now();
        let err = Simctl::new()
            // Absolute path: `spawn` runs the binary INSIDE the simulator, where a bare `sleep`
            // is not on the path and fails instantly with ENOENT rather than hanging.
            .run(&["spawn", &udid, "/bin/sleep", "100"])
            .expect_err("a call that outlives its budget must fail, not return");
        let waited = started.elapsed();
        println!("waited {waited:?} against a {budget:?} budget; error: {err}");

        assert!(
            waited < budget + Duration::from_secs(5),
            "waited {waited:?}, past the {budget:?} budget — the bound did not fire"
        );
        assert!(
            err.to_string().contains("simctl:query"),
            "must name the operation: {err}"
        );
        // The next call still works: the timeout killed the child, it did not wedge simctl.
        let listed = Simctl::new()
            .run(&["list", "devices"])
            .expect("still usable");
        assert!(listed.contains("Devices"), "{listed}");
    }

    #[test]
    fn bootstatus_may_take_far_longer_than_any_ordinary_call() {
        // `simctl bootstatus <udid> -b` blocks until the simulator finishes booting — minutes on a
        // cold machine — so it cannot share a deadline sized for a query.
        assert!(SimctlOp::BootStatus.budget() >= Duration::from_secs(180));
        assert!(SimctlOp::Query.budget() < SimctlOp::Lifecycle.budget());
        assert!(SimctlOp::Query.budget() >= Duration::from_secs(10));
    }

    #[test]
    fn every_sub_this_crate_actually_runs_classifies_as_intended() {
        // The real argvs, taken from the call sites, so renaming a subcommand is caught here
        // rather than by a call silently dropping to the short budget.
        for (sub, want) in [
            (vec!["bootstatus", "UDID", "-b"], SimctlOp::BootStatus),
            (
                vec!["launch", "UDID", "com.example.app"],
                SimctlOp::Lifecycle,
            ),
            (vec!["install", "UDID", "/tmp/App.app"], SimctlOp::Lifecycle),
            (vec!["shutdown", "UDID"], SimctlOp::Lifecycle),
            (
                vec!["io", "UDID", "screenshot", "/tmp/f.png"],
                SimctlOp::Screenshot,
            ),
            (vec!["list", "devices", "-j"], SimctlOp::Query),
            (vec!["pbpaste", "UDID"], SimctlOp::Query),
            (
                vec!["terminate", "UDID", "com.example.app"],
                SimctlOp::Query,
            ),
        ] {
            assert_eq!(SimctlOp::for_sub(&sub), want, "sub {sub:?}");
        }
    }

    #[test]
    fn an_unrecognized_sub_takes_the_short_budget_not_a_generous_one() {
        assert_eq!(SimctlOp::for_sub(&["some-future-sub"]), SimctlOp::Query);
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
