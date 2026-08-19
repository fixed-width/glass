//! A launched app's stdout/stderr, read on a thread that can be stopped.
//!
//! The line splitting is pure and tested on any host; the reader around it is `cfg(windows)`. A
//! reader that ended only at EOF could not be ended at all once something the app spawned
//! inherited the write end and outlived the teardown — it would park in `ReadFile` holding a
//! thread and a handle for the life of the process (glass#477). The unix backends get the same
//! bound from `glass-pipe-unix`, which this crate cannot link.

use std::sync::{Arc, Mutex, PoisonError};

use glass_core::logbuf::Stream;

/// Log lines captured from the app, tagged by stream.
pub type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// Splits what the pipe delivered into lines. Reads land at pipe boundaries, so a line can span
/// any number of them — `pending` is what has been seen of the line still open.
pub struct Lines {
    tag: Stream,
    sink: LogSink,
    pending: Vec<u8>,
}

impl Lines {
    pub fn new(tag: Stream, sink: LogSink) -> Self {
        Lines {
            tag,
            sink,
            pending: Vec::new(),
        }
    }

    /// Feed the next read's bytes in.
    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.emit();
            } else {
                self.pending.push(byte);
            }
        }
    }

    /// No more is coming. A last line that never got its newline is still what the app said — a
    /// crash mid-write, or a process terminated between the text and the terminator. An empty
    /// `pending` is a stream that ended on a newline, and must not become a blank line.
    pub fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.emit();
        }
    }

    /// File `pending` as a line and start the next one.
    ///
    /// Lossy rather than a decode that can fail: the reader this replaces stopped at the first bad
    /// byte, silencing an app for the rest of its run. The trailing `\r` is dropped the way
    /// `BufRead::lines` drops it — Windows apps write CRLF.
    fn emit(&mut self) {
        let line = std::mem::take(&mut self.pending);
        let text = String::from_utf8_lossy(&line);
        let text = text.strip_suffix('\r').unwrap_or(&text).to_owned();
        self.sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((self.tag, text));
    }
}

#[cfg(windows)]
pub(crate) use imp::LogTap;

#[cfg(windows)]
mod imp {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    use super::{Lines, LogSink};
    use glass_core::logbuf::Stream;

    /// How long an idle reader sleeps before looking at the stop flag again. Also the ceiling on
    /// [`LogTap::stop`]'s join.
    const IDLE: Duration = Duration::from_millis(50);

    /// A cap on the drain after the stop flag, so a survivor still writing cannot hold it open.
    const FINAL_DRAIN: usize = 64 * 1024;

    /// How much one read asks for.
    const CHUNK: usize = 4096;

    /// A launched app's stream, read on a helper thread until the pipe breaks or the tap ends it.
    pub(crate) struct LogTap {
        /// What stops the reader. Read before every peek, so a survivor that never stops writing
        /// cannot keep the reader here either.
        stopping: Arc<AtomicBool>,
        /// Taken by whichever of [`LogTap::stop`] and `drop` runs first.
        reader: Option<JoinHandle<()>>,
    }

    impl LogTap {
        /// Start reading `source`, filing each line into `logs` under `tag`.
        ///
        /// `Err` is the host refusing the thread — `Builder`, where `thread::spawn` panics.
        /// `source` is consumed either way, so on `Err` the app's next write to that stream fails
        /// with `ERROR_BROKEN_PIPE`, the analogue of the `EPIPE` the unix siblings name.
        pub(crate) fn start<R>(source: R, tag: Stream, logs: LogSink) -> std::io::Result<LogTap>
        where
            R: Read + AsRawHandle + Send + 'static,
        {
            let stopping: Arc<AtomicBool> = Arc::default();
            let stop = Arc::clone(&stopping);
            let reader = std::thread::Builder::new()
                .name("glass-log-tap".into())
                .spawn(move || read_until_stopped(source, &stop, Lines::new(tag, logs)))?;
            Ok(LogTap {
                stopping,
                reader: Some(reader),
            })
        }

        /// Whether the reader has left and the sink has been flushed — the pipe broke, or
        /// nothing more can be read from it.
        pub(crate) fn is_done(&self) -> bool {
            self.reader
                .as_ref()
                .is_none_or(std::thread::JoinHandle::is_finished)
        }

        /// Stop the reader and wait for it to go, which is what makes the pipe closed rather than
        /// assumed closed. Idempotent, because `drop` runs it too.
        pub(crate) fn stop(&mut self) {
            let Some(reader) = self.reader.take() else {
                return;
            };
            self.stopping.store(true, Ordering::Release);
            let _ = reader.join();
        }
    }

    impl Drop for LogTap {
        /// A tap nobody stopped still ends its reader.
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Hides the sink: a derived `Debug` would render the whole log buffer into whatever printed
    /// the struct holding this.
    impl std::fmt::Debug for LogTap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LogTap")
                // The thread's own state: `reader` is taken only by `stop`, so `is_some` would
                // report a reader that died an hour ago as running.
                .field("done", &self.is_done())
                .finish_non_exhaustive()
        }
    }

    /// How much is readable right now, or `None` once the pipe cannot be read at all — a broken
    /// pipe (every writer gone) or a closed handle.
    fn available<R: AsRawHandle>(source: &R) -> Option<usize> {
        let mut avail: u32 = 0;
        // SAFETY: the handle is `source`'s own, live for this borrow; `avail` is a local this
        // call fills, and every other out-param is opted out of with `None`.
        unsafe {
            PeekNamedPipe(
                HANDLE(source.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut avail),
                None,
            )
        }
        .ok()?;
        Some(avail as usize)
    }

    /// What one peek-and-read did.
    enum Pumped {
        Read(usize),
        /// Nothing readable right now — the app is running and quiet.
        Empty,
        /// Nothing will be readable again.
        Ended,
    }

    /// Peek, then read exactly what the peek reported, so the read itself never waits.
    ///
    /// A blocking `ReadFile` is what makes an EOF-only reader unstoppable: it parks past every
    /// look at the stop flag, and an anonymous pipe offers nothing to interrupt it with.
    fn pump<R: Read + AsRawHandle>(
        source: &mut R,
        chunk: &mut [u8; CHUNK],
        cap: usize,
        lines: &mut Lines,
    ) -> Pumped {
        let Some(avail) = available(source) else {
            return Pumped::Ended;
        };
        if avail == 0 {
            return Pumped::Empty;
        }
        let want = avail.min(cap).min(CHUNK);
        match source.read(&mut chunk[..want]) {
            Ok(0) => Pumped::Ended,
            Ok(n) => {
                lines.push(&chunk[..n]);
                Pumped::Read(n)
            }
            // The peek said the bytes are there, so ending capture over one signal would silence
            // the app for the rest of its run; the unix reader retries the same case.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Pumped::Empty,
            Err(_) => Pumped::Ended,
        }
    }

    /// Read `source` into `lines` until the pipe breaks or `stopping` says to, then tell `lines`
    /// no more is coming.
    ///
    /// `lines` is taken by value and flushed here rather than by the caller: a hand-call beside the
    /// loop is one a refactor can drop with every test still green, losing every last unterminated
    /// line.
    fn read_until_stopped<R: Read + AsRawHandle>(
        mut source: R,
        stopping: &AtomicBool,
        mut lines: Lines,
    ) {
        let mut chunk = [0u8; CHUNK];
        while !stopping.load(Ordering::Acquire) {
            match pump(&mut source, &mut chunk, CHUNK, &mut lines) {
                Pumped::Read(_) => continue,
                Pumped::Empty => std::thread::sleep(IDLE),
                Pumped::Ended => {
                    lines.flush();
                    return;
                }
            }
        }
        final_drain(&mut source, &mut chunk, &mut lines);
        lines.flush();
    }

    /// Read what the pipe already holds, at most [`FINAL_DRAIN`] bytes, without waiting for more.
    ///
    /// Only reached via the stop flag, which the loop leaves on without reading — so without this
    /// a teardown loses the last thing the app wrote, which is what a failed one is diagnosed
    /// from. The peek reports what is there now, so `Empty` ends the drain rather than waiting.
    fn final_drain<R: Read + AsRawHandle>(
        source: &mut R,
        chunk: &mut [u8; CHUNK],
        lines: &mut Lines,
    ) {
        let mut taken = 0;
        while taken < FINAL_DRAIN {
            match pump(source, chunk, FINAL_DRAIN - taken, lines) {
                Pumped::Read(n) => taken += n,
                Pumped::Empty | Pumped::Ended => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the splitter filed, given reads that arrived at these boundaries.
    fn split(chunks: &[&[u8]]) -> Vec<(Stream, String)> {
        let sink: LogSink = Arc::default();
        let mut lines = Lines::new(Stream::Stdout, Arc::clone(&sink));
        for chunk in chunks {
            lines.push(chunk);
        }
        lines.flush();
        sink.lock().expect("sink").clone()
    }

    #[test]
    fn each_line_lands_without_its_terminator() {
        assert_eq!(
            split(&[b"one\r\ntwo\n"]),
            vec![
                (Stream::Stdout, "one".to_string()),
                (Stream::Stdout, "two".to_string())
            ]
        );
    }

    #[test]
    fn a_line_split_across_chunks_arrives_whole() {
        // Reads land at pipe boundaries, not line boundaries.
        assert_eq!(
            split(&[b"one", b"-and-", b"the-same\n"]),
            vec![(Stream::Stdout, "one-and-the-same".to_string())]
        );
    }

    #[test]
    fn a_final_line_with_no_terminator_is_still_emitted() {
        // A crash mid-write, or a process terminated between the text and its newline.
        assert_eq!(
            split(&[b"truncated"]),
            vec![(Stream::Stdout, "truncated".to_string())]
        );
    }

    #[test]
    fn nothing_written_emits_nothing() {
        // `flush` must not invent a trailing empty line out of an empty pending buffer.
        assert!(split(&[]).is_empty());
    }

    #[test]
    fn a_blank_line_between_two_others_is_kept() {
        // Blank lines are content — an app's own paragraph breaks in a stack trace.
        assert_eq!(
            split(&[b"one\n\ntwo\n"]),
            vec![
                (Stream::Stdout, "one".to_string()),
                (Stream::Stdout, String::new()),
                (Stream::Stdout, "two".to_string())
            ]
        );
    }

    #[test]
    fn invalid_utf8_is_replaced_rather_than_ending_the_split() {
        // The reader this replaces stopped at the first bad byte, so one stray byte silenced an
        // app for the rest of its run.
        let out = split(&[b"good\n\xff\nafter\n"]);
        assert_eq!(
            out.len(),
            3,
            "a stray byte must not silence the rest: {out:?}"
        );
        assert_eq!(out[0].1, "good");
        assert_eq!(out[2].1, "after");
    }
}

/// The reader itself, which only exists on Windows. Runs on the Windows CI job, and under wine
/// from a Linux host (`docs/how-to/verify-a-change.md`).
#[cfg(all(test, windows))]
mod reader_tests {
    use super::*;
    use std::io::{PipeReader, PipeWriter};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use glass_core::logbuf::Stream;

    /// A child writing into a pipe whose write end this test also holds, so the pipe stays open
    /// after the child is gone — what a launch leaves behind when something it spawned outlives
    /// the teardown.
    ///
    /// Held here rather than by a detached process: `start /b` does not detach one under wine.
    /// Ablating [`LogTap::stop`]'s flag must hang this test; a fixture where it still passes is
    /// not testing the bound.
    fn child_into_a_pipe_the_test_holds_open(said: &str) -> (Child, PipeReader, PipeWriter) {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let child = Command::new("cmd")
            .args(["/c", &format!("echo {said}")])
            .stdout(Stdio::from(writer.try_clone().expect("dup the write end")))
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd is runnable");
        (child, reader, writer)
    }

    #[test]
    fn the_final_drain_is_bounded_against_a_writer_that_never_stops() {
        // Something still writing must not be able to keep the drain fed. A thread rather than a
        // process, so it needs nothing wine will not detach.
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let stop_writing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writing = {
            let stop_writing = Arc::clone(&stop_writing);
            std::thread::spawn(move || {
                let mut writer = writer;
                while !stop_writing.load(std::sync::atomic::Ordering::Relaxed) {
                    if std::io::Write::write_all(&mut writer, b"noise\n").is_err() {
                        return;
                    }
                }
            })
        };
        let sink: LogSink = Arc::default();
        let mut tap = LogTap::start(reader, Stream::Stdout, sink).expect("a reader");

        let t0 = Instant::now();
        tap.stop();
        let took = t0.elapsed();

        stop_writing.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = writing.join();
        assert!(
            took < Duration::from_secs(5),
            "the final drain must be capped, not run as long as somebody keeps writing: {took:?}"
        );
    }

    #[test]
    fn stopping_returns_and_keeps_what_the_app_said_while_the_pipe_is_still_held_open() {
        let (mut child, reader, _held) = child_into_a_pipe_the_test_holds_open("the last line");
        let sink: LogSink = Arc::default();
        let mut tap = LogTap::start(reader, Stream::Stdout, Arc::clone(&sink)).expect("a reader");
        // Waited for, not slept past: everything the child wrote is in the pipe once it is gone.
        // `_held` keeps the pipe from reaching EOF.
        let _ = child.wait();

        let t0 = Instant::now();
        tap.stop();
        let took = t0.elapsed();

        assert!(
            took < Duration::from_secs(5),
            "stop must not wait for an EOF that is not coming: took {took:?}"
        );
        let said: Vec<String> = sink
            .lock()
            .expect("sink")
            .iter()
            .map(|(_, line)| line.trim().to_string())
            .collect();
        assert!(
            said.iter().any(|line| line == "the last line"),
            "the drain after the stop must keep what the app said: {said:?}"
        );
    }
}
