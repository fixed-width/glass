use std::process::Command;
use std::time::{Duration, Instant};

use glass_core::{GlassError, Result, run_bounded, run_bounded_until};

/// What a `simctl` invocation is doing, which is what decides how long it may take.
///
/// `BootStatus` is the outlier: `simctl bootstatus <udid> -b` blocks *by design* until the
/// simulator finishes booting — minutes on a cold machine — so it is bounded generously rather
/// than left unbounded, because a boot that never completes must still end as an error.
///
/// Budgets are ~4x the slowest healthy run measured on the dogfood simulator, floored at 10s — a
/// bound on a simulator that has stopped answering, not what a call is expected to cost. Measured
/// over 25 runs: `terminate` 101ms median, `io screenshot` well inside its own budget. The
/// exception is `shutdown` at ~3.3s, which is why nothing waits on one during teardown (glass#427,
/// `SimulatorRegistry::shutdown_all`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimctlOp {
    /// `bootstatus -b` — waits out the whole boot.
    BootStatus,
    /// `launch` / `boot` / `shutdown` — device lifecycle.
    Lifecycle,
    /// `install` — copies an `.app` into the simulator and registers it. Payload-sized, like
    /// Android's `Transfer`, and unlike the rest of the lifecycle verbs.
    Install,
    /// `io <udid> screenshot` — encodes a frame.
    Screenshot,
    /// Everything else: `list`, `pbcopy`, `pbpaste`, `terminate`, `spawn`. `terminate` is a
    /// teardown call and measures ~101ms, so the 10s here is headroom for a wedged simulator; the
    /// teardown *sequence* is bounded by the deadline [`Simctl::run_until`] carries instead.
    Query,
}

impl SimctlOp {
    /// The deadline for this kind of call.
    pub fn budget(self) -> Duration {
        match self {
            Self::BootStatus => Duration::from_secs(180),
            Self::Lifecycle => Duration::from_secs(60),
            Self::Install => Duration::from_secs(120),
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
            Some("install") => Self::Install,
            Some("launch" | "boot" | "shutdown" | "erase") => Self::Lifecycle,
            Some("io") => Self::Screenshot,
            _ => Self::Query,
        }
    }

    /// Operation name for the timeout error, prefixed so a reader knows which tool hung.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::BootStatus => "simctl:bootstatus",
            Self::Lifecycle => "simctl:lifecycle",
            Self::Install => "simctl:install",
            Self::Screenshot => "simctl:io screenshot",
            Self::Query => "simctl:query",
        }
    }
}

/// A stateless `xcrun simctl <argv>` runner. Every call site passes the target device's UDID
/// positionally in `sub` (matching how `simctl` itself takes it), so this holds no per-device
/// state.
///
/// The program is a field so a unit test can point it at a stand-in, as `glass-android`'s `Adb`
/// carries its `bin` — but unlike `Adb`, there is deliberately no production override.
#[derive(Clone, Debug)]
pub struct Simctl {
    program: String,
}

impl Default for Simctl {
    fn default() -> Self {
        Self::new()
    }
}

impl Simctl {
    /// A runner over the real `xcrun`.
    pub fn new() -> Self {
        Self {
            program: "xcrun".to_string(),
        }
    }

    /// A runner that invokes `program` in place of `xcrun` — the seam every stand-in in this
    /// crate's tests goes through, so no unit test loads CoreSimulator.
    #[cfg(test)]
    pub(crate) fn at(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }

    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    /// Full argv passed to `xcrun`: `simctl <sub...>`.
    pub(crate) fn full_args(&self, sub: &[&str]) -> Vec<String> {
        let mut v = vec!["simctl".to_string()];
        v.extend(sub.iter().map(|s| s.to_string()));
        v
    }

    /// Run `xcrun simctl <sub...>` and return captured stdout as lossy UTF-8 text.
    pub fn run(&self, sub: &[&str]) -> Result<String> {
        self.run_until(sub, None)
    }

    /// [`Simctl::run`] under a deadline the whole sequence shares, `None` for a call that answers
    /// to nothing but its own budget.
    ///
    /// Teardown is what needs it: glass-mcp abandons the whole of it at
    /// [`glass_core::TEARDOWN_BUDGET`], so a call that wedges would otherwise spend what the calls
    /// behind it needed (glass#427).
    pub fn run_until(&self, sub: &[&str], deadline: Option<Instant>) -> Result<String> {
        let out = self.output(sub, deadline)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn output(&self, sub: &[&str], deadline: Option<Instant>) -> Result<Vec<u8>> {
        let op = SimctlOp::for_sub(sub);
        let mut cmd = Command::new(self.program());
        cmd.args(self.full_args(sub));
        let out = match deadline {
            Some(d) => run_bounded_until(&mut cmd, op.budget(), d, op.label())?,
            None => run_bounded(&mut cmd, op.budget(), op.label())?,
        };
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

/// An `xcrun` that is a shell script rather than the real tool: it records every invocation and,
/// unless told otherwise, exits 0 saying nothing.
///
/// Records the argv because a teardown stubbed to do nothing is otherwise indistinguishable from
/// one that ran — the whole question a `Drop` test asks.
#[cfg(test)]
pub(crate) struct FakeSimctl {
    dir: tempfile::TempDir,
}

#[cfg(test)]
const FAKE_SIMCTL_SCRIPT: &str = "\
#!/bin/sh
# Stand-in for xcrun; see FakeSimctl in simctl.rs.
[ \"$1\" = --glass-probe ] && exit 0
dir=$(dirname \"$0\")
printf '%s\\n' \"$*\" >> \"$dir/calls\"
# `$2` is the simctl verb, since `$1` is always `simctl`.
# Recorded before sleeping, so a test can see the call while this one is still running.
[ -f \"$dir/slow-$2\" ] && sleep \"$(cat \"$dir/slow-$2\")\"
if [ -f \"$dir/fail-$2\" ]; then
    printf 'the fake xcrun was told to fail %s\\n' \"$2\" >&2
    exit 1
fi
case \"$*\" in
    *screenshot*)
        # A planted PNG is the only way past capture — the caller decodes the file this writes.
        # Its destination is the last argument.
        if [ -f \"$dir/screenshot.png\" ]; then
            for last in \"$@\"; do :; done
            cp \"$dir/screenshot.png\" \"$last\"
        fi
        ;;
esac
exit 0
";

#[cfg(test)]
impl FakeSimctl {
    pub(crate) fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        // A `TempDir`, not a pid-derived name — a pid the OS has since reused would hand this
        // fake an earlier run's recorded calls.
        let dir = tempfile::Builder::new()
            .prefix("glass-fake-simctl")
            .tempdir()
            .expect("create the fake xcrun's directory");
        let path = dir.path().join("xcrun");
        std::fs::write(&path, FAKE_SIMCTL_SCRIPT).expect("write the fake xcrun");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake xcrun executable");

        // Proven runnable before it is handed out: a parallel test's fork can be holding an
        // inherited write fd, which raises `ETXTBSY` on exec (`glass-android`'s
        // `write_executable`, same hazard). `--glass-probe` exits before recording anything.
        for _ in 0..100 {
            match Command::new(&path).arg("--glass-probe").status() {
                Ok(_) => return FakeSimctl { dir },
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("the fake xcrun is not runnable: {e}"),
            }
        }
        panic!("the fake xcrun was still ETXTBSY after 100 retries");
    }

    /// The path to hand [`Simctl::at`].
    pub(crate) fn program(&self) -> String {
        self.dir.path().join("xcrun").to_string_lossy().into_owned()
    }

    /// Answer `io … screenshot` with these PNG bytes, for a test that needs to get *past*
    /// capture — without one the fake writes no file and the caller fails to decode.
    pub(crate) fn writes_screenshot(&self, png: &[u8]) {
        std::fs::write(self.dir.path().join("screenshot.png"), png)
            .expect("plant the fake's screenshot");
    }

    /// Make every `simctl <verb>` call exit non-zero, for a test that asks what the caller does
    /// when the Simulator refuses.
    pub(crate) fn fails(&self, verb: &str) {
        std::fs::write(self.dir.path().join(format!("fail-{verb}")), "")
            .expect("tell the fake to fail");
    }

    /// Make every `simctl <verb>` call take `seconds`, for a test about whether the caller waits.
    /// The call is recorded before the sleep starts.
    pub(crate) fn slow(&self, verb: &str, seconds: u32) {
        std::fs::write(
            self.dir.path().join(format!("slow-{verb}")),
            seconds.to_string(),
        )
        .expect("tell the fake to be slow");
    }

    /// The argv of every invocation so far, in order, joined by spaces.
    pub(crate) fn calls(&self) -> Vec<String> {
        // An unreadable `calls` is "nothing run yet" only while the directory is there; gone,
        // every assertion built on this silently reads as "no calls".
        assert!(
            self.dir.path().exists(),
            "the fake xcrun's directory is gone; it was dropped before what it recorded was read"
        );
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Whether any invocation's argv contains `needle`.
    pub(crate) fn called(&self, needle: &str) -> bool {
        self.calls().iter().any(|c| c.contains(needle))
    }

    /// [`FakeSimctl::called`], waiting up to `within` for the call to arrive — for a caller that
    /// spawns rather than waits, where the argv lands after the call under test has returned.
    pub(crate) fn wait_called(&self, needle: &str, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        loop {
            if self.called(needle) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live proof that a wedged simctl call ends instead of hanging. Ignored by default:
    ///   GLASS_IOS_UDID=<booted udid> \
    ///     cargo test -p glass-ios --lib -- --ignored --nocapture a_spawned_command
    ///
    /// `simctl spawn <udid> /bin/sleep` is a real call into a real simulator that never answers,
    /// which is what the budget tests above cannot exercise. In-crate because `simctl` is a private
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
        // Pins what the enum doc explains, so a later edit cannot quietly fold BootStatus into
        // the generic budget.
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
            (vec!["install", "UDID", "/tmp/App.app"], SimctlOp::Install),
            (vec!["shutdown", "UDID"], SimctlOp::Lifecycle),
            (
                vec!["io", "UDID", "screenshot", "/tmp/f.png"],
                SimctlOp::Screenshot,
            ),
            (vec!["list", "devices", "-j"], SimctlOp::Query),
            (vec!["pbpaste", "UDID"], SimctlOp::Query),
            (vec!["pbcopy", "UDID"], SimctlOp::Query),
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
        // `Default` is hand-written and public API — a derived one would hand back an empty
        // program.
        assert_eq!(Simctl::default().program(), "xcrun");
        assert_eq!(
            s.full_args(&["help"]),
            vec!["simctl".to_string(), "help".to_string()]
        );
    }

    /// The fake is load-bearing for every teardown test — prove it records, or those tests pass
    /// against a stand-in that does nothing.
    #[test]
    fn the_fake_xcrun_records_what_it_was_asked_and_the_real_one_is_not_run() {
        let fake = FakeSimctl::new();
        let simctl = Simctl::at(fake.program());
        // A real `xcrun simctl terminate UDID app.id` could not answer `Ok("")`.
        assert_eq!(simctl.run(&["terminate", "UDID", "app.id"]).unwrap(), "");
        // Two calls: a fake that recorded only the first would satisfy every assertion a single
        // call can make.
        simctl.run(&["list", "devices"]).unwrap();

        assert_eq!(
            fake.calls(),
            vec!["simctl terminate UDID app.id", "simctl list devices"]
        );
        assert!(!fake.called("boot"), "{:?}", fake.calls());
    }

    #[test]
    fn a_fake_told_to_fail_a_verb_fails_that_verb_and_no_other() {
        let fake = FakeSimctl::new();
        let simctl = Simctl::at(fake.program());
        fake.fails("terminate");

        assert!(simctl.run(&["terminate", "UDID", "app.id"]).is_err());
        assert!(simctl.run(&["shutdown", "UDID"]).is_ok());
    }

    #[test]
    fn a_fake_removes_its_directory_when_it_drops() {
        let path = {
            let fake = FakeSimctl::new();
            std::path::PathBuf::from(fake.program())
        };
        assert!(!path.exists(), "{} outlived its fake", path.display());
    }
}
