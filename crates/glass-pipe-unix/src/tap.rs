use std::io::{ErrorKind, PipeReader, PipeWriter, Read};
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

/// How long an idle reader sleeps before looking at the stop flag again — the backstop for a
/// wakeup that never arrives, not what a stop normally waits for.
const HEARD_BY: Timespec = Timespec {
    tv_sec: 5,
    tv_nsec: 0,
};

/// A cap on the drain after the stop flag, so a survivor still writing cannot hold the stop open.
/// At least one pipe buffer on either platform — Linux defaults to 64 KiB, macOS to 16.
const FINAL_DRAIN: usize = 64 * 1024;

/// How many `EINTR`s the final drain will absorb. It takes no bytes on one, so without a budget a
/// persistent signal spins there.
const DRAIN_INTERRUPTS: u8 = 8;

/// What a `poll` that errored falls back to, rather than spinning the loop on an `EAGAIN` read.
const POLL_ERROR_BACKOFF: Duration = Duration::from_millis(20);

/// How much one read asks for.
const CHUNK: usize = 4096;

/// What to do with what a [`PipeTap`] read, and what to do once no more is coming.
pub trait ChunkSink {
    /// Bytes as they arrive, in order, at whatever boundaries the pipe delivered them.
    fn chunk(&mut self, bytes: &[u8]);
    /// The reader is leaving: EOF, an unreadable pipe, or a stop. Anything held back — a line
    /// still waiting for its newline — has no later chance.
    fn end(&mut self) {}
}

/// A child's pipe, read on a helper thread until it closes or the tap ends it.
///
/// Not a read at the end: a child whose output nobody drains stalls on a full pipe, which the
/// caller reads as a process that never came up.
///
/// The write end is inherited by everything the child spawned, so a process outliving the teardown
/// holds the pipe open and a reader that could only stop at EOF parks there holding an fd — one
/// per stream per launch, for the life of a `glass-mcp serve --http` (glass#477).
pub struct PipeTap {
    /// What actually stops the reader. A flag rather than the wakeup below, so a wakeup that
    /// never arrives costs [`HEARD_BY`] rather than the life of the process.
    stopping: Arc<AtomicBool>,
    /// Wakes a reader asleep in `poll` so it reads the flag now instead of at [`HEARD_BY`]. The
    /// write end of a self-pipe nobody drains, so one byte stays readable and the next poll is
    /// certain to see it.
    ///
    /// Do not close it while the reader lives: `poll` would return `POLLHUP` forever, spinning the
    /// loop on a flag that is still false. `stop` joins first, and `Drop` runs `stop` before any
    /// field is dropped.
    wake: PipeWriter,
    /// Taken by whichever of [`PipeTap::stop`] and `drop` runs first.
    reader: Option<JoinHandle<()>>,
}

impl PipeTap {
    /// Start reading `source` into `sink` on a thread called `name`.
    ///
    /// `Err` is the host refusing a resource the bounded read is built from: the thread —
    /// `Builder`, where `thread::spawn` panics (glass#454) — the wakeup pipe, or the non-blocking
    /// mode. `source` is consumed either way, so on `Err` the child takes `EPIPE` rather than
    /// stalling on a pipe nobody drains.
    pub fn start<R, S>(source: R, name: &str, mut sink: S) -> std::io::Result<PipeTap>
    where
        R: Read + AsFd + Send + 'static,
        S: ChunkSink + Send + 'static,
    {
        // Blocking, a read with nothing to read parks past every look at the flag. `pipe(2)` gives
        // the two ends separate open file descriptions, so this never reaches the child's.
        fcntl_setfl(&source, fcntl_getfl(&source)?.union(OFlags::NONBLOCK))?;
        // `std::io::pipe` rather than rustix's `pipe_with(CLOEXEC)`: that is `pipe2(2)`, which
        // macOS lacks and rustix gates out there. std sets close-on-exec on both platforms.
        let (woken, wake) = std::io::pipe()?;
        let stopping: Arc<AtomicBool> = Arc::default();
        let stop = Arc::clone(&stopping);
        let reader = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                read_until_stopped(source, &woken, &stop, &mut sink);
                sink.end();
            })?;
        Ok(PipeTap {
            stopping,
            wake,
            reader: Some(reader),
        })
    }

    /// Whether the reader has left and the sink has been told so. A caller waiting on a child's
    /// last words watches this rather than guessing a sleep.
    ///
    /// The thread's own state, not a flag stored on the way out: that is never stored when the
    /// reader unwinds, and a caller would wait out its whole grace for a thread already gone.
    pub fn is_done(&self) -> bool {
        self.reader
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Stop the reader and wait for it to go, which is what makes the pipe closed rather than
    /// assumed closed. Idempotent, because `drop` runs it too.
    pub fn stop(&mut self) {
        let Some(reader) = self.reader.take() else {
            return;
        };
        // Set before the wake, or a reader the write pulls out of `poll` finds the flag still
        // false and goes back to sleep for another `HEARD_BY`.
        self.stopping.store(true, Ordering::Release);
        // Retried: a signal landing here drops the byte and leaves the reader asleep until
        // `HEARD_BY`, inside a teardown budget measured in seconds.
        let _ = rustix::io::retry_on_intr(|| rustix::io::write(&self.wake, &[1u8]));
        let _ = reader.join();
    }
}

impl Drop for PipeTap {
    /// A tap nobody stopped still ends its reader.
    fn drop(&mut self) {
        self.stop();
    }
}

/// Hides whatever the sink is holding: a derived `Debug` would render a capture, or a log buffer,
/// into whatever printed the struct holding this.
impl std::fmt::Debug for PipeTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeTap")
            .field("done", &self.is_done())
            .finish_non_exhaustive()
    }
}

/// Read `source` into `sink` until it ends or `stopping` says to.
fn read_until_stopped<R: Read + AsFd, S: ChunkSink>(
    mut source: R,
    woken: &PipeReader,
    stopping: &AtomicBool,
    sink: &mut S,
) {
    let mut chunk = [0u8; CHUNK];
    // Read before every read, not only when the pipe runs dry, so a child that never stops
    // writing cannot keep the reader here either.
    while !stopping.load(Ordering::Acquire) {
        match source.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => sink.chunk(&chunk[..n]),
            Err(e) => match e.kind() {
                // Nothing to read yet, or the read was interrupted: back to the wait.
                ErrorKind::WouldBlock | ErrorKind::Interrupted => wait_for_more(&source, woken),
                // Anything else will not read better next time.
                _ => return,
            },
        }
    }
    // Only reached via the flag, which the loop leaves on without reading — so without this a
    // teardown loses the last thing the app wrote, which is what a failed one is diagnosed from.
    final_drain(&mut source, &mut chunk, sink);
}

/// Read what is already in the pipe, at most [`FINAL_DRAIN`] bytes, without waiting for more.
///
/// The read is non-blocking, so `WouldBlock` is the pipe being empty right now — the end of the
/// drain, not a reason to wait.
fn final_drain<R: Read, S: ChunkSink>(source: &mut R, chunk: &mut [u8; CHUNK], sink: &mut S) {
    let mut taken = 0;
    let mut interrupts = 0;
    while taken < FINAL_DRAIN {
        let room = (FINAL_DRAIN - taken).min(CHUNK);
        match source.read(&mut chunk[..room]) {
            Ok(0) => return,
            Ok(n) => {
                taken += n;
                sink.chunk(&chunk[..n]);
            }
            // An interrupt takes no bytes, so `taken` cannot be what ends the loop on one.
            Err(e) if e.kind() == ErrorKind::Interrupted && interrupts < DRAIN_INTERRUPTS => {
                interrupts += 1;
            }
            Err(_) => return,
        }
    }
}

/// Sleep until the pipe has more to say, the wakeup is rung, or [`HEARD_BY`] elapses. The caller
/// re-reads whichever it was, so none of the three needs telling apart.
///
/// A poll that errors sleeps rather than returning at once: a persistent one — `EINVAL` under an
/// `RLIMIT_NOFILE` below two — would spin this loop on an `EAGAIN` read and burn a core.
fn wait_for_more<R: AsFd>(source: &R, woken: &PipeReader) {
    let mut waiting = [
        PollFd::new(source, PollFlags::IN),
        PollFd::new(woken, PollFlags::IN),
    ];
    if poll(&mut waiting, Some(&HEARD_BY)).is_err() {
        std::thread::sleep(POLL_ERROR_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the `/proc`-based assertions below use it, and they are Linux-only.
    #[cfg(target_os = "linux")]
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::testsup::{child_with_survivor, said_then_survived};

    /// How long a test waits for a `stop` that is supposed to be bounded — a third of the
    /// survivor's life, which must not be what ends it.
    const NEVER_RETURNS: Duration = Duration::from_secs(10);

    /// The most a collector keeps. Past it only the count grows.
    ///
    /// Bounded on purpose: several fixtures here read from something that never ends, so a mutant
    /// that breaks the cap or the stop flag turns them into an unbounded read. Storing every byte
    /// then exhausts the host's memory and takes the whole CI job down — which reports as a runner
    /// shutdown, not as the one missed mutant it is.
    const COLLECTED_KEPT: usize = 128 * 1024;

    /// Collects what it is handed, keeping at most [`COLLECTED_KEPT`] bytes, counting all of them,
    /// and recording whether `end` ran.
    #[derive(Clone, Default)]
    struct Collected(Arc<Mutex<Held>>);

    #[derive(Default)]
    struct Held {
        kept: Vec<u8>,
        total: usize,
        ended: bool,
    }

    impl ChunkSink for Collected {
        fn chunk(&mut self, bytes: &[u8]) {
            let mut held = self.0.lock().expect("collector");
            held.total += bytes.len();
            let room = COLLECTED_KEPT.saturating_sub(held.kept.len());
            held.kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
        }
        fn end(&mut self) {
            self.0.lock().expect("collector").ended = true;
        }
    }

    impl Collected {
        fn read(&self) -> (String, bool) {
            let held = self.0.lock().expect("collector");
            (String::from_utf8_lossy(&held.kept).into_owned(), held.ended)
        }

        fn total(&self) -> usize {
            self.0.lock().expect("collector").total
        }
    }

    /// Stop `tap` off-thread, so a reader with no exit fails the test rather than hanging the
    /// suite. Returns how long the stop took.
    fn stop_within(mut tap: PipeTap) -> Duration {
        let (tx, rx) = mpsc::channel();
        let t0 = Instant::now();
        std::thread::spawn(move || {
            tap.stop();
            tx.send(())
        });
        rx.recv_timeout(NEVER_RETURNS)
            .expect("stop must return; a reader parked in read() never does");
        t0.elapsed()
    }

    /// A `Read` with far more to give than the drain may take: `per_read` bytes at a time until
    /// `left` runs out, then an empty pipe.
    ///
    /// `per_read` divides neither [`CHUNK`] nor [`FINAL_DRAIN`], so the last read the cap allows is
    /// a short one and the remaining-room arithmetic is what decides the total — chunk-aligned, an
    /// error in it lands on a boundary and cancels out.
    ///
    /// `left` rather than truly endless so that a drain which reads past its cap gives a wrong
    /// *finite* answer instead of an unbounded one. Endless, the same mutant exhausts the host's
    /// memory and takes the CI job with it.
    struct Flooding {
        per_read: usize,
        left: usize,
    }

    impl Flooding {
        /// Twice what the drain may take, so reading past the cap is unmistakable and still ends.
        fn twice_the_cap() -> Self {
            Flooding {
                per_read: 3000,
                left: FINAL_DRAIN * 2,
            }
        }
    }

    impl Read for Flooding {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                !buf.is_empty(),
                "the drain must not ask for zero bytes; it has room or it is finished"
            );
            let n = buf.len().min(self.per_read).min(self.left);
            if n == 0 {
                return Err(std::io::Error::from(ErrorKind::WouldBlock));
            }
            self.left -= n;
            buf[..n].fill(b'x');
            Ok(n)
        }
    }

    /// A `Read` that reports `Interrupted` `left` times, then hands back `bytes` once, then says
    /// the pipe is empty.
    struct InterruptedThen {
        left: u32,
        bytes: usize,
    }

    impl Read for InterruptedThen {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.left > 0 {
                self.left -= 1;
                return Err(std::io::Error::from(ErrorKind::Interrupted));
            }
            match std::mem::take(&mut self.bytes) {
                0 => Err(std::io::Error::from(ErrorKind::WouldBlock)),
                n => {
                    buf[..n].fill(b'y');
                    Ok(n)
                }
            }
        }
    }

    /// A `Read` that yields `first` once and then reports what `then` says.
    struct ThenErr {
        first: Option<usize>,
        then: ErrorKind,
    }

    impl Read for ThenErr {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.first.take() {
                Some(n) => {
                    buf[..n].fill(b'y');
                    Ok(n)
                }
                None => Err(std::io::Error::from(self.then)),
            }
        }
    }

    /// Bytes `final_drain` handed the sink, driving it directly — through a real pipe it cannot be
    /// told apart from the main loop, which races it, so ablating it leaves those tests green.
    fn drained(source: impl Read) -> String {
        drain_all(source).0
    }

    /// What `final_drain` took from `source`: the bytes it kept, and how many it read in total.
    fn drain_all(mut source: impl Read) -> (String, usize) {
        let collected = Collected::default();
        let mut sink = collected.clone();
        let mut chunk = [0u8; CHUNK];
        final_drain(&mut source, &mut chunk, &mut sink);
        (collected.read().0, collected.total())
    }

    #[test]
    fn the_final_drain_stops_at_the_cap_rather_than_reading_on() {
        // The cap is the whole bound against a survivor that keeps writing. Counted, not kept: the
        // collector holds far less than the cap on purpose.
        assert_eq!(drain_all(Flooding::twice_the_cap()).1, FINAL_DRAIN);
    }

    #[test]
    fn the_final_drain_absorbs_its_whole_interrupt_budget_and_still_takes_what_follows() {
        // An interrupt takes no bytes, so ending the drain on one loses what the app already
        // wrote. The budget is spent exactly here, and the read after it must still be taken.
        assert_eq!(
            drained(InterruptedThen {
                left: DRAIN_INTERRUPTS.into(),
                bytes: 5,
            }),
            "yyyyy"
        );
    }

    #[test]
    fn the_final_drain_gives_up_one_interrupt_past_the_budget() {
        // The other edge, and what makes the budget a bound rather than a number: one more
        // interrupt than it allows ends the drain, whatever would have followed.
        assert_eq!(
            drained(InterruptedThen {
                left: u32::from(DRAIN_INTERRUPTS) + 1,
                bytes: 5,
            }),
            ""
        );
    }

    #[test]
    fn the_final_drain_ends_on_an_empty_pipe_rather_than_waiting() {
        // Non-blocking, so `WouldBlock` ends the drain; waiting on it would put teardown behind a
        // survivor's silence.
        assert_eq!(
            drained(ThenErr {
                first: Some(4),
                then: ErrorKind::WouldBlock,
            }),
            "yyyy"
        );
    }

    #[test]
    fn the_final_drain_ends_at_eof() {
        assert_eq!(
            drained(ThenErr {
                first: Some(2),
                then: ErrorKind::UnexpectedEof,
            }),
            "yy"
        );
    }

    #[test]
    fn a_tap_over_a_pipe_that_closes_ends_itself() {
        // The EOF exit, which every other test here deliberately never reaches — its survivor
        // holds the pipe open. Without it, `StderrTail::finish` has nothing to wait on.
        let mut c = Command::new("sh")
            .arg("-c")
            .arg("printf 'all of it\n' >&2")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh is runnable");
        let collected = Collected::default();
        let tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            collected.clone(),
        )
        .expect("a reader");
        c.wait().expect("the child exits");

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !tap.is_done() {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            tap.is_done(),
            "a closed pipe must end its reader on its own"
        );
        assert_eq!(collected.read(), ("all of it\n".to_string(), true));
    }

    #[test]
    fn a_tap_whose_pipe_a_survivor_holds_open_is_not_done() {
        // The other half: `is_done` must not report a reader still parked on a live pipe as gone,
        // or a caller waiting for a child's last words stops waiting at once.
        let (mut c, _survivor) = said_then_survived("the last line\n");
        let tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            Collected::default(),
        )
        .expect("a reader");

        assert!(!tap.is_done(), "the survivor still holds the write end");
    }

    #[test]
    fn debug_reports_whether_the_reader_left_and_never_the_capture() {
        // Derived, this would render the sink — a whole log buffer — into whatever printed the
        // struct holding it.
        let (mut c, _survivor) = said_then_survived("the fatal line\n");
        let tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            Collected::default(),
        )
        .expect("a reader");

        let rendered = format!("{tap:?}");
        assert!(rendered.contains("done: false"), "{rendered}");
        assert!(
            !rendered.contains("fatal"),
            "the capture must not be in it: {rendered}"
        );
    }

    #[test]
    fn a_reader_waits_out_a_quiet_pipe_rather_than_leaving_it() {
        // A pipe with nothing in it yet reads `WouldBlock`. Send that to the same arm as a real
        // error and the reader leaves at the app's first quiet moment, losing everything after.
        let (mut c, _survivor) = child_with_survivor(
            "(printf 'first\n' >&2; sleep 0.4; printf 'second\n' >&2; sleep 30) & echo $!",
        );
        let collected = Collected::default();
        let _tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            collected.clone(),
        )
        .expect("a reader");

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !collected.read().0.contains("second") {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(
            collected.read().0.contains("second"),
            "the reader must still be there after the pause: {:?}",
            collected.read().0
        );
    }

    #[test]
    fn dropping_a_tap_instead_of_stopping_it_still_ends_the_reader() {
        // `Drop` is how every backend ends a tap — none of them call `stop`. Without it a
        // teardown that just lets the field go leaves the reader parked on the survivor's pipe.
        let (mut c, _survivor) = said_then_survived("the last line\n");
        let collected = Collected::default();
        let tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            collected.clone(),
        )
        .expect("a reader");

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(tap);
            tx.send(())
        });
        rx.recv_timeout(NEVER_RETURNS)
            .expect("drop must end the reader; one parked in read() never returns");

        assert!(collected.read().1, "the sink must have been ended");
    }

    #[test]
    fn stop_returns_while_a_survivor_holds_the_pipe_open() {
        // The write end is inherited by everything the child spawned, so EOF is not a deadline any
        // reader can count on reaching (glass#477).
        let (mut c, _survivor) = said_then_survived("the last line\\n");
        let tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            Collected::default(),
        )
        .expect("a reader");

        let took = stop_within(tap);

        assert!(
            took < Duration::from_secs(2),
            "stop must not wait for an EOF that is not coming: took {took:?}"
        );
    }

    // `/proc/self/fd`, so Linux only; the property is not. `stop_returns_while_a_survivor_holds_
    // the_pipe_open` above covers the half that can be asserted anywhere.
    #[test]
    #[cfg(target_os = "linux")]
    fn stop_releases_the_pipe_a_survivor_holds_open() {
        // A tap that stops without ending its reader leaves the fd held for the process's life.
        let (mut c, _survivor) = said_then_survived("the last line\\n");
        let stderr = c.stderr.take().expect("piped stderr");
        let link = format!("/proc/self/fd/{}", stderr.as_raw_fd());
        let held = std::fs::read_link(&link).expect("the read end is open");
        let tap = PipeTap::start(stderr, "test-tap", Collected::default()).expect("a reader");

        stop_within(tap);

        // The survivor still holds the write end, so nothing else can be handed this pipe —
        // finding the same target again means the same fd, still ours.
        assert!(
            std::fs::read_link(&link).ok().is_none_or(|now| now != held),
            "the reader must close its end, but {link} still points at {held:?}"
        );
    }

    #[test]
    fn what_the_child_wrote_before_exiting_survives_the_stop() {
        // The loop leaves at the flag without reading, so a stop straight after the reap would
        // lose the last thing the app said.
        let (mut c, _survivor) = said_then_survived("the last line\\n");
        let collected = Collected::default();
        let mut tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            collected.clone(),
        )
        .expect("a reader");

        tap.stop();

        let (said, ended) = collected.read();
        assert_eq!(said, "the last line\n");
        assert!(ended, "the sink must be told no more is coming");
    }

    // `/proc/self/fdinfo`, so Linux only — macOS exposes no per-fd status flags to read back.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_read_end_is_non_blocking() {
        // Blocking, a read with nothing to read parks past every look at the stop flag, where the
        // wakeup cannot reach it.
        let (mut c, _survivor) = said_then_survived("the last line\\n");
        let stderr = c.stderr.take().expect("piped stderr");
        let fd = stderr.as_raw_fd();
        // Held for the whole assertion, so this reads our own pipe and not a reused number.
        let _tap = PipeTap::start(stderr, "test-tap", Collected::default()).expect("a reader");

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
    fn a_child_that_floods_the_pipe_is_never_blocked_writing() {
        // Stop draining and the child blocks in write() once the pipe buffer fills, which every
        // caller reads as a hang.
        let mut c = Command::new("sh")
            .arg("-c")
            .arg("dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' e >&2")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh is runnable");
        let _tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            Collected::default(),
        )
        .expect("a reader");

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !matches!(c.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            matches!(c.try_wait(), Ok(Some(_))),
            "1 MiB of stderr must not block the child writing it"
        );
    }

    #[test]
    fn the_final_drain_is_bounded_against_a_survivor_that_never_stops_writing() {
        // A survivor still writing must not be able to keep the drain fed. `yes` self-limits once
        // the pipe fills, so it costs nothing while the guard waits to kill it.
        let (mut c, _survivor) = child_with_survivor("yes noise >&2 & echo $!");
        let tap = PipeTap::start(
            c.stderr.take().expect("piped stderr"),
            "test-tap",
            Collected::default(),
        )
        .expect("a reader");

        let took = stop_within(tap);

        assert!(
            took < Duration::from_secs(2),
            "the final drain must be capped, not run as long as somebody keeps writing: {took:?}"
        );
    }
}
