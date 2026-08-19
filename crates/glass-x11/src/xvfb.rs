//! A private headless `Xvfb` the X11 backend spawns when no display is given,
//! so the default path is isolated and never touches the user's real desktop.
//! Uses `-displayfd`: the server picks a free display and reports it once ready,
//! avoiding display-number and readiness races.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use glass_core::{GlassError, Result};
use glass_proc_linux::StderrTail;

/// How long to wait for Xvfb to report its display before treating it as wedged.
/// Readiness is normally well under a second; this ceiling is generous so a
/// slow/loaded host isn't falsely failed, while a hung Xvfb can't block start-up.
/// A wedge gets one retry (see `start_binary`), so the worst still-failing start
/// is two of these plus two reap graces — about 24s; see `start_deadline`.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on how long `Xvfb::start` can take before it returns (both
/// attempts wedge, each reaped and each collected): callers that put their own
/// timeout around a start (doctor's deep probe) must budget at least this or
/// they'll misreport a start that would have succeeded on the retry.
///
/// Excludes the reader's own backstop, which only runs if a wakeup is lost.
pub(crate) fn start_deadline() -> Duration {
    2 * (READY_TIMEOUT + glass_proc_linux::REAP_GRACE + SAID_GRACE)
}

/// How long a failed start waits for Xvfb's stderr pipe to close before rendering what it has.
///
/// The server is already reaped, so the pipe EOFs within moments — unless the server left
/// something holding it, and then this is what the message costs instead of everything.
const SAID_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct Xvfb {
    child: Child,
    /// The chosen display, formatted `:N`.
    pub display: String,
    // Held open for the server's lifetime so Xvfb never gets SIGPIPE on the fd.
    #[expect(
        dead_code,
        reason = "RAII: held open for the server's lifetime so the fd never SIGPIPEs"
    )]
    displayfd: ChildStdout,
    /// Kept draining for the server's lifetime — a chatty server must never stall on a full
    /// pipe. Fields drop after `Drop::drop` returns, so the reader ends only once the server
    /// has been reaped.
    #[expect(
        dead_code,
        reason = "RAII: drained for the server's lifetime, and its reader ends when this drops"
    )]
    stderr: StderrTail,
}

impl Xvfb {
    /// Spawn a private Xvfb on a server-chosen free display, returning once it is
    /// ready. `screen` is a `WxHxDepth` string (e.g. `"1280x800x24"`).
    pub fn start(screen: &str) -> Result<Xvfb> {
        let xvfb = glass_core::tool_path("GLASS_XVFB", "Xvfb");
        start_binary(&xvfb, screen, READY_TIMEOUT)
    }

    /// The server process's pid. Exposed so a test can put the display itself into a state glass
    /// has to survive — an X server that is running but not answering, which is otherwise
    /// impossible to produce from the client side.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// One spawn attempt's failure, carrying whatever the server wrote to stderr —
/// the only diagnostics a failed Xvfb offers.
enum StartErr {
    /// `exec` itself failed — the binary is missing/not runnable.
    Spawn(String),
    /// The server started, but the host refused something the stderr reader is built from —
    /// the thread, an fd. Not `Spawn`: nothing is wrong with the binary, so its remedy would
    /// misdirect.
    NoReader(String),
    /// Xvfb exited before reporting a display.
    Exited { stderr: String },
    /// A line arrived on `-displayfd` but wasn't a display number.
    Garbage { line: String, stderr: String },
    /// Alive but silent past the deadline.
    Wedged { stderr: String },
}

/// Spawn `xvfb` and wait for its `-displayfd` report. A wedge (spawned but
/// silent past `ready_timeout`) gets ONE retry against a fresh server — it's the
/// transient failure class (seen under heavy host load), and on a user's first
/// run a single quiet retry is the difference between working and giving up.
/// Exit/garbage failures are deterministic (bad binary/args/env); retrying those
/// would only double the time to the same error. A refused reader is not retried
/// either: the host is out of a resource, and a second server would ask for more.
fn start_binary(xvfb: &str, screen: &str, ready_timeout: Duration) -> Result<Xvfb> {
    match start_once(xvfb, screen, ready_timeout) {
        Ok(x) => Ok(x),
        Err(StartErr::Wedged { .. }) => {
            eprintln!(
                "glass: Xvfb did not report a display within {}s; \
                 killing it and retrying once with a fresh server",
                ready_timeout.as_secs()
            );
            start_once(xvfb, screen, ready_timeout)
                .map_err(|e| into_glass_error(xvfb, e, ready_timeout))
        }
        Err(e) => Err(into_glass_error(xvfb, e, ready_timeout)),
    }
}

/// A single spawn-and-wait attempt. On failure the child is reaped before
/// returning, so a retry never overlaps a dying server.
fn start_once(
    xvfb: &str,
    screen: &str,
    ready_timeout: Duration,
) -> std::result::Result<Xvfb, StartErr> {
    let mut child = Command::new(xvfb)
        .args(["-displayfd", "1", "-screen", "0", screen])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| StartErr::Spawn(e.to_string()))?;

    let stderr_tail = match StderrTail::drain(child.stderr.take().expect("piped stderr")) {
        Ok(tail) => tail,
        // `drain` consumed the pipe, so the read end is already closed and the server's next
        // complaint takes SIGPIPE — reap it rather than leave a display glass cannot hear.
        Err(e) => {
            glass_proc_linux::reap_graceful(&mut child, glass_proc_linux::REAP_GRACE);
            return Err(StartErr::NoReader(e.to_string()));
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    match read_displayfd(stdout, ready_timeout) {
        Ok((num, displayfd)) => Ok(Xvfb {
            child,
            display: format!(":{num}"),
            displayfd,
            stderr: stderr_tail,
        }),
        Err(e) => {
            glass_proc_linux::reap_graceful(&mut child, glass_proc_linux::REAP_GRACE);
            // Reaped first, so what is left is the reader catching up: collecting before that
            // races it and reports an empty stderr, the only diagnostics a failed start has.
            let stderr = stderr_tail.finish(SAID_GRACE).trim().to_string();
            Err(match e {
                ReadErr::Closed => StartErr::Exited { stderr },
                ReadErr::Garbage(line) => StartErr::Garbage { line, stderr },
                ReadErr::TimedOut => StartErr::Wedged { stderr },
            })
        }
    }
}

/// Render a final (post-retry) failure as a user-facing error that names the
/// recovery and carries the server's stderr.
fn into_glass_error(xvfb: &str, e: StartErr, ready_timeout: Duration) -> GlassError {
    let msg = match e {
        StartErr::Spawn(e) => format!(
            "could not spawn {xvfb} ({e}); install it (e.g. `apt install xvfb`), \
             set GLASS_XVFB to its path, or set GLASS_DISPLAY=:N to attach to an \
             existing display"
        ),
        StartErr::NoReader(e) => format!(
            "started {xvfb} but could not set up the reader for its stderr ({e}); the server \
             was stopped rather than left to stall on a pipe nobody drains — free up threads \
             and file descriptors on the host, or set GLASS_DISPLAY=:N to attach to an \
             existing display"
        ),
        StartErr::Exited { stderr } => with_stderr(
            "Xvfb exited without reporting a display (failed to start); \
             set GLASS_DISPLAY=:N to attach to an existing display instead"
                .into(),
            &stderr,
        ),
        StartErr::Garbage { line, stderr } => with_stderr(
            format!("unexpected Xvfb -displayfd output: {line:?}"),
            &stderr,
        ),
        StartErr::Wedged { stderr } => with_stderr(
            format!(
                "Xvfb did not report a display within {}s, twice (the first server \
                 was killed and a fresh one retried); try again, set GLASS_DISPLAY=:N \
                 to attach to an existing display, or run `Xvfb -displayfd 1` \
                 manually to see why it stalls",
                ready_timeout.as_secs()
            ),
            &stderr,
        ),
    };
    GlassError::Backend(msg)
}

/// How much of the captured stderr to render into an error message. X servers
/// dump their whole option table (~5KB) after a config error, with the fatal
/// line FIRST — so the head is the useful part and the rest is disclosed as a
/// byte count rather than pasted into a one-line check/detail.
const STDERR_SHOWN: usize = 512;

fn with_stderr(msg: String, stderr: &str) -> String {
    if stderr.is_empty() {
        return format!("{msg} (nothing arrived on Xvfb's stderr)");
    }
    if stderr.len() <= STDERR_SHOWN {
        return format!("{msg}; Xvfb stderr: {stderr}");
    }
    let cut = stderr.floor_char_boundary(STDERR_SHOWN);
    format!(
        "{msg}; Xvfb stderr (first {cut} bytes of {}): {}…",
        stderr.len(),
        &stderr[..cut]
    )
}

/// Why reading the `-displayfd` line failed.
#[derive(Debug)]
enum ReadErr {
    /// The pipe closed before a line arrived — Xvfb exited (failed to start).
    Closed,
    /// A line arrived but wasn't a display number.
    Garbage(String),
    /// No line within the timeout — Xvfb spawned but never became ready.
    TimedOut,
}

/// Read the display number Xvfb writes to its `-displayfd` pipe, bounded by
/// `timeout`. The blocking `read_line` runs on a helper thread so a wedged Xvfb
/// (alive, stdout open, but never reporting) can't block the caller forever —
/// the original hang. On success the `ChildStdout` is handed back so the caller
/// can hold it open for Xvfb's lifetime (closing it would SIGPIPE the server).
fn read_displayfd(
    stdout: ChildStdout,
    timeout: Duration,
) -> std::result::Result<(u32, ChildStdout), ReadErr> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        // Hand the fd back so the caller keeps it open; ignore a send failure —
        // the caller timed out and dropped the receiver, the child will be
        // killed, and this read unblocks and drops the fd here.
        let _ = tx.send((n, line, reader.into_inner()));
    });
    match rx.recv_timeout(timeout) {
        Ok((0, _, _)) => Err(ReadErr::Closed),
        Ok((_, line, fd)) => match line.trim().parse::<u32>() {
            Ok(num) => Ok((num, fd)),
            Err(_) => Err(ReadErr::Garbage(line.trim().to_string())),
        },
        Err(_) => Err(ReadErr::TimedOut),
    }
}

impl Drop for Xvfb {
    /// Reaping is the whole teardown. Do not add back a sweep of `/tmp/.X11-unix/X{N}`: the
    /// next Xvfb connect-probes that path and rebinds a refusing one, so a SIGKILLed server's
    /// leftover costs nothing — while unlinking it cuts off whoever reclaimed the number.
    fn drop(&mut self) {
        glass_proc_linux::reap_graceful(&mut self.child, glass_proc_linux::REAP_GRACE);
        // `stderr` drops on the way out of here, ending its reader.
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadErr, Xvfb, read_displayfd, start_binary};
    use glass_core::{GlassError, Result};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// Call `start_binary` on a fixture script, retrying past a transient
    /// ETXTBSY: a sibling test thread's fork can momentarily hold the freshly
    /// written script's fd open, racing our exec (same rationale as the
    /// glass-ios companion tests). Resets the script's `$0.ran` marker before
    /// each attempt so a stateful fixture always re-runs from invocation one.
    fn start_fixture(script: &std::path::Path, timeout: Duration) -> Result<Xvfb> {
        let marker = format!("{}.ran", script.display());
        let mut last = None;
        for _ in 0..100 {
            let _ = std::fs::remove_file(&marker);
            match start_binary(script.to_str().unwrap(), "640x480x24", timeout) {
                Err(GlassError::Backend(m)) if m.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(10));
                    last = Some(m);
                }
                r => return r,
            }
        }
        panic!("ETXTBSY persisted after 100 retries: {last:?}")
    }

    #[test]
    fn the_start_deadline_covers_both_attempts_and_both_reaps() {
        // doctor's deep probe puts its own timeout around `Xvfb::start`. Budget less than
        // the wedge-retry ladder actually takes and it reports a start that the retry
        // would have completed as a failure.
        let one_attempt = super::READY_TIMEOUT + glass_proc_linux::REAP_GRACE;
        assert!(
            super::start_deadline() >= one_attempt + one_attempt,
            "a caller budgeting {:?} cannot outlast two wedged attempts",
            super::start_deadline()
        );
    }

    #[test]
    fn a_multibyte_char_straddling_the_clip_point_is_not_split() {
        // Xvfb's stderr is whatever the server prints, which is not guaranteed ASCII;
        // slicing mid-character would panic on the error path itself.
        let head = "a".repeat(super::STDERR_SHOWN - 1);
        let out = super::with_stderr("Xvfb failed".into(), &format!("{head}é{}", "b".repeat(600)));
        assert!(
            out.contains(&format!("first {} bytes", super::STDERR_SHOWN - 1)),
            "must clip back to the boundary before the two-byte char: {out}"
        );
    }

    #[test]
    fn the_reported_pid_is_the_server_itself() {
        // A test that SIGSTOPs this pid to make the display unresponsive stops something
        // else entirely if it is wrong.
        let script = fixture("pid.sh", "echo 4321\nexec sleep 30\n");
        let server = start_fixture(&script, Duration::from_secs(5)).expect("must start");
        // Its parent, not its command line: the fixture `exec`s, so what it is running
        // changes under it, while who spawned it does not.
        let status = std::fs::read_to_string(format!("/proc/{}/status", server.pid()))
            .expect("the reported pid must be a live process");
        let parent = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .map(|v| v.trim().to_string())
            .expect("every process reports a parent");
        assert_eq!(
            parent,
            std::process::id().to_string(),
            "the reported pid must be the server this test spawned, not another process"
        );
    }

    /// Kills a fixture's leftover process however the test ends, panic included.
    struct Survivor(u32);

    impl Drop for Survivor {
        fn drop(&mut self) {
            if let Some(pid) = rustix::process::Pid::from_raw(self.0 as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }

    #[test]
    fn a_server_that_complains_after_reporting_its_display_is_still_drained() {
        // The tail is held for the server's lifetime, not the start's. End the reader when the
        // start returns and the server blocks on a full 64KiB pipe or takes SIGPIPE on its next
        // complaint — a display that vanishes mid-session either way.
        let script = fixture(
            "chatty-after.sh",
            "echo 4321\n\
             dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' e >&2\n\
             echo 'still here' >&2\n\
             touch \"$0.flushed\"\n\
             exec sleep 30\n",
        );
        let flushed = format!("{}.flushed", script.display());
        let _ = std::fs::remove_file(&flushed);
        let server = start_fixture(&script, Duration::from_secs(5)).expect("must start");

        // One marker catches both failures: a closed read end kills the shell at the `echo`
        // before the `touch`, and an undrained pipe blocks `tr` after 64 KiB so it never runs.
        assert!(
            glass_proc_linux::await_condition(Duration::from_secs(10), || {
                std::path::Path::new(&flushed).exists()
            }),
            "1MiB of stderr written after the display report must not stall or kill the server"
        );
        assert!(running(server.pid()), "the server must survive complaining");
    }

    #[test]
    fn dropping_the_server_releases_a_stderr_pipe_its_survivor_holds_open() {
        // A reader that can only stop at EOF is held open by whatever the server left behind,
        // and doctor's deep probe starts a server per call (glass#471).
        let script = fixture(
            "survivor.sh",
            "sleep 30 &\necho $! > \"$0.survivor\"\necho 4321\nexec sleep 30\n",
        );
        let server = start_fixture(&script, Duration::from_secs(5)).expect("must start");
        let survivor: u32 = std::fs::read_to_string(format!("{}.survivor", script.display()))
            .expect("the fixture reports what it left running")
            .trim()
            .parse()
            .expect("a pid");
        let _reap = Survivor(survivor);
        // The survivor inherited the write end, so this names the one pipe — no other process
        // can hold it, and nothing else in this one can reopen it.
        let pipe = std::fs::read_link(format!("/proc/{survivor}/fd/2"))
            .expect("the survivor holds the server's stderr");

        drop(server);

        let held: Vec<_> = std::fs::read_dir("/proc/self/fd")
            .expect("/proc/self/fd")
            .filter_map(|e| e.ok())
            .filter(|e| std::fs::read_link(e.path()).is_ok_and(|target| target == pipe))
            .map(|e| e.file_name())
            .collect();
        assert!(
            held.is_empty(),
            "a reaped server must leave no reader on its stderr, but {pipe:?} is still open on {held:?}"
        );
    }

    fn running(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Write an executable fake-Xvfb shell script into a unique temp dir and
    /// return its path. `$0.ran` is the script's own scratch marker.
    fn fixture(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("glass-xvfb-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn a_refused_stderr_reader_is_not_reported_as_a_missing_binary() {
        // The server started; what failed was the host giving glass a thread. `Spawn`'s remedy
        // — install it, or point GLASS_XVFB somewhere else — would send the user after a
        // binary that is already there and working.
        let GlassError::Backend(msg) = super::into_glass_error(
            "/usr/bin/Xvfb",
            super::StartErr::NoReader("Resource temporarily unavailable".into()),
            Duration::from_secs(10),
        ) else {
            panic!("a start failure is a Backend error")
        };
        assert!(
            msg.contains("Resource temporarily unavailable"),
            "the host's own reason is the diagnosis: {msg}"
        );
        assert!(
            !msg.contains("install it"),
            "nothing is missing, so nothing needs installing: {msg}"
        );
    }

    #[test]
    fn dropping_the_server_reaps_it() {
        // Without this the display outlives the session that spawned it, and a run that
        // starts a few of them leaves an X server per launch behind.
        let script = fixture("reap.sh", "echo 4321\nexec sleep 30\n");
        let server = start_fixture(&script, Duration::from_secs(5)).expect("must start");
        let pid = server.pid();
        assert!(running(pid), "the fixture server should be up first");
        drop(server);
        assert!(!running(pid), "pid {pid} outlived the Xvfb that owned it");
    }

    #[test]
    fn stderr_render_keeps_the_head_and_says_how_much_was_clipped() {
        // Real Xvfb dumps its whole option table (~5KB) on a config error, with
        // the fatal line FIRST. The rendered error must keep the head and
        // disclose the clip, not paste kilobytes into a one-line check/detail.
        let fatal = "fatal: something specific went wrong";
        let noise = "usage noise line\n".repeat(200); // ~3.4KB
        let out = super::with_stderr("Xvfb failed".into(), &format!("{fatal}\n{noise}"));
        assert!(out.contains(fatal), "fatal first line kept: {out}");
        assert!(
            out.len() < 800,
            "rendered error stays bounded, got {} bytes",
            out.len()
        );
        assert!(
            out.contains("bytes"),
            "clip must be disclosed with sizes: {out}"
        );
    }

    #[test]
    fn short_stderr_renders_whole_without_clip_note() {
        let out = super::with_stderr("Xvfb failed".into(), "one useful line");
        assert!(out.contains("one useful line"), "{out}");
        assert!(
            !out.contains("bytes of"),
            "no clip note when nothing clipped: {out}"
        );
    }

    #[test]
    fn chatty_stderr_before_report_does_not_stall_startup() {
        // The drain thread must keep reading past what it keeps: a server writing
        // more than the 64KiB pipe buffer before reporting its display would
        // otherwise block on write() forever and turn every start into a wedge.
        let script = fixture(
            "chatty.sh",
            "dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' e >&2\n\
             echo 4321\n\
             exec sleep 30\n",
        );
        let t0 = std::time::Instant::now();
        let x = start_fixture(&script, Duration::from_secs(5)).expect("must start");
        assert_eq!(x.display, ":4321");
        assert!(
            t0.elapsed() < Duration::from_secs(4),
            "1MiB of stderr must not wedge the start (took {:?})",
            t0.elapsed()
        );
    }

    #[test]
    fn wedged_first_attempt_is_killed_and_retried_once() {
        // First invocation wedges (alive, silent); second reports a display.
        // A transient wedge must cost one retry, not the whole session.
        let script = fixture(
            "wedge-then-ok.sh",
            "if [ -e \"$0.ran\" ]; then echo 4321; exec sleep 30; fi\n\
             touch \"$0.ran\"\n\
             exec sleep 30\n",
        );
        let x = start_fixture(&script, Duration::from_millis(300))
            .expect("second attempt must succeed");
        assert_eq!(x.display, ":4321");
    }

    #[test]
    fn wedged_twice_error_names_recovery_and_includes_stderr() {
        // Both attempts wedge. The error must carry the server's stderr (the
        // only diagnostics it offers) and name a recovery, not internal
        // rationale.
        let script = fixture(
            "wedge-always.sh",
            "echo 'fixture stderr complaint' >&2\nexec sleep 30\n",
        );
        let err = start_fixture(&script, Duration::from_millis(200))
            .expect_err("must fail after the retry")
            .to_string();
        assert!(err.contains("did not report a display"), "msg: {err}");
        assert!(
            err.contains("retried"),
            "must say it already retried: {err}"
        );
        assert!(err.contains("GLASS_DISPLAY"), "must name a recovery: {err}");
        assert!(
            err.contains("fixture stderr complaint"),
            "must include Xvfb stderr: {err}"
        );
    }

    #[test]
    fn a_server_that_printed_only_whitespace_is_reported_as_silent() {
        // The trim is what makes the silent branch true for a server whose last word was a
        // newline; without it the error ends in an empty quote for the reader to interpret.
        let script = fixture("blank-stderr.sh", "printf '  \\n \\n' >&2\nexit 1\n");
        let err = start_fixture(&script, Duration::from_millis(500))
            .expect_err("exit must fail")
            .to_string();
        assert!(err.contains("nothing arrived"), "msg: {err}");
    }

    #[test]
    fn immediate_exit_is_not_retried_and_includes_stderr() {
        // Exit-without-display is deterministic (bad binary/args/env) — a retry
        // would only double the wait. The fixture would SUCCEED on a second
        // invocation, so a wrongly-added retry turns this Err into Ok.
        let script = fixture(
            "exit-then-ok.sh",
            "if [ -e \"$0.ran\" ]; then echo 4321; exec sleep 30; fi\n\
             touch \"$0.ran\"\n\
             echo 'exiting complaint' >&2\n\
             exit 1\n",
        );
        let err = start_fixture(&script, Duration::from_millis(500))
            .expect_err("exit must fail without retry")
            .to_string();
        assert!(err.contains("exited without reporting"), "msg: {err}");
        assert!(
            err.contains("exiting complaint"),
            "must include Xvfb stderr: {err}"
        );
    }

    #[test]
    fn read_displayfd_times_out_on_a_silent_child() {
        // A child that stays alive and never writes its display (the wedged-Xvfb
        // case) must NOT block forever — read_displayfd returns TimedOut.
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let stdout = child.stdout.take().expect("piped");
        let r = read_displayfd(stdout, Duration::from_millis(200));
        let _ = child.kill();
        let _ = child.wait();
        assert!(matches!(r, Err(ReadErr::TimedOut)), "expected TimedOut");
    }

    #[test]
    fn read_displayfd_parses_a_reported_display() {
        // Writes "7" then stays alive (Xvfb keeps fd 1 open after reporting).
        // `exec sleep` keeps the same pid so child.kill() reaps it (no orphan).
        let mut child = Command::new("sh")
            .args(["-c", "echo 7; exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let stdout = child.stdout.take().expect("piped");
        let r = read_displayfd(stdout, Duration::from_secs(5));
        let _ = child.kill();
        let _ = child.wait();
        match r {
            Ok((num, _fd)) => assert_eq!(num, 7),
            Err(e) => panic!("expected display 7, got {e:?}"),
        }
    }

    #[test]
    fn read_displayfd_reports_closed_on_immediate_exit() {
        // Exits without writing — the pipe closes (EOF) → Closed, not a hang.
        let mut child = Command::new("true")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn true");
        let stdout = child.stdout.take().expect("piped");
        let r = read_displayfd(stdout, Duration::from_secs(5));
        let _ = child.wait();
        assert!(matches!(r, Err(ReadErr::Closed)), "expected Closed");
    }
}
