//! A private headless `Xvfb` the X11 backend spawns when no display is given,
//! so the default path is isolated and never touches the user's real desktop.
//! Uses `-displayfd`: the server picks a free display and reports it once ready,
//! avoiding display-number and readiness races.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use glass_core::{GlassError, Result};

/// How long to wait for Xvfb to report its display before treating it as wedged.
/// Readiness is normally well under a second; this ceiling is generous so a
/// slow/loaded host isn't falsely failed, while a hung Xvfb can't block start-up.
/// A wedge gets one retry (see `start_binary`), so the worst still-failing start
/// is two of these plus two reap graces — about 24s; see `start_deadline`.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on how long `Xvfb::start` can take before it returns (both
/// attempts wedge, each reaped): callers that put their own timeout around a
/// start (doctor's deep probe) must budget at least this or they'll misreport
/// a start that would have succeeded on the retry.
pub(crate) fn start_deadline() -> Duration {
    2 * (READY_TIMEOUT + glass_proc_linux::REAP_GRACE)
}

/// How much of Xvfb's stderr to keep for error messages. Failures print early;
/// past the cap the pipe is still drained (see `StderrTail`) but bytes are dropped.
const STDERR_CAP: usize = 8 * 1024;

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
}

impl Xvfb {
    /// Spawn a private Xvfb on a server-chosen free display, returning once it is
    /// ready. `screen` is a `WxHxDepth` string (e.g. `"1280x800x24"`).
    pub fn start(screen: &str) -> Result<Xvfb> {
        let xvfb = glass_core::tool_path("GLASS_XVFB", "Xvfb");
        start_binary(&xvfb, screen, READY_TIMEOUT)
    }
}

/// One spawn attempt's failure, carrying whatever the server wrote to stderr —
/// the only diagnostics a failed Xvfb offers.
enum StartErr {
    /// `exec` itself failed — the binary is missing/not runnable.
    Spawn(String),
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
/// would only double the time to the same error.
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

    let stderr_tail = StderrTail::drain(child.stderr.take().expect("piped stderr"));
    let stdout = child.stdout.take().expect("piped stdout");
    match read_displayfd(stdout, ready_timeout) {
        Ok((num, displayfd)) => Ok(Xvfb {
            child,
            display: format!(":{num}"),
            displayfd,
        }),
        Err(e) => {
            glass_proc_linux::reap_graceful(&mut child, glass_proc_linux::REAP_GRACE);
            // The child is dead, so its stderr pipe has EOF'd (or will within
            // moments); wait briefly for the drain to finish so the message is
            // complete rather than racing the reader thread.
            let stderr = stderr_tail.snapshot(Duration::from_millis(500));
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
        return format!("{msg} (Xvfb printed nothing to stderr)");
    }
    if stderr.len() <= STDERR_SHOWN {
        return format!("{msg}; Xvfb stderr: {stderr}");
    }
    let mut cut = STDERR_SHOWN;
    while !stderr.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{msg}; Xvfb stderr (first {cut} bytes of {}): {}…",
        stderr.len(),
        &stderr[..cut]
    )
}

/// Drains a child's stderr on a helper thread for the child's whole lifetime
/// (so a chatty server can never stall on a full pipe), keeping the first
/// `STDERR_CAP` bytes for diagnostics.
struct StderrTail(Arc<TailState>);

struct TailState {
    buf: Mutex<TailBuf>,
    eof: Condvar,
}

struct TailBuf {
    bytes: Vec<u8>,
    done: bool,
}

impl StderrTail {
    fn drain(mut stderr: ChildStderr) -> StderrTail {
        let state = Arc::new(TailState {
            buf: Mutex::new(TailBuf {
                bytes: Vec::new(),
                done: false,
            }),
            eof: Condvar::new(),
        });
        let s = state.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = s.buf.lock().unwrap();
                        let room = STDERR_CAP.saturating_sub(g.bytes.len());
                        g.bytes.extend_from_slice(&chunk[..n.min(room)]);
                        // keep looping past the cap — the pipe must stay drained
                    }
                }
            }
            let mut g = s.buf.lock().unwrap();
            g.done = true;
            s.eof.notify_all();
        });
        StderrTail(state)
    }

    /// The captured stderr, waiting up to `timeout` for the pipe to EOF first
    /// (call after the child is reaped). Lossy UTF-8, trimmed.
    fn snapshot(&self, timeout: Duration) -> String {
        let g = self.0.buf.lock().unwrap();
        let (g, _) = self
            .0
            .eof
            .wait_timeout_while(g, timeout, |b| !b.done)
            .unwrap();
        String::from_utf8_lossy(&g.bytes).trim().to_string()
    }
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
    fn drop(&mut self) {
        glass_proc_linux::reap_graceful(&mut self.child, glass_proc_linux::REAP_GRACE);
        // Fallback: Xvfb removes its own lock/socket on SIGTERM, but if it had to
        // be SIGKILLed (ignored SIGTERM) they linger; clean them up.
        if let Some(num) = self.display.strip_prefix(':') {
            let _ = std::fs::remove_file(format!("/tmp/.X{num}-lock"));
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{num}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{read_displayfd, start_binary, ReadErr, Xvfb};
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
        // The drain thread must keep reading past STDERR_CAP: a server writing
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
