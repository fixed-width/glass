use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, PoisonError};

use crate::{ChunkSink, PipeTap};

/// A child's stream read as tagged lines into a buffer the caller drains.
///
/// Held by the backend for the app's lifetime and dropped at teardown: the app's stdout/stderr
/// write ends are inherited by everything it spawns, so a reader that ended only at EOF would
/// park on a survivor's pipe (glass#477).
#[derive(Debug)]
pub struct LineTap(PipeTap);

impl LineTap {
    /// Start reading `source`, filing each line into `sink` under `tag`.
    ///
    /// `Err` is the host refusing the thread or the fds — see [`PipeTap::start`], whose failures
    /// these are. `source` is consumed either way.
    pub fn start<S, R>(
        source: R,
        tag: S,
        sink: Arc<Mutex<Vec<(S, String)>>>,
    ) -> std::io::Result<LineTap>
    where
        S: Copy + Send + 'static,
        R: Read + AsFd + Send + 'static,
    {
        PipeTap::start(source, "glass-log-tap", Lines::new(tag, sink)).map(LineTap)
    }

    /// Stop the reader and wait for it to go, releasing the pipe. Idempotent; `drop` runs it too.
    pub fn stop(&mut self) {
        self.0.stop();
    }
}

/// Splits what the pipe delivered into lines. Chunks arrive at pipe boundaries, so a line can
/// span any number of them — `pending` is what has been seen of the line still open.
struct Lines<S> {
    tag: S,
    sink: Arc<Mutex<Vec<(S, String)>>>,
    pending: Vec<u8>,
}

impl<S> Lines<S> {
    fn new(tag: S, sink: Arc<Mutex<Vec<(S, String)>>>) -> Self {
        Lines {
            tag,
            sink,
            pending: Vec::new(),
        }
    }
}

impl<S: Copy> Lines<S> {
    /// File `pending` as a line and start the next one.
    ///
    /// Lossy rather than a decode that can fail: `BufRead::lines` returns `Err` on a bad byte and
    /// the readers this replaces stopped there, so one stray byte silenced an app for the rest of
    /// its run. The trailing `\r` goes the way `BufRead::lines` drops it, so a CRLF-writing app
    /// does not gain one at the end of every line.
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

impl<S: Copy> ChunkSink for Lines<S> {
    fn chunk(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if *byte == b'\n' {
                self.emit();
            } else {
                self.pending.push(*byte);
            }
        }
    }

    /// A last line that never got its newline is still what the app said — a crash mid-write, or a
    /// process killed between the text and the terminator. An empty `pending` is the ordinary case
    /// of a stream that ended on a newline, and must not become a blank line.
    fn end(&mut self) {
        if !self.pending.is_empty() {
            self.emit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{ChildStderr, Command, Stdio};
    use std::time::{Duration, Instant};

    use rustix::process::{Pid, Signal, kill_process};

    /// Stands in for `glass_core::logbuf::Stream`, which this crate does not depend on.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Tag {
        Out,
    }

    /// A process holding the pipe after the child that made it is gone; killed with the test.
    struct Survivor(Option<u32>);

    impl Drop for Survivor {
        fn drop(&mut self) {
            if let Some(pid) = self.0.and_then(|p| Pid::from_raw(p as i32)) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
    }

    /// Run `script`, which must print the pid it backgrounded on stdout, and return its stderr
    /// pipe plus a guard for the survivor.
    fn stderr_of(script: &str) -> (ChildStderr, Survivor) {
        let mut c = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh is runnable");
        let mut pid = String::new();
        let read = BufReader::new(c.stdout.take().expect("piped stdout")).read_line(&mut pid);
        let survivor = Survivor(pid.trim().parse().ok());
        read.expect("sh reports the pid it backgrounded");
        assert!(survivor.0.is_some(), "sh reported {pid:?}, not a pid");
        c.wait().expect("the child exits, the survivor does not");
        (c.stderr.take().expect("piped stderr"), survivor)
    }

    /// What the tap filed after being stopped with the survivor still holding the pipe.
    fn lines_after_stopping(script: &str) -> Vec<(Tag, String)> {
        let (stderr, _survivor) = stderr_of(script);
        let sink: Arc<Mutex<Vec<(Tag, String)>>> = Arc::default();
        let mut tap = LineTap::start(stderr, Tag::Out, Arc::clone(&sink)).expect("a reader");
        tap.stop();
        sink.lock().expect("sink").clone()
    }

    #[test]
    fn each_line_lands_tagged_and_without_its_newline() {
        let lines = lines_after_stopping("printf 'one\\ntwo\\n' >&2; sleep 30 & echo $!");
        assert_eq!(
            lines,
            vec![(Tag::Out, "one".to_string()), (Tag::Out, "two".to_string())]
        );
    }

    #[test]
    fn a_final_line_with_no_newline_is_still_emitted() {
        // `BufRead::lines` yields it; a splitter that only emitted on '\n' would drop the last
        // thing a crashing app said.
        let lines = lines_after_stopping("printf 'one\\ntruncated' >&2; sleep 30 & echo $!");
        assert_eq!(
            lines,
            vec![
                (Tag::Out, "one".to_string()),
                (Tag::Out, "truncated".to_string())
            ]
        );
    }

    #[test]
    fn a_trailing_carriage_return_is_not_part_of_the_line() {
        // `BufRead::lines` strips it, so a CRLF-writing app must not gain a stray '\r' at the end
        // of every captured line.
        let lines = lines_after_stopping("printf 'one\\r\\n' >&2; sleep 30 & echo $!");
        assert_eq!(lines, vec![(Tag::Out, "one".to_string())]);
    }

    #[test]
    fn a_blank_line_between_two_others_is_kept() {
        // Blank lines are content — an app's own paragraph breaks in a stack trace.
        let lines = lines_after_stopping("printf 'one\\n\\ntwo\\n' >&2; sleep 30 & echo $!");
        assert_eq!(
            lines,
            vec![
                (Tag::Out, "one".to_string()),
                (Tag::Out, String::new()),
                (Tag::Out, "two".to_string())
            ]
        );
    }

    #[test]
    fn nothing_written_emits_nothing() {
        // `end` must not invent a trailing empty line out of an empty pending buffer.
        let lines = lines_after_stopping("sleep 30 & echo $!");
        assert!(lines.is_empty(), "expected no lines, got {lines:?}");
    }

    #[test]
    fn invalid_utf8_is_replaced_rather_than_ending_the_reader() {
        // `BufRead::lines` returns `Err(InvalidData)` here and the reader this replaces stopped
        // on it, losing every later line — an app that wrote one stray byte went quiet.
        let lines =
            lines_after_stopping("printf 'good\\n\\377\\nafter\\n' >&2; sleep 30 & echo $!");
        assert_eq!(
            lines.len(),
            3,
            "the reader must not stop at bad bytes: {lines:?}"
        );
        assert_eq!(lines[0].1, "good");
        assert_eq!(lines[2].1, "after");
    }

    #[test]
    fn a_line_split_across_reads_arrives_whole() {
        // Chunks arrive at pipe boundaries, not line boundaries.
        let lines = lines_after_stopping(
            "head -c 9000 /dev/zero | tr '\\0' x >&2; printf '\\n' >&2; sleep 30 & echo $!",
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].1.len(),
            9000,
            "the line must not be cut at a chunk edge"
        );
    }

    #[test]
    fn stop_is_bounded_when_a_survivor_holds_the_pipe() {
        let (stderr, _survivor) = stderr_of("printf 'one\\n' >&2; sleep 30 & echo $!");
        let sink: Arc<Mutex<Vec<(Tag, String)>>> = Arc::default();
        let mut tap = LineTap::start(stderr, Tag::Out, sink).expect("a reader");

        let t0 = Instant::now();
        tap.stop();

        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop must not wait for an EOF that is not coming: {:?}",
            t0.elapsed()
        );
    }
}
