//! Bounded capture of a child's stderr, shared by the Linux backends.

use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::process::ChildStderr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

/// How much of a child's stderr is kept for diagnostics.
///
/// An X server dumps its option table (~5 KiB) *after* the fatal line and sway's complaints come
/// first, so the head is what is kept.
pub const STDERR_KEPT: usize = 8 * 1024;

/// How long an idle reader sleeps before looking at the stop flag again.
///
/// Also the ceiling on [`StderrTail::finish`]'s join if the wakeup is lost — the flag is what
/// stops the reader.
const HEARD_BY: Timespec = Timespec {
    tv_sec: 5,
    tv_nsec: 0,
};

/// A child's stderr, read on a helper thread until the pipe closes or the tail ends it.
///
/// Not a read at the end: a child whose stderr nobody drains stalls on a full pipe, which the
/// caller reads as a process that never came up.
///
/// The write end is inherited by everything the child spawned, so a process outliving the
/// teardown holds the pipe open, and a reader that could only stop at EOF would park there
/// holding an fd — one per deep probe, for the life of a `glass-mcp serve --http` (glass#471).
pub struct StderrTail {
    kept: Arc<Mutex<Vec<u8>>>,
    /// Set by the reader on its way out, so [`StderrTail::finish`] can end its wait as soon as
    /// the pipe closes on its own.
    done: Arc<AtomicBool>,
    /// What actually stops the reader. A flag rather than the wakeup below, so a wakeup that
    /// never arrives costs [`HEARD_BY`] rather than the life of the process.
    stopping: Arc<AtomicBool>,
    /// Wakes a reader asleep in `poll` so it reads the flag now instead of at [`HEARD_BY`].
    /// Nobody drains the counter, so one write stays readable and the next poll is certain to
    /// see it.
    wake: OwnedFd,
    /// Taken by whichever of [`StderrTail::finish`] and `drop` runs first.
    reader: Option<JoinHandle<()>>,
}

impl StderrTail {
    /// Start reading.
    ///
    /// `Err` is the host refusing a resource the bounded read is built from: the thread —
    /// `Builder`, where `thread::spawn` panics (glass#454) — the wakeup fd or its dup, or the
    /// non-blocking mode. `stderr` is consumed either way, so on `Err` the read end is already
    /// closed and the child will take `EPIPE` rather than stall.
    pub fn drain(stderr: ChildStderr) -> std::io::Result<StderrTail> {
        // Blocking, a read with nothing to read parks past every look at the flag. Safe for the
        // child: `pipe(2)` gives the two ends separate open file descriptions, so a status flag
        // set here never reaches the write end it inherited.
        fcntl_setfl(&stderr, fcntl_getfl(&stderr)?.union(OFlags::NONBLOCK))?;
        // CLOEXEC or this fd lands in every process the host spawns afterwards. No NONBLOCK:
        // `end` writes to it once, and only a saturated counter would block that.
        let wake = eventfd(0, EventfdFlags::CLOEXEC)?;
        // A dup, not a second eventfd: both fds name one counter, so `end`'s write is what this
        // reader's poll sees.
        let woken = wake.try_clone()?;
        let kept: Arc<Mutex<Vec<u8>>> = Arc::default();
        let done: Arc<AtomicBool> = Arc::default();
        let stopping: Arc<AtomicBool> = Arc::default();
        let (keeping, ended, stop) = (Arc::clone(&kept), Arc::clone(&done), Arc::clone(&stopping));
        let reader = std::thread::Builder::new()
            .name("glass-stderr-tail".into())
            .spawn(move || {
                read_until_stopped(stderr, &woken, &stop, &keeping);
                ended.store(true, Ordering::Release);
            })?;
        Ok(StderrTail {
            kept,
            done,
            stopping,
            wake,
            reader: Some(reader),
        })
    }

    /// What it said, at most [`STDERR_KEPT`] bytes, after `grace` for the pipe to close — call
    /// once the child is reaped.
    ///
    /// Collected whether or not the pipe closed: anything the child left running holds it open,
    /// and the reader is ended either way.
    pub fn finish(mut self, grace: Duration) -> String {
        crate::await_condition(grace, || self.done.load(Ordering::Acquire));
        self.end();
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&kept).into_owned()
    }

    /// Stop the reader and wait for it to go, which is what makes the pipe closed rather than
    /// assumed closed. Idempotent, because `finish` runs it and then drops.
    fn end(&mut self) {
        let Some(reader) = self.reader.take() else {
            return;
        };
        // Set before the wake, or a reader the write pulls out of `poll` finds the flag still
        // false and goes back to sleep for another `HEARD_BY`.
        self.stopping.store(true, Ordering::Release);
        let _ = rustix::io::write(&self.wake, &1u64.to_ne_bytes());
        let _ = reader.join();
    }
}

impl Drop for StderrTail {
    /// A tail nobody reads still ends its reader.
    fn drop(&mut self) {
        self.end();
    }
}

/// Hides the capture: derived, it renders up to [`STDERR_KEPT`] decimal byte values into
/// whatever printed the struct holding it.
impl std::fmt::Debug for StderrTail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("StderrTail")
            .field("kept", &format_args!("{} bytes", kept.len()))
            .field("done", &self.done.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Read `stderr` until it ends or `stopping` says to, keeping the first [`STDERR_KEPT`] bytes.
fn read_until_stopped(
    mut stderr: ChildStderr,
    wake: &OwnedFd,
    stopping: &AtomicBool,
    keeping: &Mutex<Vec<u8>>,
) {
    let mut chunk = [0u8; 4096];
    // The flag is read before every read, not only when the pipe runs dry, so a child that
    // never stops writing cannot keep the reader here either.
    while !stopping.load(Ordering::Acquire) {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut kept = keeping.lock().unwrap_or_else(PoisonError::into_inner);
                // Past the cap the pipe is still drained, so the child is never blocked writing
                // to it.
                let room = STDERR_KEPT.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(e) => match e.kind() {
                // Nothing to read yet, or the read was interrupted: back to the wait.
                ErrorKind::WouldBlock | ErrorKind::Interrupted => wait_for_more(&stderr, wake),
                // Anything else will not read better next time.
                _ => break,
            },
        }
    }
}

/// Sleep until the pipe has more to say, the wakeup is rung, or [`HEARD_BY`] elapses. The caller
/// re-reads whichever it was, so none of the three needs telling apart.
///
/// A poll that errors sleeps instead of returning at once: a persistent one — `EINVAL` under an
/// `RLIMIT_NOFILE` below two — would otherwise spin this loop on an `EAGAIN` read and burn a core.
fn wait_for_more(stderr: &ChildStderr, wake: &OwnedFd) {
    let mut waiting = [
        PollFd::new(stderr, PollFlags::IN),
        PollFd::new(wake, PollFlags::IN),
    ];
    if poll(&mut waiting, Some(&HEARD_BY)).is_err() {
        std::thread::sleep(crate::POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use rustix::process::{Pid, Signal, kill_process};

    /// How long a test waits for a `finish` that is supposed to be bounded — twice the longest
    /// grace a test passes, and a third of the survivor's life, which must not be what ends it.
    const NEVER_RETURNS: Duration = Duration::from_secs(10);

    /// A process holding a child's stderr after the child is gone, killed when the test ends,
    /// panic included. It outlives [`NEVER_RETURNS`] three times over, so a reader that only
    /// stops at EOF stays parked for the whole test, and self-limits if the guard cannot run.
    ///
    /// Load-bearing for the fd assertions, not only for cleanup: while it holds the write end
    /// the pipe's inode cannot be freed, so no sibling test thread can be handed the same
    /// `pipe:[n]` and read as a false failure.
    struct Survivor(Option<u32>);

    impl Drop for Survivor {
        fn drop(&mut self) {
            if let Some(pid) = self.0.and_then(|p| Pid::from_raw(p as i32)) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
    }

    /// Spawn `sh -c script` with its stderr piped.
    fn child(script: &str) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh is runnable")
    }

    /// A child that says `said`, leaves a process holding its stderr, and exits.
    fn child_with_survivor(said: &str) -> (Child, Survivor) {
        let mut c = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '{said}\\n' >&2; sleep 30 & echo $!"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh is runnable");
        let mut pid = String::new();
        let read = BufReader::new(c.stdout.take().expect("piped stdout")).read_line(&mut pid);
        // Guarded before anything that can panic: the sleeper exists from the moment `sh` runs.
        let survivor = Survivor(pid.trim().parse().ok());
        read.expect("sh reports the pid it backgrounded");
        assert!(survivor.0.is_some(), "sh reported {pid:?}, not a pid");
        c.wait().expect("the child exits, the survivor does not");
        (c, survivor)
    }

    /// What `finish` produced, and how long it took. Runs off-thread so a reader with no exit
    /// fails the test rather than hanging the suite.
    fn finish_within(tail: StderrTail, grace: Duration) -> (String, Duration) {
        let (tx, rx) = mpsc::channel();
        let t0 = Instant::now();
        std::thread::spawn(move || tx.send(tail.finish(grace)));
        let said = rx
            .recv_timeout(NEVER_RETURNS)
            .expect("finish must return; a reader parked in read() never does");
        (said, t0.elapsed())
    }

    #[test]
    fn the_read_end_is_non_blocking() {
        // Blocking, a read with nothing to read parks past every look at the stop flag, and the
        // wakeup cannot reach a thread that is no longer in `poll`. `Xvfb` holds its tail for
        // the server's lifetime, and a healthy X server is silent, so that is `drop` hanging.
        let (mut c, _survivor) = child_with_survivor("the fatal line");
        let stderr = c.stderr.take().expect("piped stderr");
        let fd = stderr.as_raw_fd();
        // Held for the whole assertion, so this reads our own pipe and not a reused number.
        let _tail = StderrTail::drain(stderr).expect("a reader");

        let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).expect("fdinfo");
        let flags = fdinfo
            .lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .expect("fdinfo reports the open file description's flags");
        let bits = u32::from_str_radix(flags.trim(), 8).expect("octal");
        assert!(
            bits & 0o4000 != 0,
            "the read end must carry O_NONBLOCK, but fdinfo says flags:{flags}"
        );
    }

    #[test]
    fn debug_summarises_the_capture_instead_of_dumping_it() {
        // Derived, this renders every kept byte as a decimal — into `Xvfb`'s own derived `Debug`,
        // and from there into whatever printed the struct holding it.
        let (mut c, _survivor) = child_with_survivor("the fatal line");
        let tail = StderrTail::drain(c.stderr.take().expect("piped stderr")).expect("a reader");
        assert!(
            crate::await_condition(Duration::from_secs(5), || !format!("{tail:?}")
                .contains("0 bytes")),
            "nothing was captured, so a dump would have had nothing to show either"
        );

        let rendered = format!("{tail:?}");
        assert!(
            rendered.contains("15 bytes"),
            "a count, not a dump: {rendered}"
        );
        assert!(
            !rendered.contains("116"),
            "the bytes themselves must not be in it: {rendered}"
        );
    }

    #[test]
    fn finish_returns_what_was_said_while_a_survivor_holds_the_pipe() {
        // The write end is inherited by everything the child spawned, so EOF is not a deadline
        // the reader can count on reaching (glass#471).
        let (mut c, _survivor) = child_with_survivor("the fatal line");
        let tail = StderrTail::drain(c.stderr.take().expect("piped stderr")).expect("a reader");

        let (said, took) = finish_within(tail, Duration::from_millis(200));

        assert_eq!(said.trim(), "the fatal line");
        assert!(
            took < Duration::from_secs(2),
            "the grace is the wait, not a floor: took {took:?}"
        );
    }

    #[test]
    fn finish_releases_the_pipe_a_survivor_holds_open() {
        // A `finish` that gives up on a still-open pipe without ending its reader leaves the
        // fd held for the process's life.
        let (mut c, _survivor) = child_with_survivor("the fatal line");
        let stderr = c.stderr.take().expect("piped stderr");
        let link = format!("/proc/self/fd/{}", stderr.as_raw_fd());
        let held = std::fs::read_link(&link).expect("the read end is open");
        let tail = StderrTail::drain(stderr).expect("a reader");

        finish_within(tail, Duration::from_millis(200));

        // The survivor still holds the write end, so this pipe cannot be reopened by anything
        // else: finding the same target again means the same fd, still ours.
        assert!(
            std::fs::read_link(&link).ok().is_none_or(|now| now != held),
            "the reader must close its end, but {link} still points at {held:?}"
        );
    }

    #[test]
    fn dropping_a_tail_instead_of_finishing_it_still_releases_the_pipe() {
        // `finish` is how the capture is read, not how the reader is ended.
        let (mut c, _survivor) = child_with_survivor("the fatal line");
        let stderr = c.stderr.take().expect("piped stderr");
        let link = format!("/proc/self/fd/{}", stderr.as_raw_fd());
        let held = std::fs::read_link(&link).expect("the read end is open");

        drop(StderrTail::drain(stderr).expect("a reader"));

        assert!(
            std::fs::read_link(&link).ok().is_none_or(|now| now != held),
            "a dropped tail must end its reader too, but {link} still points at {held:?}"
        );
    }

    #[test]
    fn the_reader_stops_keeping_bytes_at_the_documented_cap() {
        // A child dumping megabytes must cost bounded memory. The range is half the assertion:
        // against the constant alone, shrinking the cap to a byte would read as agreement.
        assert!(
            (5 * 1024..64 * 1024).contains(&STDERR_KEPT),
            "the cap must hold an X server's option table without holding a log: {STDERR_KEPT}"
        );
        let mut c = child("dd if=/dev/zero bs=1024 count=64 2>/dev/null | tr '\\0' e >&2");
        let tail = StderrTail::drain(c.stderr.take().expect("piped stderr")).expect("a reader");
        // Not `wait`: the child writes exactly one pipe buffer, so a reader that stopped
        // draining would hang the suite here instead of failing it.
        assert!(
            crate::await_condition(Duration::from_secs(10), || matches!(
                c.try_wait(),
                Ok(Some(_))
            )),
            "the child must not block writing 64 KiB into a drained pipe"
        );
        assert_eq!(
            finish_within(tail, Duration::from_secs(5)).0.len(),
            STDERR_KEPT
        );
    }

    #[test]
    fn a_child_that_floods_stderr_past_the_cap_is_never_blocked_writing() {
        // Past the cap the pipe must still be drained. Stop reading and the child blocks in
        // write() once the 64 KiB pipe buffer fills, which every caller reads as a hang.
        let mut c = child("dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' e >&2");
        let tail = StderrTail::drain(c.stderr.take().expect("piped stderr")).expect("a reader");
        let exited = crate::await_condition(Duration::from_secs(10), || {
            matches!(c.try_wait(), Ok(Some(_)))
        });
        assert!(
            exited,
            "1 MiB of stderr must not block the child writing it"
        );
        assert!(c.wait().expect("already exited").success());
        finish_within(tail, Duration::from_secs(5));
    }

    #[test]
    fn finish_waits_out_the_grace_for_a_line_that_has_not_arrived_yet() {
        // The diagnostics arrive after the caller asks for them. Read without waiting and a
        // failed start reports an empty stderr — the only clue it had.
        let mut c = child("sleep 0.3; printf 'the fatal line\\n' >&2");
        let tail = StderrTail::drain(c.stderr.take().expect("piped stderr")).expect("a reader");
        let (said, took) = finish_within(tail, Duration::from_secs(5));
        c.wait().expect("the child exits");
        assert_eq!(said.trim(), "the fatal line");
        // The other half: a `finish` that always burned its whole grace would satisfy the line
        // above and cost every caller the ceiling on every call.
        assert!(
            took < Duration::from_secs(2),
            "the grace is a ceiling, not a floor: took {took:?}"
        );
    }
}
