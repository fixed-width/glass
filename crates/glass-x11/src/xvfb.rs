//! A private headless `Xvfb` the X11 backend spawns when no display is given,
//! so the default path is isolated and never touches the user's real desktop.
//! Uses `-displayfd`: the server picks a free display and reports it once ready,
//! avoiding display-number and readiness races.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use glass_core::{GlassError, Result};

/// How long to wait for Xvfb to report its display before treating it as wedged.
/// Readiness is normally well under a second; this ceiling is generous so a
/// slow/loaded host isn't falsely failed, while a hung Xvfb can't block start-up.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

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
        let mut child = Command::new(&xvfb)
            .args(["-displayfd", "1", "-screen", "0", screen])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                GlassError::Backend(format!(
                    "could not spawn {xvfb} ({e}); install it (e.g. `apt install xvfb`), \
                     set GLASS_XVFB to its path, or set GLASS_DISPLAY=:N to attach to an \
                     existing display"
                ))
            })?;

        let stdout = child.stdout.take().expect("piped stdout");
        match read_displayfd(stdout, READY_TIMEOUT) {
            Ok((num, displayfd)) => Ok(Xvfb {
                child,
                display: format!(":{num}"),
                displayfd,
            }),
            Err(e) => {
                glass_proc_linux::reap_graceful(&mut child, glass_proc_linux::REAP_GRACE);
                Err(GlassError::Backend(match e {
                    ReadErr::Closed => {
                        "Xvfb exited without reporting a display (failed to start)".into()
                    }
                    ReadErr::Garbage(line) => {
                        format!("unexpected Xvfb -displayfd output: {line:?}")
                    }
                    ReadErr::TimedOut => format!(
                        "Xvfb spawned but did not report a display within {}s (wedged); \
                         not blocking start-up",
                        READY_TIMEOUT.as_secs()
                    ),
                }))
            }
        }
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
    use super::{read_displayfd, ReadErr};
    use std::process::{Command, Stdio};
    use std::time::Duration;

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
