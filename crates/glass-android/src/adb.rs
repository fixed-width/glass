use std::process::{Command, Output};
use std::time::Duration;

use glass_core::{GlassError, Result, run_bounded};

/// What an adb invocation is doing, which is what decides how long it may take.
///
/// One deadline for the whole backend would have to accommodate the slowest healthy call — a full
/// `uiautomator dump`, or an APK install — and would then let a wedged `input` hang just as long.
///
/// Budgets are ~4x the slowest healthy run measured on the dogfood AVD, floored at 10s: a budget
/// that fires on a loaded-but-working device is worse than the hang it replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdbOp {
    /// `uiautomator dump` — walks the whole view tree on device.
    Dump,
    /// `exec-out screencap` — encodes a full-resolution frame.
    Screencap,
    /// `install` / `push` — copies and verifies a file over the wire.
    Transfer,
    /// Everything else: `shell`, `input`, `am start`, `devices`, `forward`, `getprop`.
    Shell,
}

impl AdbOp {
    /// The deadline for this kind of call.
    pub fn budget(self) -> Duration {
        match self {
            Self::Dump => Duration::from_secs(20),
            Self::Screencap => Duration::from_secs(15),
            Self::Transfer => Duration::from_secs(120),
            Self::Shell => Duration::from_secs(10),
        }
    }

    /// Classify an adb argv, so no call site has to pass its own budget and none can pick a longer
    /// one than its work needs. An unrecognized call is [`AdbOp::Shell`] — the SHORT budget, so a
    /// future caller that forgets to extend this fails fast rather than hanging for two minutes.
    pub fn for_args(args: &[&str]) -> Self {
        if args.iter().any(|a| a.contains("uiautomator")) {
            Self::Dump
        } else if args.iter().any(|a| a.contains("screencap")) {
            Self::Screencap
        } else if args.iter().any(|a| {
            matches!(
                *a,
                "install" | "install-multiple" | "push" | "pull" | "uninstall"
            )
        }) {
            Self::Transfer
        } else {
            Self::Shell
        }
    }

    /// Operation name for the timeout error, prefixed so a reader knows which tool hung.
    fn label(self) -> &'static str {
        match self {
            Self::Dump => "adb:uiautomator dump",
            Self::Screencap => "adb:screencap",
            Self::Transfer => "adb:file transfer",
            Self::Shell => "adb:shell",
        }
    }
}

/// A thin wrapper over the `adb` binary, targeting one device serial.
#[derive(Clone, Debug)]
pub struct Adb {
    bin: String,
    serial: Option<String>,
}

impl Adb {
    /// Resolve the `adb` binary: `GLASS_ADB`, else `$SDK/platform-tools/adb` from a
    /// discovered SDK root (env or a common install location), else `"adb"` on `PATH`.
    /// Serial is unset until a target resolves it.
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        let bin = crate::sdk::resolve_adb(&get, &|p| p.exists()).bin();
        Self { bin, serial: None }
    }

    /// Return a copy bound to `serial`.
    pub fn with_serial(&self, serial: impl Into<String>) -> Self {
        Self {
            bin: self.bin.clone(),
            serial: Some(serial.into()),
        }
    }

    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }

    /// The resolved adb binary, for callers spawning their own long-lived adb process.
    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// Run adb with captured text stdout; `Backend` error on spawn failure or non-zero exit.
    pub fn run<'a, I>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let out = self.output(args)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run adb capturing stdout *and* stderr. For a command whose exit status does not
    /// signal success — `uiautomator dump` exits 0 when it fails, and explains itself on
    /// stderr — the caller has to judge the outcome from the streams itself.
    pub fn run_streams<'a, I>(&self, args: I) -> Result<(String, String)>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let out = self.output(args)?;
        Ok((
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// Run adb capturing raw stdout bytes (e.g. `exec-out screencap`).
    pub fn run_bytes<'a, I>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = &'a str>,
    {
        Ok(self.output(args)?.stdout)
    }

    /// Run adb and return the completed process, erroring on spawn failure or non-zero
    /// exit so every caller reports those two the same way.
    fn output<'a, I>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let args: Vec<&str> = args.into_iter().collect();
        let op = AdbOp::for_args(&args);
        let argv = build_argv(self.serial.as_deref(), &args);
        let mut cmd = Command::new(&self.bin);
        cmd.args(&argv);
        // A wedged adb server answers nothing at all, and the fix is one command — so the remedy
        // rides in the error the caller sees rather than in a log line nobody reads.
        // Extend the message rather than wrapping the error: nesting one `Backend` inside another
        // makes `Display` print its prefix twice, and this text is read by an agent.
        let out = run_bounded(&mut cmd, op.budget(), op.label()).map_err(|e| match e {
            GlassError::Backend(msg) => GlassError::Backend(format!(
                "{msg}; if this repeats, run `adb kill-server` and retry"
            )),
            other => other,
        })?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(exit_error(&self.bin, &argv, &out))
        }
    }
}

/// Build the adb argument vector, prefixing `-s <serial>` when targeting a device.
/// Pure, so it is unit-tested without invoking adb.
pub(crate) fn build_argv(serial: Option<&str>, args: &[&str]) -> Vec<String> {
    let mut v = Vec::with_capacity(args.len() + 2);
    if let Some(s) = serial {
        v.push("-s".to_string());
        v.push(s.to_string());
    }
    v.extend(args.iter().map(|a| a.to_string()));
    v
}

fn exit_error(bin: &str, argv: &[String], out: &Output) -> GlassError {
    GlassError::Backend(format!(
        "`{bin} {}` failed: {}",
        argv.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::AdbOp;
    use std::time::Duration;

    /// Live proof that a wedged adb call ends instead of hanging. Ignored by default:
    ///   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
    ///     cargo test -p glass-android --lib -- --ignored --nocapture a_shell_call
    ///
    /// The budget tests above cover which deadline a call gets, and `glass_core::bounded`'s cover
    /// the mechanism against a local `sh`. Neither exercises what actually failed before this
    /// existed: a real `adb` call that never answers. It lives in-crate rather than in
    /// `tests/*_loop.rs` because `adb` is a private module, and a test is not a reason to widen the
    /// crate's public surface.
    #[test]
    #[ignore = "requires a booted AVD + GLASS_ADB"]
    fn a_shell_call_that_never_answers_dies_at_its_budget() {
        let adb = super::Adb::from_env();
        let budget = AdbOp::Shell.budget();

        let started = std::time::Instant::now();
        // Ten times the budget, so "it returned because the command finished" is no explanation.
        let err = adb
            .run(["shell", "sleep", "100"])
            .expect_err("a call that outlives its budget must fail, not return");
        let waited = started.elapsed();
        println!("waited {waited:?} against a {budget:?} budget; error: {err}");

        assert!(
            waited < budget + Duration::from_secs(5),
            "waited {waited:?}, past the {budget:?} budget — the bound did not fire"
        );
        let msg = err.to_string();
        // Whoever reads this has to learn which call hung and what to do about it.
        assert!(msg.contains("adb:shell"), "must name the operation: {msg}");
        assert!(
            msg.contains("adb kill-server"),
            "must carry the remedy: {msg}"
        );
        // Extending the message must not wrap one Backend error in another: Display prints its
        // prefix per layer, and "backend error: backend error: ..." is what an agent would read.
        assert_eq!(
            msg.matches("backend error").count(),
            1,
            "the error prefix must appear once: {msg}"
        );

        // The next call still works, so the timeout killed the child rather than leaving the
        // connection wedged behind it.
        let devices = adb.run(["devices"]).expect("adb usable after a timeout");
        assert!(devices.contains("List of devices"), "{devices}");
    }

    #[test]
    fn a_dump_gets_the_longest_budget_of_the_interactive_calls() {
        // The slowest healthy call decides its own deadline; a single per-backend budget would
        // have to be sized for it and would then let a wedged `input` hang exactly as long.
        assert!(AdbOp::Dump.budget() > AdbOp::Shell.budget());
        assert!(AdbOp::Transfer.budget() > AdbOp::Dump.budget());
        assert!(AdbOp::Shell.budget() >= Duration::from_secs(10));
    }

    #[test]
    fn every_argv_this_crate_actually_runs_classifies_as_intended() {
        // The real argvs, taken from the call sites, so a refactor that renames a subcommand is
        // caught here rather than by a call silently dropping to the short budget.
        for (argv, want) in [
            (
                vec!["shell", "uiautomator", "dump", "/sdcard/glass_dump.xml"],
                AdbOp::Dump,
            ),
            (vec!["exec-out", "screencap"], AdbOp::Screencap),
            (vec!["install", "-r", "/tmp/app.apk"], AdbOp::Transfer),
            (
                vec!["push", "/tmp/agent.jar", "/data/local/tmp/agent.jar"],
                AdbOp::Transfer,
            ),
            (
                vec!["uninstall", "tech.fixedwidth.glass.a11y"],
                AdbOp::Transfer,
            ),
            (vec!["shell", "input", "tap", "10", "20"], AdbOp::Shell),
            (vec!["shell", "am", "start", "-n", "pkg/.Act"], AdbOp::Shell),
            (vec!["shell", "dumpsys", "window", "windows"], AdbOp::Shell),
            (vec!["devices"], AdbOp::Shell),
            (
                vec!["forward", "tcp:0", "localabstract:glass"],
                AdbOp::Shell,
            ),
            (vec!["shell", "getprop", "sys.boot_completed"], AdbOp::Shell),
            (vec!["emu", "kill"], AdbOp::Shell),
            (vec!["version"], AdbOp::Shell),
        ] {
            assert_eq!(AdbOp::for_args(&argv), want, "argv {argv:?}");
        }
    }

    #[test]
    fn an_unrecognized_call_takes_the_short_budget_not_a_generous_one() {
        // Fail fast beats hanging for two minutes when a future caller forgets to extend the
        // classifier.
        assert_eq!(AdbOp::for_args(&["some-future-subcommand"]), AdbOp::Shell);
    }

    use super::build_argv;

    #[test]
    fn argv_without_serial_is_passthrough() {
        assert_eq!(build_argv(None, &["devices"]), vec!["devices".to_string()]);
    }

    #[test]
    fn argv_with_serial_prefixes_dash_s() {
        assert_eq!(
            build_argv(Some("emulator-5554"), &["shell", "echo", "hi"]),
            vec!["-s", "emulator-5554", "shell", "echo", "hi"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }
}
