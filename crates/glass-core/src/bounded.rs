//! Run a one-shot child process under a deadline.
//!
//! Every backend that drives an external tool (`adb`, `xcrun simctl`, `plutil`) needs the same
//! thing: the tool answers quickly, or it has hung and must not hang the agent's call with it.
//! `std::process` offers no wait-with-a-deadline, so this module supplies the one shape they all
//! use. A long-lived child — a log tail, an emulator, the on-device agent — is NOT this: those use
//! `Command::spawn` directly and are meant to outlive the call that started them.

use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{GlassError, Result};

/// Longest gap between checks on a child that has not exited yet. The wait starts far tighter and
/// backs off to this, so a one-shot that answers in a millisecond is not billed a full tick while
/// waiting out a three-minute boot still costs only a handful of wakeups.
const POLL: Duration = Duration::from_millis(20);

/// First gap between checks, doubled up to [`POLL`].
const POLL_START: Duration = Duration::from_millis(1);

/// How long to let a drain thread reach EOF after the child has exited. Normally instant — the pipe
/// closes with the child — but a grandchild that inherited the write end keeps it open, and no
/// output is worth stalling a completed call for.
const DRAIN_SETTLE: Duration = Duration::from_millis(200);

/// How long to wait for a killed child to leave the process table. Bounded rather than a plain
/// `wait()`: SIGKILL only lands when the child next leaves the kernel, so a process stuck in an
/// uninterruptible syscall — the very failure this module exists for — would otherwise strand a
/// caller who has already spent their whole budget. A stray zombie is the cheaper outcome.
const KILL_REAP: Duration = Duration::from_millis(500);

/// Most output one call may buffer, so a drain thread left reading a pipe some grandchild still
/// holds cannot grow without bound. Far past any real one-shot's output — a full `uiautomator dump`
/// of a deep tree is a few hundred KiB.
const MAX_CAPTURE: usize = 64 * 1024 * 1024;

/// Run `cmd` to completion, or kill it and fail once `budget` elapses.
///
/// `op` names the operation in the error (`"adb:uiautomator dump"`), so a timeout says which call
/// hung rather than merely which binary. A non-zero exit is NOT an error here: the returned
/// [`Output`] carries it exactly as [`Command::output`] would, leaving each caller's own exit
/// handling untouched.
///
/// Both pipes are drained on their own threads: a child that fills a pipe buffer while the parent
/// waits blocks in `write` and never exits, putting the deadline itself out of reach.
pub fn run_bounded(cmd: &mut Command, budget: Duration, op: &str) -> Result<Output> {
    run_bounded_inner(cmd, budget, op, None)
}

/// [`run_bounded`], but writing `stdin` to the child first.
///
/// The write happens on its own thread for the same reason the reads do: a tool that answers before
/// consuming all of its input leaves the parent blocked in `write`, with the deadline out of reach.
/// `simctl pbcopy` is the caller — it takes the clipboard text on stdin, so it cannot go through the
/// plain runner, which closes stdin.
pub fn run_bounded_with_stdin(
    cmd: &mut Command,
    budget: Duration,
    op: &str,
    stdin: &[u8],
) -> Result<Output> {
    run_bounded_inner(cmd, budget, op, Some(stdin.to_vec()))
}

fn run_bounded_inner(
    cmd: &mut Command,
    budget: Duration,
    op: &str,
    stdin: Option<Vec<u8>>,
) -> Result<Output> {
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GlassError::Backend(format!("{op}: failed to start: {e}")))?;

    if let Some(bytes) = stdin {
        // Best-effort and detached: a child that never reads its stdin must not block the parent,
        // and whatever it does read it reads before it exits, which the deadline already covers.
        if let Some(mut pipe) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
    }

    let stdout = Pipe::drain(child.stdout.take());
    let stderr = Pipe::drain(child.stderr.take());

    let deadline = Instant::now() + budget;
    let mut wait = POLL_START;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            // The one exit that would otherwise leave the child running with no deadline at all —
            // `Child::drop` neither kills nor reaps.
            Err(e) => {
                let killed = kill_and_reap(&mut child);
                return Err(GlassError::Backend(format!(
                    "{op}: could not check whether the process had exited: {e}{}",
                    if killed {
                        "; it was killed"
                    } else {
                        "; it may still be running"
                    }
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(timed_out(&mut child, op, budget, stdout, stderr));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(POLL);
    };

    // One deadline for both pipes, so a finished call is never stalled twice over.
    let settled_by = Instant::now() + DRAIN_SETTLE;
    let (out_bytes, out_done) = stdout.take(settled_by);
    let (err_bytes, err_done) = stderr.take(settled_by);
    // A short read must never pass for a complete answer: `dumpsys window windows` is parsed by a
    // tolerant line scanner that would read a truncated dump as a shorter window list, and glass
    // would then report the wrong geometry. `Command::output` cannot produce this — it reads to EOF
    // — so failing here keeps the contract callers already had.
    if !out_done || !err_done {
        return Err(GlassError::Backend(format!(
            "{op}: exited {status} but its output was still arriving {DRAIN_SETTLE:?} later \
             (something inherited its pipe), so the {} bytes read may be incomplete",
            out_bytes.len() + err_bytes.len()
        )));
    }

    Ok(Output {
        status,
        stdout: out_bytes,
        stderr: err_bytes,
    })
}

/// One of the child's pipes, read on its own thread.
struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    /// Set once the thread has read to EOF, so a caller can tell a complete answer from one it gave
    /// up waiting for.
    done: Arc<Mutex<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl Pipe {
    /// Start reading `pipe` on a new thread, appending each chunk to a buffer readable at any time.
    ///
    /// Chunked rather than `read_to_end` so a timeout can report what the child had already said: a
    /// killed child does not necessarily close the pipe — `sh -c "...; sleep 30"` leaves `sleep`
    /// holding the write end, and `adb` likewise spawns subprocesses — so a thread blocked in
    /// `read_to_end` would still be waiting for EOF with the bytes stuck in its local buffer.
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Self {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(false));
        let Some(mut pipe) = pipe else {
            // No pipe is a complete read of nothing, not an abandoned one.
            *done.lock().expect("fresh mutex") = true;
            return Self {
                buf,
                done,
                thread: None,
            };
        };
        let sink = Arc::clone(&buf);
        let finished = Arc::clone(&done);
        let thread = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => {
                        if let Ok(mut finished) = finished.lock() {
                            *finished = true;
                        }
                        break;
                    }
                    Ok(n) => {
                        let Ok(mut sink) = sink.lock() else { break };
                        if sink.len() + n > MAX_CAPTURE {
                            break;
                        }
                        sink.extend_from_slice(&chunk[..n]);
                    }
                    // A signal interrupting the read is not end of stream; `read_to_end` retries
                    // this for you, a bare `read` does not, and glass-mcp installs signal handlers.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        Self {
            buf,
            done,
            thread: Some(thread),
        }
    }

    /// Everything read so far, and whether the pipe reached EOF. Waits until `settled_by` for the
    /// thread to finish first, so a completed call reports its whole output; a grandchild holding
    /// the pipe open must not strand the caller.
    fn take(self, settled_by: Instant) -> (Vec<u8>, bool) {
        poll_until(settled_by, || {
            self.thread.as_ref().is_none_or(JoinHandle::is_finished)
        });
        let bytes = self
            .buf
            .lock()
            .map(|mut b| std::mem::take(&mut *b))
            .unwrap_or_default();
        // A lock we cannot read is not evidence of a complete read.
        let done = self.done.lock().map(|d| *d).unwrap_or(false);
        (bytes, done)
    }

    /// Everything read so far, without waiting — for a timeout report, where partial output is the
    /// point.
    fn snapshot(&self) -> Vec<u8> {
        self.buf.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

/// Sleep in [`POLL`] steps until `ready` answers true or `deadline` passes.
fn poll_until(deadline: Instant, mut ready: impl FnMut() -> bool) {
    while !ready() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}

/// Kill a child and wait briefly for it to leave the process table. `true` if it did.
fn kill_and_reap(child: &mut Child) -> bool {
    let _ = child.kill();
    let mut reaped = false;
    poll_until(Instant::now() + KILL_REAP, || {
        reaped = matches!(child.try_wait(), Ok(Some(_)));
        reaped
    });
    reaped
}

/// Kill the child and describe the timeout, including whatever it managed to say first — a partial
/// `uiautomator dump` tells a reader more about the hang than silence does.
fn timed_out(
    child: &mut Child,
    op: &str,
    budget: Duration,
    stdout: Pipe,
    stderr: Pipe,
) -> GlassError {
    let reaped = kill_and_reap(child);
    let out = String::from_utf8_lossy(&stdout.snapshot())
        .trim()
        .to_string();
    let err = String::from_utf8_lossy(&stderr.snapshot())
        .trim()
        .to_string();
    let mut said: Vec<String> = Vec::new();
    if !out.is_empty() {
        said.push(format!("stdout: {out}"));
    }
    if !err.is_empty() {
        said.push(format!("stderr: {err}"));
    }
    let said = if said.is_empty() {
        "it produced no output before the kill".to_string()
    } else {
        format!("before the kill — {}", said.join("; "))
    };
    let fate = if reaped {
        "so the process was killed"
    } else {
        "so the process was killed, though it had not exited yet (it may be stuck in the kernel)"
    };
    GlassError::Backend(format!("{op}: no answer within {budget:?}, {fate}; {said}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn a_fast_command_returns_its_output() {
        let out = run_bounded(
            Command::new("/bin/sh").args(["-c", "printf ready"]),
            Duration::from_secs(10),
            "test:fast",
        )
        .expect("fast command");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ready");
    }

    #[test]
    fn a_command_that_outlives_its_budget_is_killed_and_named() {
        let started = std::time::Instant::now();
        let err = run_bounded(
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
            "test:hang",
        )
        .expect_err("must not wait out a hung child");
        // The budget is enforced, not merely documented.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("test:hang"), "{msg}");
        assert!(msg.contains("300ms"), "{msg}");
    }

    #[test]
    fn a_nonzero_exit_is_returned_not_reported_as_a_timeout() {
        let out = run_bounded(
            Command::new("/bin/sh").args(["-c", "exit 3"]),
            Duration::from_secs(10),
            "test:exit",
        )
        .expect("a failing command still yields its Output");
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn stderr_is_captured_alongside_stdout() {
        let out = run_bounded(
            Command::new("/bin/sh").args(["-c", "printf out; printf err 1>&2"]),
            Duration::from_secs(10),
            "test:streams",
        )
        .expect("both streams");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err");
    }

    #[test]
    fn what_a_hung_child_said_before_the_kill_rides_in_the_error() {
        // A partial `uiautomator dump` says more about a hang than silence does.
        let err = run_bounded(
            Command::new("/bin/sh").args(["-c", "printf partial-tree; sleep 30"]),
            Duration::from_millis(300),
            "test:partial",
        )
        .expect_err("must time out");
        assert!(err.to_string().contains("partial-tree"), "{err}");
    }

    #[test]
    fn a_pipe_still_open_after_the_child_exits_ends_promptly_as_an_error() {
        // A grandchild that inherited the write end keeps the pipe open after the child exits, so
        // EOF may never come. Two things must hold, and they pull against each other: the call must
        // not stall waiting for EOF, and it must not pass off what arrived as the whole answer —
        // `dumpsys window windows` is parsed by a tolerant line scanner that would read a truncated
        // dump as a shorter window list, and glass would report the wrong geometry from it.
        //
        // The parent cannot tell "the grandchild will write nothing more" from "the grandchild is
        // about to write", so both settle out as this error rather than a short success.
        let started = Instant::now();
        let err = run_bounded(
            Command::new("/bin/sh").args([
                "-c",
                "printf head; { sleep 1; printf tail-written-late; } &",
            ]),
            Duration::from_secs(20),
            "test:late-writer",
        )
        .expect_err("an incomplete read must not pass for success");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} — a finished call must not wait out a grandchild",
            started.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("test:late-writer"), "{msg}");
        assert!(msg.contains("may be incomplete"), "{msg}");
    }

    #[test]
    fn a_flood_on_either_pipe_completes() {
        // 1 MiB down each stream: draining only stdout would deadlock on a stderr-heavy child.
        let out = run_bounded(
            Command::new("/bin/sh").args([
                "-c",
                "yes A | head -c 1048576; yes B | head -c 1048576 1>&2",
            ]),
            Duration::from_secs(30),
            "test:both-floods",
        )
        .expect("both pipes drained");
        assert_eq!(out.stdout.len(), 1_048_576);
        assert_eq!(out.stderr.len(), 1_048_576);
    }

    #[test]
    fn a_killed_child_is_reaped_not_left_a_zombie() {
        // `Child::drop` does not reap on Unix, and glass-mcp is long-lived, so a timeout that
        // skipped the wait would leak a zombie per hung call. The pid rides out in the error.
        let err = run_bounded(
            Command::new("/bin/sh").args(["-c", "printf $$; sleep 30"]),
            Duration::from_millis(300),
            "test:reap",
        )
        .expect_err("must time out");
        let msg = err.to_string();
        let pid: i32 = msg
            .split("stdout: ")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {msg}"));
        // `kill -0` fails once the process is gone, including as a zombie's parent has reaped it.
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "pid {pid} still exists — the child was not reaped");
    }

    #[test]
    fn stdin_is_written_to_the_child() {
        let out = run_bounded_with_stdin(
            &mut Command::new("/bin/cat"),
            Duration::from_secs(10),
            "test:stdin",
            b"clipboard text",
        )
        .expect("cat echoes its stdin");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "clipboard text");
    }

    #[test]
    fn a_child_that_ignores_its_stdin_still_returns() {
        // A tool that answers without consuming its input would block a parent that wrote inline.
        let out = run_bounded_with_stdin(
            Command::new("/bin/sh").args(["-c", "printf done"]),
            Duration::from_secs(10),
            "test:stdin-ignored",
            &vec![b'x'; 1024 * 1024],
        )
        .expect("must not block on an unread stdin");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "done");
    }

    #[test]
    fn a_missing_program_is_a_spawn_error_naming_the_operation() {
        let err = run_bounded(
            &mut Command::new("/nonexistent/glass-test-binary"),
            Duration::from_secs(10),
            "test:spawn",
        )
        .expect_err("spawn must fail");
        assert!(err.to_string().contains("test:spawn"), "{err}");
    }
}
