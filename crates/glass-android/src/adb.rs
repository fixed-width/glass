use std::process::{Command, Output};
use std::time::{Duration, Instant};

use glass_core::{BoundKind, GlassError, Result, run_bounded, run_bounded_until};

/// What an adb invocation is doing, which is what decides how long it may take.
///
/// One deadline for the whole backend would have to accommodate the slowest healthy call — a full
/// `uiautomator dump`, or an APK install — and would then let a wedged `input` hang just as long.
///
/// Budgets are ~4x the slowest healthy run measured on the dogfood AVD, floored at 10s: a budget
/// that fires on a loaded-but-working device is worse than the hang it replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdbOp {
    /// `uiautomator dump`, and the `cat` that reads the tree it wrote — two steps of one snapshot,
    /// so they share a deadline rather than the read-back inheriting a tap's. That deadline bounds
    /// the whole snapshot: `a11y::dump_once` runs all three of its steps inside this budget,
    /// including the removal of the file it read, which classifies as [`AdbOp::Shell`].
    Dump,
    /// `exec-out screencap` — encodes a full-resolution frame.
    Screencap,
    /// `install` / `push` / `pull` / `uninstall` — moves a file over the wire, and an install also
    /// verifies it on device.
    Transfer,
    /// `am start -W`, which blocks until the activity is up. A first launch after an install pays
    /// for ART compiling and a cold page cache, so it is nothing like the tap it would otherwise
    /// share a budget with. `am` alone is NOT this: `am force-stop` runs during teardown, inside
    /// `glass_core::TEARDOWN_BUDGET`, and must keep the short deadline.
    Launch,
    /// Everything else: `input`, `dumpsys`, `devices`, `forward`, `getprop`, `pidof`.
    Shell,
}

impl AdbOp {
    /// The deadline for this kind of call, and — where a caller passes one to
    /// [`Adb::run_streams_until`] — the ceiling a call gets rather than the deadline itself.
    pub fn budget(self) -> Duration {
        match self {
            Self::Dump => Duration::from_secs(20),
            Self::Screencap => Duration::from_secs(15),
            Self::Transfer => Duration::from_secs(120),
            Self::Launch => Duration::from_secs(60),
            Self::Shell => Duration::from_secs(10),
        }
    }

    /// Classify an adb argv, so no call site has to pass its own budget and none can pick a longer
    /// one than its work needs. An unrecognized call is [`AdbOp::Shell`] — the SHORT budget, so a
    /// future caller that forgets to extend this fails fast rather than hanging for two minutes.
    ///
    /// Matched by POSITION, never by scanning the argv for a substring: arguments carry
    /// caller-supplied data — an APK path from `AppSpec.run`, a package name, text a `glass_type`
    /// sends — and a path like `~/uiautomator-samples/app.apk` would otherwise classify an install
    /// as a dump and kill it at 20s instead of 120s, blaming the wrong operation as it went.
    ///
    /// `args` is what the caller passed, before [`build_argv`] prefixes `-s <serial>`, so `args[0]`
    /// is always the subcommand.
    pub fn for_args(args: &[&str]) -> Self {
        match (args.first().copied(), args.get(1).copied()) {
            (Some("install" | "install-multiple" | "push" | "pull" | "uninstall"), _) => {
                Self::Transfer
            }
            (Some("shell"), Some("uiautomator" | "cat")) => Self::Dump,
            (Some("exec-out" | "shell"), Some("screencap")) => Self::Screencap,
            (Some("shell"), Some("am")) if args.get(2) == Some(&"start") => Self::Launch,
            _ => Self::Shell,
        }
    }

    /// Operation name for the timeout error, prefixed so a reader knows which tool hung.
    fn label(self) -> &'static str {
        match self {
            Self::Dump => "adb:uiautomator dump",
            Self::Screencap => "adb:screencap",
            Self::Transfer => "adb:file transfer",
            Self::Launch => "adb:am start",
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
        let out = self.output(args, None)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run adb capturing stdout *and* stderr, as one step of a sequence that answers as a whole.
    ///
    /// Two streams because a command whose exit status does not signal success — `uiautomator dump`
    /// exits 0 when it fails, and explains itself on stderr — leaves the caller to judge the outcome
    /// from the streams itself. The call gets its operation's budget or what is left of `deadline`,
    /// whichever is nearer, so the steps of one dump cannot together cost the sum of their
    /// budgets.
    pub fn run_streams_until<'a, I>(&self, args: I, deadline: Instant) -> Result<(String, String)>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let out = self.output(args, Some(deadline))?;
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
        Ok(self.output(args, None)?.stdout)
    }

    /// Run adb and return the completed process, erroring on spawn failure or non-zero
    /// exit so every caller reports those two the same way. `deadline` is the outer bound of a
    /// multi-call sequence, where there is one.
    fn output<'a, I>(&self, args: I, deadline: Option<Instant>) -> Result<Output>
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
        let out = match deadline {
            Some(d) => run_bounded_until(&mut cmd, op.budget(), d, op.label()),
            None => run_bounded(&mut cmd, op.budget(), op.label()),
        }
        .map_err(with_adb_hint)?;
        if out.status.success() {
            Ok(out)
        } else {
            let said = String::from_utf8_lossy(&out.stderr);
            Err(exit_error(&self.bin, &argv, &said))
        }
    }
}

/// Add adb's one-command remedy to a TIMEOUT, and nothing else.
///
/// Appended in place rather than wrapped: nesting one error inside another makes `Display` print
/// `"backend error: "` twice, and an agent reads this text. In place also keeps the [`BoundKind`],
/// which `a11y::bound_fired` reads downstream — a rebuild that dropped it would report every
/// hint-carrying timeout as a device that failed.
fn with_adb_hint(mut e: GlassError) -> GlassError {
    if let GlassError::Bounded { kind, message, .. } = &mut e {
        match kind {
            BoundKind::TimedOut => {
                message.push_str("; if this repeats, run `adb kill-server` and retry");
            }
            // adb was never asked, so nothing about the server is implicated.
            BoundKind::NotStarted => {}
        }
    }
    e
}

/// A timeout as `glass_core` really raises one for an adb call, before [`with_adb_hint`].
///
/// Run rather than typed: every caller keys on a bound `glass-core` sets, so a fixture that set it
/// too could not notice the runner stopping (glass#348).
///
/// `/bin/sh` stands in for adb; `ping` does on Windows, which has no `/bin/sh` — it paces one echo
/// per second even when transmission fails, so it outlives the 200ms budget either way.
#[cfg(test)]
pub(crate) fn a_real_timeout() -> GlassError {
    #[cfg(unix)]
    let (bin, args): (&str, &[&str]) = ("/bin/sh", &["-c", "sleep 30"]);
    #[cfg(windows)]
    let (bin, args): (&str, &[&str]) = ("ping", &["-n", "31", "127.0.0.1"]);

    let e = run_bounded(
        Command::new(bin).args(args),
        Duration::from_millis(200),
        "adb:shell",
    )
    .expect_err("must time out");
    // A spawn failure is also an `Err`: it sails past `expect_err`, then fails whatever the caller
    // asserts next, blaming the code under test for a missing binary.
    assert_eq!(
        e.bound(),
        Some(BoundKind::TimedOut),
        "expected a timeout, got: {e}"
    );
    e
}

/// [`a_real_timeout`] as a caller outside this module meets one: every adb error leaves
/// [`Adb::output`] through [`with_adb_hint`], and `a11y`'s classifier reads what comes out.
#[cfg(test)]
pub(crate) fn a_real_timeout_hinted() -> GlassError {
    with_adb_hint(a_real_timeout())
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

/// The error for a call that ran and exited non-zero. `said` is what it wrote to stderr.
fn exit_error(bin: &str, argv: &[String], said: &str) -> GlassError {
    GlassError::ToolFailed {
        call: format!("{bin} {}", argv.join(" ")),
        said: said.trim().to_string(),
    }
}

/// [`exit_error`] as production builds it, for a test that needs a chosen `said` — the whole
/// question `a11y::died_unexplained` asks is whether that is empty.
#[cfg(test)]
pub(crate) fn a_failed_call(argv: &[&str], said: &str) -> GlassError {
    let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
    exit_error("adb", &argv, said)
}

/// What the fake adb does for one invocation it matches.
#[cfg(all(test, unix))]
pub(crate) enum Answer {
    /// Exit 0, writing these bytes on stdout.
    Says(Vec<u8>),
    /// Exit non-zero, writing these bytes on stderr — what [`Adb::output`] turns into
    /// `ToolFailed`, and the stream `a11y` reads a device's reason from.
    Fails(Vec<u8>),
    /// Exit 0 having said nothing, like `am force-stop` on success.
    Silent,
    /// Stay up rather than returning, like the backgrounded `adb shell` that holds the on-device
    /// agent open. Writes its pid to `linger.pid` so a test can watch for it being killed.
    Lingers,
}

#[cfg(all(test, unix))]
impl Answer {
    pub(crate) fn says(s: impl AsRef<[u8]>) -> Answer {
        Answer::Says(s.as_ref().to_vec())
    }
    pub(crate) fn fails(s: impl AsRef<[u8]>) -> Answer {
        Answer::Fails(s.as_ref().to_vec())
    }
}

/// An `adb` that is a shell script rather than the real tool: it answers the first rule whose
/// glob matches the argv and records every invocation.
///
/// The seam is the one production already has — [`Adb`] runs whatever `bin` names — so nothing is
/// stubbed out and the argv under test is the argv that would reach a device. What a call *did*
/// is otherwise invisible: a backend method replaced by `Ok(Default::default())` returns the same
/// shape as one that ran, and only the reply it parsed or the call it made tells them apart.
///
/// Unix-only: it is a `/bin/sh` script. The mutation gate and the coverage run are Linux, and
/// `adb.rs` already gates its process-level tests this way.
#[cfg(all(test, unix))]
pub(crate) struct FakeAdb {
    dir: std::path::PathBuf,
    adb: Adb,
}

/// Each answer lives in files rather than inline in the rules line: `adb devices` prints
/// tab-separated rows and `exec-out screencap` prints raw bytes, and a tab-separated line can
/// express neither — POSIX `read` folds repeated tabs together, and `printf %b` has no way to
/// write an arbitrary byte.
#[cfg(all(test, unix))]
const FAKE_ADB_SCRIPT: &str = "\
#!/bin/sh
# Stand-in for the adb binary; see FakeAdb in adb.rs. Records the argv, then answers the first
# rule whose glob matches it, stepping through that rule's answers and repeating the last. An
# unmatched call exits 0 saying nothing.
dir=$(dirname \"$0\")
printf '%s\\n' \"$*\" >> \"$dir/calls\"
while IFS='\t' read -r glob answers stem; do
    case \"$*\" in
        $glob)
            n=0
            [ -f \"$dir/$stem.seen\" ] && n=$(cat \"$dir/$stem.seen\")
            printf '%s' \"$((n + 1))\" > \"$dir/$stem.seen\"
            [ \"$n\" -ge \"$answers\" ] && n=$((answers - 1))
            code=$(cat \"$dir/${stem}_$n.code\")
            if [ \"$code\" = linger ]; then
                printf '%s' \"$$\" > \"$dir/linger.pid\"
                sleep 30
                exit 0
            fi
            cat \"$dir/${stem}_$n.out\"
            cat \"$dir/${stem}_$n.err\" >&2
            exit \"$code\"
            ;;
    esac
done < \"$dir/rules\"
exit 0
";

#[cfg(all(test, unix))]
impl FakeAdb {
    /// Serve `rules` — an argv glob and what to answer — tried in order.
    ///
    /// The glob is matched against the whole argv joined by spaces, as adb received it: that is
    /// *after* `-s <serial>` is prefixed, so a rule for a serial-bound call has to allow for it.
    pub(crate) fn new(rules: &[(&str, Answer)]) -> FakeAdb {
        let scripted: Vec<(&str, Vec<&Answer>)> =
            rules.iter().map(|(g, a)| (*g, vec![a])).collect();
        FakeAdb::scripted(&scripted)
    }

    /// [`FakeAdb::new`], but each rule steps through a sequence: the k'th call matching that glob
    /// gets the k'th answer, and the last answer repeats.
    ///
    /// A device changes under the caller — an emulator that was not in `adb devices` a moment ago
    /// is there now — and a fake that answers the same thing forever can only model a device that
    /// never does.
    pub(crate) fn scripted(rules: &[(&str, Vec<&Answer>)]) -> FakeAdb {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let dir = std::env::temp_dir().join(format!(
            "glass-fake-adb-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create the fake adb's directory");

        let mut rendered = String::new();
        for (i, (glob, answers)) in rules.iter().enumerate() {
            assert!(!answers.is_empty(), "rule {glob:?} answers nothing");
            let stem = format!("r{i}");
            for (k, answer) in answers.iter().enumerate() {
                let (code, out, err): (&str, &[u8], &[u8]) = match answer {
                    Answer::Says(s) => ("0", s, b""),
                    Answer::Fails(s) => ("1", b"", s),
                    Answer::Silent => ("0", b"", b""),
                    Answer::Lingers => ("linger", b"", b""),
                };
                let at = |ext| dir.join(format!("{stem}_{k}.{ext}"));
                std::fs::write(at("out"), out).expect("write the fake adb's stdout");
                std::fs::write(at("err"), err).expect("write the fake adb's stderr");
                std::fs::write(at("code"), code).expect("write the fake adb's status");
            }
            rendered.push_str(&format!("{glob}\t{}\t{stem}\n", answers.len()));
        }
        std::fs::write(dir.join("rules"), rendered).expect("write the fake adb's rules");

        let bin = write_executable(&dir, "adb", FAKE_ADB_SCRIPT);

        FakeAdb {
            adb: Adb {
                bin: bin.to_string_lossy().into_owned(),
                serial: None,
            },
            dir,
        }
    }

    pub(crate) fn adb(&self) -> &Adb {
        &self.adb
    }

    /// The argv of every invocation so far, in order, joined by spaces.
    pub(crate) fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Whether any invocation's argv contains `needle`.
    pub(crate) fn called(&self, needle: &str) -> bool {
        self.calls().iter().any(|c| c.contains(needle))
    }

    /// Write another executable script into this fake's directory and return its path — an
    /// `emulator` to stand beside the `adb`, cleaned up with it.
    pub(crate) fn alongside(&self, name: &str, script: &str) -> std::path::PathBuf {
        write_executable(&self.dir, name, script)
    }

    /// A file in this fake's directory, for a stand-in tool that records what it was asked.
    pub(crate) fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }
}

/// Write `script` into `dir` as an executable `name`, and return its path.
///
/// Written under a temporary name and renamed into place, never opened for writing at its final
/// path: these tests run in parallel, and a `Command::spawn` on one thread forks while another is
/// mid-write, so the child inherits the write handle. Executing a file some process holds open
/// for writing is `ETXTBSY`, which surfaces as an unrelated test failing to run its adb at all.
#[cfg(all(test, unix))]
fn write_executable(dir: &std::path::Path, name: &str, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let staged = dir.join(format!("{name}.staged"));
    let path = dir.join(name);
    std::fs::write(&staged, script).expect("write the stand-in tool");
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in tool executable");
    std::fs::rename(&staged, &path).expect("move the stand-in tool into place");
    path
}

#[cfg(all(test, unix))]
impl Drop for FakeAdb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A stand-in for the `emulator` binary, to sit beside a [`FakeAdb`] via
/// [`FakeAdb::alongside`]. `-list-avds` prints the contents of an `avds` file in the same
/// directory; anything else records its argv in `boots` and then stays up, which is what a boot
/// waits on.
#[cfg(all(test, unix))]
pub(crate) const FAKE_EMULATOR_SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
if [ "$1" = "-list-avds" ]; then
    cat "$dir/avds"
    exit 0
fi
printf '%s\n' "$*" >> "$dir/boots"
sleep 30
"#;

/// Whether `pid` is still a live process. A child that was killed *and reaped* is gone rather
/// than left as a zombie, which would still answer here.
#[cfg(all(test, unix))]
pub(crate) fn still_running(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        // A gone process is the answer this asks for, not a problem to report on stderr.
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill -0")
        .success()
}

/// The error a call to a *missing* binary raises — the shape `a11y` must not read as a tool that
/// ran and said nothing.
#[cfg(test)]
pub(crate) fn a_real_spawn_failure() -> GlassError {
    let adb = Adb {
        bin: "/nonexistent/glass-test-adb".to_string(),
        serial: None,
    };
    let e = adb
        .run(["devices"])
        .expect_err("a missing binary cannot run");
    assert_eq!(e.bound(), None, "a missing binary is not a bound: {e}");
    assert_eq!(
        e.tool_said(),
        None,
        "a tool that never ran said nothing: {e}"
    );
    e
}

#[cfg(test)]
mod tests {
    use super::{Adb, AdbOp, a_failed_call, a_real_timeout, build_argv, with_adb_hint};
    use glass_core::{BoundKind, GlassError};
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
        let adb = Adb::from_env();
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
    #[cfg(unix)]
    fn a_deadline_handed_to_adb_reaches_the_process_that_runs() {
        // The wiring the fix rests on: `output` must pass the deadline to the bounded runner, not
        // merely accept one. `/bin/sh` stands in for adb, and `AdbOp::Shell`'s 10s budget is what
        // this call would otherwise wait out.
        let adb = Adb {
            bin: "/bin/sh".to_string(),
            serial: None,
        };
        let started = std::time::Instant::now();
        let err = adb
            .run_streams_until(
                ["-c", "sleep 30"],
                std::time::Instant::now() + Duration::from_millis(300),
            )
            .expect_err("a step must not outlive the deadline it serves");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} — the deadline never reached the process",
            started.elapsed()
        );
        assert_eq!(err.bound(), Some(BoundKind::TimedOut), "{err}");
    }

    /// Which stream `said` comes from, proven against a real non-zero exit rather than asserted in
    /// a doc comment.
    ///
    /// `Adb::output` picks the stream and nothing else exercises that branch: passing `out.stdout`
    /// leaves the workspace green while a dead emulator, which writes its reason to stderr, reads
    /// as the silent crash `a11y` retries.
    ///
    /// `/bin/sh` stands in for adb; `cmd` does on Windows.
    #[test]
    fn a_tool_that_exits_non_zero_carries_its_stderr_and_not_its_stdout() {
        #[cfg(unix)]
        let (bin, loud): (&str, &[&str]) = (
            "/bin/sh",
            &["-c", "printf not-this; printf boom 1>&2; exit 1"],
        );
        #[cfg(windows)]
        let (bin, loud): (&str, &[&str]) =
            ("cmd", &["/c", "echo not-this& echo boom 1>&2& exit 1"]);
        let adb = Adb {
            bin: bin.to_string(),
            serial: None,
        };

        let e = adb
            .run(loud.iter().copied())
            .expect_err("a non-zero exit is a failure");
        // Only `said` discriminates: the message also carries the argv, and the argv here is the
        // shell command that prints the stdout text, so searching the message finds it either way.
        assert_eq!(
            e.tool_said(),
            Some("boom"),
            "stdout was read as the reason: {e}"
        );
    }

    /// A real silent exit is the crash `a11y::died_unexplained` names — the one fact this whole
    /// classification turns on, and the one a typed fixture cannot prove.
    #[test]
    fn a_tool_that_exits_non_zero_saying_nothing_reads_as_having_said_nothing() {
        #[cfg(unix)]
        let (bin, quiet): (&str, &[&str]) = ("/bin/sh", &["-c", "exit 9"]);
        #[cfg(windows)]
        let (bin, quiet): (&str, &[&str]) = ("cmd", &["/c", "exit 9"]);
        let adb = Adb {
            bin: bin.to_string(),
            serial: None,
        };

        let e = adb
            .run(quiet.iter().copied())
            .expect_err("a non-zero exit is a failure");
        assert_eq!(e.tool_said(), Some(""), "{e}");
        assert_eq!(e.bound(), None, "{e}");
    }

    /// The fixture the `a11y` tests build their device failures from renders what production does.
    #[test]
    fn the_failed_call_fixture_renders_what_a_real_exit_error_does() {
        assert_eq!(
            a_failed_call(
                &["shell", "cat", "/sdcard/x.xml"],
                "cat: /sdcard/x.xml: No such file"
            )
            .to_string(),
            "backend error: `adb shell cat /sdcard/x.xml` failed: cat: /sdcard/x.xml: No such file"
        );
    }

    #[test]
    fn a_dump_gets_the_longest_budget_of_the_interactive_calls() {
        // Ordering is the invariant here; the values themselves are calibrated on device.
        assert!(AdbOp::Dump.budget() > AdbOp::Shell.budget());
        assert!(AdbOp::Transfer.budget() > AdbOp::Dump.budget());
        assert!(AdbOp::Shell.budget() >= Duration::from_secs(10));
    }

    #[test]
    fn every_argv_this_crate_actually_runs_classifies_as_intended() {
        // The real argvs, taken from the call sites, so a refactor that renames a subcommand is
        // caught here rather than by a call silently dropping to the short budget. Paths are
        // illustrative — classification reads only the first two arguments.
        for (argv, want) in [
            (
                vec![
                    "shell",
                    "uiautomator",
                    "dump",
                    "/sdcard/glass_dump_1234_9_0.xml",
                ],
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
            (
                vec!["shell", "cat", "/sdcard/glass_dump_1234_9_0.xml"],
                AdbOp::Dump,
            ),
            (vec!["shell", "input", "tap", "10", "20"], AdbOp::Shell),
            (
                vec!["shell", "am", "start", "-W", "-n", "pkg/.Act"],
                AdbOp::Launch,
            ),
            (
                vec!["shell", "rm", "-f", "/sdcard/glass_dump_1_2_0.xml"],
                AdbOp::Shell,
            ),
            // `am` alone is not a launch: force-stop runs during teardown, inside
            // TEARDOWN_BUDGET, and a 60s deadline there would outlast the budget that calls it.
            (
                vec!["shell", "am", "force-stop", "com.example.app"],
                AdbOp::Shell,
            ),
            (vec!["shell", "pidof", "com.example.app"], AdbOp::Shell),
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
    fn caller_supplied_data_cannot_choose_a_budget() {
        // Each of these carries a word the classifier used to scan for anywhere in the argv. The
        // install pair is the harmful direction: a 120s transfer cut to 20s and killed mid-flight,
        // blamed on `adb:uiautomator dump`. The rest merely lengthened a tap's deadline.
        for (argv, want) in [
            (
                vec!["install", "-r", "-t", "/home/u/uiautomator-samples/app.apk"],
                AdbOp::Transfer,
            ),
            (
                vec![
                    "push",
                    "/tmp/screencap-tools/agent.jar",
                    "/data/local/tmp/agent.jar",
                ],
                AdbOp::Transfer,
            ),
            (vec!["shell", "input text 'uiautomator'"], AdbOp::Shell),
            (
                vec!["shell", "pidof", "com.example.uiautomator"],
                AdbOp::Shell,
            ),
        ] {
            assert_eq!(AdbOp::for_args(&argv), want, "argv {argv:?}");
        }
    }

    #[test]
    fn only_a_timeout_gets_the_kill_server_remedy() {
        // The spawn case must stay bare: `with_adb_hint`'s doc says why.
        let spawn = with_adb_hint(GlassError::Backend(
            "adb:shell: failed to start: No such file or directory (os error 2)".into(),
        ));
        assert!(!spawn.to_string().contains("kill-server"), "{spawn}");

        let timeout = with_adb_hint(a_real_timeout());
        assert!(timeout.to_string().contains("adb kill-server"), "{timeout}");
        // Display prints its prefix per layer, and "backend error: backend error: ..." is what
        // an agent would read.
        assert_eq!(timeout.to_string().matches("backend error").count(), 1);
        assert_eq!(timeout.bound(), Some(BoundKind::TimedOut), "{timeout}");
    }

    #[test]
    fn every_operation_names_the_tool_that_hung() {
        // A timeout message carries nothing else about which call stalled, and every one of
        // these deadlines exists because something did.
        let labelled = [
            (AdbOp::Dump, "adb:uiautomator dump"),
            (AdbOp::Screencap, "adb:screencap"),
            (AdbOp::Transfer, "adb:file transfer"),
            (AdbOp::Launch, "adb:am start"),
            (AdbOp::Shell, "adb:shell"),
        ];
        for (op, want) in labelled {
            assert_eq!(op.label(), want, "{op:?}");
        }
        // Distinct, so the message tells the operations apart rather than merely naming adb.
        let mut labels: Vec<&str> = labelled.iter().map(|(_, l)| *l).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), labelled.len());
    }

    #[test]
    fn a_launch_is_not_billed_as_a_tap() {
        // Ordering only; the variant doc explains why a launch is not a tap.
        assert!(AdbOp::Launch.budget() > AdbOp::Shell.budget());
    }

    #[test]
    fn an_unrecognized_call_takes_the_short_budget_not_a_generous_one() {
        // Fail fast beats hanging for two minutes when a future caller forgets to extend the
        // classifier.
        assert_eq!(AdbOp::for_args(&["some-future-subcommand"]), AdbOp::Shell);
    }

    /// The fixture itself: a rule's reply must reach the caller and the argv must reach the log,
    /// or every test built on it would pass against a fake that does nothing.
    #[test]
    #[cfg(unix)]
    fn the_fake_adb_answers_its_rules_and_records_what_it_was_asked() {
        use super::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[
            (
                "devices",
                Answer::says("List of devices attached\nemulator-5554\tdevice\n"),
            ),
            ("shell getprop *", Answer::says("1\n")),
            ("shell am force-stop *", Answer::Silent),
            ("install *", Answer::fails("adb: failed to install")),
        ]);
        let adb = fake.adb();

        // Tab-separated rows survive the rules file, which is itself tab-separated.
        assert_eq!(
            adb.run(["devices"]).unwrap(),
            "List of devices attached\nemulator-5554\tdevice\n"
        );
        assert_eq!(
            adb.run(["shell", "getprop", "sys.boot_completed"]).unwrap(),
            "1\n"
        );
        assert_eq!(adb.run(["shell", "am", "force-stop", "com.x"]).unwrap(), "");

        let err = adb
            .run(["install", "-r", "/tmp/app.apk"])
            .expect_err("a non-zero exit is a failure");
        assert_eq!(err.tool_said(), Some("adb: failed to install"), "{err}");

        assert_eq!(
            fake.calls(),
            [
                "devices",
                "shell getprop sys.boot_completed",
                "shell am force-stop com.x",
                "install -r /tmp/app.apk",
            ]
        );
    }

    /// The scripted form, which everything modelling a device that changes rests on.
    #[test]
    #[cfg(unix)]
    fn a_scripted_rule_steps_through_its_answers_and_then_repeats_the_last() {
        use super::{Answer, FakeAdb};

        let (empty, one) = (Answer::says("none\n"), Answer::says("one\n"));
        let fake = FakeAdb::scripted(&[("devices", vec![&empty, &one])]);
        let adb = fake.adb();

        assert_eq!(adb.run(["devices"]).unwrap(), "none\n");
        assert_eq!(adb.run(["devices"]).unwrap(), "one\n");
        assert_eq!(
            adb.run(["devices"]).unwrap(),
            "one\n",
            "the last answer repeats"
        );
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    #[cfg(unix)]
    fn a_serial_bound_client_names_its_device_on_every_call() {
        use super::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[("*", Answer::says("ok\n"))]);
        let adb = fake.adb().with_serial("emulator-5556");

        assert_eq!(adb.serial(), Some("emulator-5556"));
        adb.run(["shell", "input", "tap", "1", "2"]).unwrap();

        // `bin()` is for callers that spawn their own adb process — `LogcatStream` does — so
        // what it owes them is a runnable binary, not merely a string.
        let spawned = std::process::Command::new(adb.bin())
            .arg("version")
            .status()
            .expect("bin() must name something that runs");
        assert!(spawned.success());

        // The serial reaches the wire, not just the struct: an accessor that answered `None`
        // would send every call to whichever device adb picked for itself.
        assert!(
            fake.called("-s emulator-5556 shell input tap 1 2"),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn raw_bytes_come_back_exactly_as_the_device_wrote_them() {
        use super::{Answer, FakeAdb};

        // A screencap header: little-endian 1x1, format 1, then four pixel bytes. Raw bytes
        // rather than text, because that is what `exec-out screencap` returns — including the
        // zeroes and the high bytes no string payload would survive.
        let mut frame = Vec::new();
        frame.extend_from_slice(&1u32.to_le_bytes()); // width
        frame.extend_from_slice(&1u32.to_le_bytes()); // height
        frame.extend_from_slice(&1u32.to_le_bytes()); // RGBA_8888
        frame.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let fake = FakeAdb::new(&[("exec-out screencap", Answer::says(&frame))]);

        let bytes = fake.adb().run_bytes(["exec-out", "screencap"]).unwrap();
        assert_eq!(bytes.len(), 16, "got {bytes:?}");
        assert_eq!(&bytes[0..4], &1u32.to_le_bytes());
        assert_eq!(&bytes[12..], &[0xde, 0xad, 0xbe, 0xef]);
    }

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
