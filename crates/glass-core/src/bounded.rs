//! Run a one-shot child process under a deadline.
//!
//! Every backend that drives an external tool (`adb`, `xcrun simctl`, `plutil`) needs the same
//! thing: the tool answers quickly, or it has hung and must not hang the agent's call with it.
//! `std::process` offers no wait-with-a-deadline, so this module supplies the one shape they all
//! use. A long-lived child — a log tail, an emulator, the on-device agent — is NOT this: those use
//! `Command::spawn` directly and are meant to outlive the call that started them.

use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{BoundKind, GlassError, Result};

/// The phrase a timeout error carries, for a reader of the message. What a *caller* keys on is
/// [`BoundKind::TimedOut`]: the message this appears in embeds the child's own output, so matching
/// on it is matching prose the device helps write (glass#348).
const TIMED_OUT: &str = "no answer within";

/// The phrase an error carries when a call was never started, because the deadline it serves was
/// already spent. Deliberately not [`TIMED_OUT`]: the remedies for a tool that hung do not apply to
/// one that never ran, and [`BoundKind`] keeps the two apart for callers.
const NOT_STARTED: &str = "was not started";

/// Longest gap between checks on a child that has not exited yet. The wait starts far tighter and
/// backs off to this, so a one-shot that answers in a millisecond is not billed a full tick and a
/// long wait settles to one wakeup every 20ms.
const POLL: Duration = Duration::from_millis(20);

/// First gap between checks, doubled up to [`POLL`].
const POLL_START: Duration = Duration::from_millis(1);

/// How long to let a drain thread reach EOF after the child has exited. Normally instant — the pipe
/// closes with the child — but something the child started may hold it open, and the call ends as
/// an error rather than stalling for it.
const DRAIN_SETTLE: Duration = Duration::from_millis(200);

/// How long to wait for a killed child to leave the process table. Bounded rather than a plain
/// `wait()`: SIGKILL only lands when the child next leaves the kernel, so a process stuck in an
/// uninterruptible syscall — the very failure this module exists for — would otherwise strand a
/// caller who has already spent their whole budget. A stray zombie is the cheaper outcome.
const KILL_REAP: Duration = Duration::from_millis(500);

/// Most output ONE PIPE may buffer, so a drain thread left reading a pipe something else still
/// holds cannot grow without bound. Far past any real one-shot's output: measured on the dogfood
/// AVD, a `uiautomator dump` is ~12KB and the largest, `exec-out screencap`, is ~10MB.
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

/// [`run_bounded`], but waiting no later than `deadline` — the bound of the larger call this one
/// serves. Killing and draining a child still runs after that, as it does for a plain budget.
///
/// Several calls can answer one request: one `uiautomator dump` is a remove, a dump and a read.
/// Each carrying only its own budget makes the sequence cost their sum, so the deadline travels
/// down and each step gets whichever bound is nearer. A step with nothing left is not started at
/// all, and fails with [`BoundKind::NotStarted`].
pub fn run_bounded_until(
    cmd: &mut Command,
    budget: Duration,
    deadline: Instant,
    op: &str,
) -> Result<Output> {
    let budget = budget_within(budget, deadline, Instant::now());
    if budget.is_zero() {
        return Err(GlassError::Bounded {
            kind: BoundKind::NotStarted,
            message: format!(
                "{op}: the deadline it shares with the rest of the call was already spent, so it \
                 {NOT_STARTED}"
            ),
        });
    }
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

    // Written on its own thread: a child that answers without consuming its input would otherwise
    // leave the parent blocked in `write` with the deadline out of reach. The outcome comes back
    // over a channel, because a payload that only partly landed is the same silent truncation as a
    // partial read — `pbcopy` would report a clipboard it never fully received.
    //
    // A channel and not a shared cell: nothing joins this thread, so a cell says only what the
    // writer had recorded when the parent looked, and "not yet recorded" reads the same as "wrote
    // everything".
    let stdin_len = stdin.as_ref().map_or(0, Vec::len);
    let wrote = if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // `pipe` is owned here, so the write end closes when this returns however it returns.
            let _ = tx.send(pipe.write_all(&bytes));
        });
        Some(rx)
    } else {
        None
    };

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
        if deadline_passed(Instant::now(), deadline) {
            return Err(timed_out(&mut child, op, budget, stdout, stderr));
        }
        std::thread::sleep(wait);
        wait = next_wait(wait);
    };

    // One deadline for both pipes, so a finished call is never stalled twice over.
    let settled_by = Instant::now() + DRAIN_SETTLE;
    let (out_bytes, out_done) = stdout.take(settled_by);
    let (err_bytes, err_done) = stderr.take(settled_by);
    // A short read must never pass for a complete answer: `dumpsys window windows` is parsed by a
    // tolerant line scanner that would read a truncated dump as a shorter window list, and glass
    // would then report the wrong geometry.
    //
    // Bounded by the same `settled_by` as the pipes, and for the same reason: a grandchild holding
    // the read end keeps the write blocked exactly as it keeps a drain from reaching EOF. A write
    // still in flight when that expires is reported rather than waited out; the thread stays parked
    // until the kernel releases it, which a blocking write offers no way to cancel.
    if let Some(rx) = wrote {
        match rx.recv_timeout(settled_by.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(GlassError::Backend(format!(
                    "{op}: could not write all of its input ({e}), so it acted on a partial payload"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(GlassError::Backend(format!(
                    "{op}: exited, but had not finished writing its {stdin_len} bytes of input \
                     {DRAIN_SETTLE:?} later (something it started may still hold the pipe), so it \
                     may have acted on a partial payload"
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(GlassError::Backend(format!(
                    "{op}: the thread writing its input ended without reporting, so whether the \
                     payload landed is unknown"
                )));
            }
        }
    }
    if !out_done || !err_done {
        return Err(GlassError::Backend(format!(
            "{op}: exited, but its output pipe had not reached end-of-file {DRAIN_SETTLE:?} \
             later (something it started may still hold it), so the {} bytes read may be incomplete",
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
                        if would_exceed_capture(sink.len(), n) {
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

/// The next gap between checks: double, capped at [`POLL`]. Its own function so the growth is
/// pinned by a test — an arithmetic slip here only changes how often the parent wakes, which no
/// behavioural test can see.
fn next_wait(previous: Duration) -> Duration {
    (previous * 2).min(POLL)
}

/// Whether appending `n` more bytes to a buffer of `len` would pass [`MAX_CAPTURE`].
fn would_exceed_capture(len: usize, n: usize) -> bool {
    len + n > MAX_CAPTURE
}

/// The budget a call actually gets: its own, or what is left of the deadline it serves, whichever
/// is nearer. Its own function so the rule is pinned by a test, which a timing test cannot do.
fn budget_within(budget: Duration, deadline: Instant, now: Instant) -> Duration {
    budget.min(deadline.saturating_duration_since(now))
}

/// Whether `deadline` has arrived. Its own function so the boundary — a deadline exactly reached
/// counts as passed — is pinned by a test rather than by an inequality no timing test can pin.
fn deadline_passed(now: Instant, deadline: Instant) -> bool {
    now >= deadline
}

/// Sleep in [`POLL`] steps until `ready` answers true or `deadline` passes.
fn poll_until(deadline: Instant, mut ready: impl FnMut() -> bool) {
    while !ready() && !deadline_passed(Instant::now(), deadline) {
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

/// Whatever the child managed to say before it was killed — a partial `uiautomator dump` tells a
/// reader more about a hang than silence does.
fn said_before_the_kill(stdout: &Pipe, stderr: &Pipe) -> String {
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
    if said.is_empty() {
        "it produced no output before the kill".to_string()
    } else {
        format!("before the kill — {}", said.join("; "))
    }
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
    let said = said_before_the_kill(&stdout, &stderr);
    let fate = if reaped {
        "so the process was killed"
    } else {
        "so the process was killed, though it had not exited yet (it may be stuck in the kernel)"
    };
    GlassError::Bounded {
        kind: BoundKind::TimedOut,
        message: format!("{op}: {TIMED_OUT} {budget:?}, {fate}; {said}"),
    }
}
#[cfg(test)]
mod tests {
    //! The tests that drive a real child use `/bin/sh` and friends, so they are `cfg(unix)`: this
    //! module runs on Windows too — `glass-android` shells out to `adb` from any host — and a
    //! hardcoded `/bin/sh` there fails with "the system cannot find the path specified" rather than
    //! testing anything. What stays portable is everything that needs no process: the capture cap,
    //! the deadline boundary, the backoff, and the pipe-drain behaviour, which takes any `Read`.

    use super::*;
    #[cfg(unix)]
    use std::process::Command;
    use std::time::Duration;

    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    fn a_pipe_still_open_after_the_child_exits_ends_promptly_as_an_error() {
        // Two properties that pull against each other: the call must not stall waiting for an EOF
        // that may never come, and it must not pass off what arrived as the whole answer. The
        // parent cannot tell "the grandchild will write nothing more" from "it is about to write",
        // so both settle out as this error.
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "pid {pid} still exists — the child was not reaped");
    }

    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
    fn a_child_that_ignores_its_stdin_ends_promptly_and_says_the_payload_did_not_land() {
        // Two properties, and the first is why the write is on a thread at all: a tool that answers
        // without reading its input must not leave the parent blocked in `write`. The second is
        // that the payload going nowhere is reported — `pbcopy` exits 0 whether or not it received
        // the whole clipboard, so silence here would be a clipboard glass claims it set.
        let started = Instant::now();
        let err = run_bounded_with_stdin(
            Command::new("/bin/sh").args(["-c", "printf done"]),
            Duration::from_secs(10),
            "test:stdin-ignored",
            &vec![b'x'; 4 * 1024 * 1024],
        )
        .expect_err("an unwritten payload must not pass silently");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} — the write must not block the call",
            started.elapsed()
        );
        assert!(
            err.to_string().contains("partial payload"),
            "must say the input did not land: {err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_write_still_in_flight_when_the_child_exits_is_not_reported_as_landed() {
        // The sibling test above races: whether the write thread has recorded its `EPIPE` when the
        // parent looks is a matter of scheduling. Here the child exits at once but leaves a
        // grandchild holding the read end, so the write is still blocked and no outcome has been
        // written by anyone. The grandchild's stdout and stderr go to /dev/null, so both drains
        // reach EOF and what is under test is the write, not a held-open output pipe.
        //
        // `exec 3<&0` then `<&3` is load-bearing: POSIX gives a background job in a non-interactive
        // shell its stdin from /dev/null *before* any explicit redirection, so a plain `sleep &`
        // does not inherit the pipe and the write fails fast with `EPIPE` instead of hanging.
        let err = run_bounded_with_stdin(
            Command::new("/bin/sh")
                .args(["-c", "exec 3<&0; sleep 2 <&3 >/dev/null 2>&1 & printf done"]),
            Duration::from_secs(10),
            "test:stdin-in-flight",
            &vec![b'x'; 4 * 1024 * 1024],
        )
        .expect_err("a payload whose fate is unknown must not pass as landed");
        assert!(
            err.to_string().contains("had not finished writing"),
            "must say the write never finished: {err}"
        );
    }

    #[test]
    fn the_capture_cap_holds_the_largest_real_payload_and_stops_just_past_itself() {
        // The cap bounds an abandoned drain thread, so it must clear the biggest thing a
        // one-shot really produces — a ~10MB `exec-out screencap` — by a wide margin. A const
        // block, so a mutation that shrinks it stops the crate compiling, which the gate counts.
        const { assert!(MAX_CAPTURE >= 10 * 1024 * 1024) };
        // Exactly at the cap is still allowed; one byte past it is not.
        assert!(!would_exceed_capture(MAX_CAPTURE, 0));
        assert!(!would_exceed_capture(MAX_CAPTURE - 1, 1));
        assert!(would_exceed_capture(MAX_CAPTURE, 1));
    }

    #[test]
    fn a_deadline_exactly_reached_counts_as_passed() {
        // The boundary no timing test can reach: waiting one more round at the deadline would mean
        // a budget that is always at least one poll longer than it says.
        let t = Instant::now();
        assert!(deadline_passed(t, t));
        assert!(deadline_passed(t + Duration::from_millis(1), t));
        assert!(!deadline_passed(t, t + Duration::from_millis(1)));
    }

    #[test]
    fn an_interrupted_read_resumes_instead_of_ending_the_stream() {
        // EINTR is not end of stream: `read_to_end` retries it for you, a bare `read` does not, and
        // glass-mcp installs signal handlers. Treating it as EOF would silently truncate.
        struct Interrupts {
            reads: usize,
        }
        impl Read for Interrupts {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                match self.reads {
                    1 => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                    2 => {
                        buf[..5].copy_from_slice(b"after");
                        Ok(5)
                    }
                    _ => Ok(0),
                }
            }
        }
        let pipe = Pipe::drain(Some(Interrupts { reads: 0 }));
        let (bytes, done) = pipe.take(Instant::now() + Duration::from_secs(5));
        assert_eq!(String::from_utf8_lossy(&bytes), "after");
        assert!(done, "the reader reached EOF, so the read is complete");
    }

    #[test]
    fn any_other_read_error_ends_the_stream_and_is_not_a_complete_read() {
        // Only EINTR resumes. Resuming past a real error would walk on to the next read and call
        // whatever it found a finished stream — the truncation this module refuses to return.
        struct Broken {
            reads: usize,
        }
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                match self.reads {
                    1 => Err(std::io::Error::other("device disappeared")),
                    _ => Ok(0),
                }
            }
        }
        let pipe = Pipe::drain(Some(Broken { reads: 0 }));
        let (bytes, done) = pipe.take(Instant::now() + Duration::from_secs(5));
        assert!(bytes.is_empty(), "{bytes:?}");
        assert!(!done, "a broken read is not an end-of-file");
    }

    #[test]
    fn the_wait_between_checks_doubles_up_to_the_cap() {
        // A one-shot answering in a millisecond must not be billed a full tick, and a long wait
        // must settle to a steady cadence rather than spinning.
        assert_eq!(
            next_wait(Duration::from_millis(1)),
            Duration::from_millis(2)
        );
        assert_eq!(
            next_wait(Duration::from_millis(8)),
            Duration::from_millis(16)
        );
        assert_eq!(next_wait(Duration::from_millis(16)), POLL);
        assert_eq!(next_wait(POLL), POLL);
    }

    #[test]
    #[cfg(unix)]
    fn one_pipe_left_open_is_enough_to_report_an_incomplete_read() {
        // stderr closes with the child; only stdout stays held. A check that demanded BOTH pipes be
        // incomplete would call this a clean, complete answer.
        let err = run_bounded(
            Command::new("/bin/sh")
                .args(["-c", "printf head; { sleep 1; printf tail; } 2>/dev/null &"]),
            Duration::from_secs(20),
            "test:one-pipe",
        )
        .expect_err("one unfinished pipe is an incomplete read");
        assert!(err.to_string().contains("may be incomplete"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn a_hung_child_that_only_wrote_to_stderr_is_quoted_too() {
        // The stdout branch is covered above; without this, deleting the emptiness check on the
        // stderr branch goes unnoticed.
        let err = run_bounded(
            Command::new("/bin/sh").args(["-c", "printf complaining 1>&2; sleep 30"]),
            Duration::from_millis(300),
            "test:stderr-only",
        )
        .expect_err("must time out");
        let msg = err.to_string();
        assert!(msg.contains("stderr: complaining"), "{msg}");
        assert!(
            !msg.contains("stdout:"),
            "nothing was written to stdout: {msg}"
        );
    }

    #[test]
    fn a_call_gets_the_nearer_of_its_own_budget_and_the_deadline_it_serves() {
        // The rule the deadline-taking runner exists for, pinned where no timing test can reach it.
        let now = Instant::now();
        assert_eq!(
            budget_within(Duration::from_secs(20), now + Duration::from_secs(5), now),
            Duration::from_secs(5)
        );
        assert_eq!(
            budget_within(Duration::from_secs(3), now + Duration::from_secs(5), now),
            Duration::from_secs(3)
        );
        assert_eq!(
            budget_within(Duration::from_secs(20), now, now),
            Duration::ZERO
        );
        let past = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        assert_eq!(
            budget_within(Duration::from_secs(20), past, now),
            Duration::ZERO
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_call_dies_at_the_deadline_it_serves_rather_than_at_its_own_budget() {
        // A step of a longer sequence gets what is left of the sequence's deadline, not the sum of
        // three steps' budgets.
        let started = Instant::now();
        let err = run_bounded_until(
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_secs(20),
            Instant::now() + Duration::from_millis(300),
            "test:outer-deadline",
        )
        .expect_err("must not wait out its own budget past the deadline it serves");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("test:outer-deadline"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn a_deadline_further_out_than_the_budget_leaves_the_budget_in_charge() {
        // The clamp is a ceiling, not a replacement: a call with room to spare still dies at the
        // budget its own operation was given.
        let err = run_bounded_until(
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(60),
            "test:own-budget",
        )
        .expect_err("must time out");
        assert!(err.to_string().contains("300ms"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn a_call_with_no_time_left_is_not_started_at_all() {
        // Spawning a child only to kill it at once costs a process and a wait. The wording is what
        // discriminates: spawning would time out at 0ns and say so.
        let started = Instant::now();
        let err = run_bounded_until(
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_secs(20),
            Instant::now(),
            "test:spent-deadline",
        )
        .expect_err("a call with no time left must fail rather than run");
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "spawning and killing a doomed child takes longer than this: {:?}",
            started.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("test:spent-deadline"), "{msg}");
        assert!(msg.contains(NOT_STARTED), "{msg}");
        // Nothing was asked, so nothing failed to answer — a reader must not be told the tool
        // hung. Callers key on `BoundKind`, not on this phrase.
        assert!(!msg.contains(TIMED_OUT), "{msg}");
    }

    #[test]
    #[cfg(unix)]
    fn a_command_that_finishes_inside_both_bounds_returns_its_output() {
        // The deadline-taking runner has the same contract as the plain one on the way out: the
        // output is returned, and a non-zero exit is the caller's to judge, not a failure here.
        let out = run_bounded_until(
            Command::new("/bin/sh").args(["-c", "printf ready; exit 3"]),
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(10),
            "test:until-fast",
        )
        .expect("a fast command yields its Output");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ready");
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    #[cfg(unix)]
    fn a_call_killed_at_its_bound_says_it_timed_out_as_a_value() {
        // That a bound fired decides whether a backend retries and whether a caller's spent budget
        // is reported as a device failure; which one decides whether the wedged-tool remedy is
        // offered.
        let hung = run_bounded(
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
            "test:kind-timeout",
        )
        .expect_err("must time out");
        assert_eq!(hung.bound(), Some(BoundKind::TimedOut), "{hung}");
    }

    #[test]
    fn a_call_with_nothing_left_says_it_never_started_as_a_value() {
        // This path returns before anything is spawned, so the binary need not exist — which is
        // what lets the Windows leg cover the kind a `wait_for_element` polls through.
        let spent = run_bounded_until(
            &mut Command::new("/nonexistent/glass-test-binary"),
            Duration::from_secs(20),
            Instant::now(),
            "test:kind-not-started",
        )
        .expect_err("a call with no time left must fail rather than run");
        assert_eq!(spent.bound(), Some(BoundKind::NotStarted), "{spent}");
    }

    #[test]
    fn a_failure_that_is_not_a_bound_firing_carries_no_kind() {
        // The classification must not widen to "this call failed": a missing binary read as a
        // bound becomes a wait polling on for its whole timeout instead of failing at once.
        // Portable, so the Windows leg covers this direction too.
        let spawn = run_bounded(
            &mut Command::new("/nonexistent/glass-test-binary"),
            Duration::from_secs(10),
            "test:kind-spawn",
        )
        .expect_err("spawn must fail");
        assert_eq!(spawn.bound(), None, "{spawn}");

        assert_eq!(GlassError::Backend("device offline".into()).bound(), None);
    }

    #[test]
    #[cfg(unix)]
    fn a_child_that_answered_before_a_glass_side_timer_elapsed_is_no_bound_firing() {
        // `DRAIN_SETTLE` does elapse here, so this is the case that looks most like a bound and is
        // not one: the child exited on its own, and the deadline never came near.
        let held_open = run_bounded(
            Command::new("/bin/sh").args(["-c", "printf head; { sleep 1; printf tail; } &"]),
            Duration::from_secs(20),
            "test:kind-incomplete",
        )
        .expect_err("an incomplete read is an error");
        assert_eq!(held_open.bound(), None, "{held_open}");
    }

    #[test]
    #[cfg(unix)]
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
