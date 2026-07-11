//! Streams the device's unified log via `xcrun simctl spawn <udid> log stream` into a
//! drainable buffer, mirroring the `LogcatStream` pattern in `glass-android`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use glass_core::Stream;

/// Buffered lines plus the one-shot readiness flags the pump thread signals.
#[derive(Default)]
struct Inner {
    lines: Vec<(Stream, String)>,
    /// Set once the pump delivers its first line: the `log stream` subscription is active, so
    /// the stream is provably live.
    first_line: bool,
    /// Set once the pump's read loop ends (EOF) — the child died / produced nothing — or when
    /// there is no child at all. Distinct from `first_line` so a dead stream unblocks a waiter
    /// without ever claiming to be live.
    finished: bool,
}

/// Thread-safe line buffer shared between the pump thread and `drain`, carrying a readiness
/// signal so a caller can wait until the stream has proven itself live before proceeding.
#[derive(Clone, Default)]
pub struct SharedLog(Arc<(Mutex<Inner>, Condvar)>);

impl SharedLog {
    /// Append a line. A poisoned lock (some other thread already panicked while holding it)
    /// simply drops the line rather than panicking here too, matching `glass-android`'s
    /// `LogSink`. The first line also flips the readiness signal and wakes any waiter.
    pub fn push(&self, stream: Stream, line: String) {
        let (lock, cv) = &*self.0;
        if let Ok(mut inner) = lock.lock() {
            inner.lines.push((stream, line));
            if !inner.first_line {
                inner.first_line = true;
                cv.notify_all();
            }
        }
    }

    /// Mark the stream finished: the pump reached EOF, or no child was ever spawned. This
    /// wakes a readiness waiter so it stops waiting for a line that will never come.
    pub fn mark_done(&self) {
        let (lock, cv) = &*self.0;
        if let Ok(mut inner) = lock.lock() {
            if !inner.finished {
                inner.finished = true;
                cv.notify_all();
            }
        }
    }

    /// Block until the stream proves live (a first line arrived), or is known dead (EOF / no
    /// child), or `timeout` elapses. Returns `true` only when a line was seen. A dead or
    /// absent stream returns `false` at once rather than burning the full `timeout`.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let (lock, cv) = &*self.0;
        // Recover the guard on poison rather than bailing to `false`: the lock holders run no
        // panic-prone code so poison is unreachable, but were it ever poisoned the real
        // `first_line` state is still the honest answer — symmetric with the post-wait
        // recovery below.
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let guard = match cv
            .wait_timeout_while(guard, timeout, |inner| !inner.first_line && !inner.finished)
        {
            Ok((guard, _)) => guard,
            Err(poisoned) => poisoned.into_inner().0,
        };
        guard.first_line
    }

    /// Take and clear all buffered lines.
    pub fn drain(&self) -> Vec<(Stream, String)> {
        let (lock, _cv) = &*self.0;
        lock.lock()
            .map(|mut g| std::mem::take(&mut g.lines))
            .unwrap_or_default()
    }
}

/// A running `xcrun simctl spawn <udid> log stream` whose lines feed a [`SharedLog`].
pub struct LogStream {
    child: Option<Child>,
    buf: SharedLog,
}

impl LogStream {
    /// Stream the device's unified log. `log stream` prints all system logs; that is
    /// noisy but honest — callers filter.
    ///
    /// Best-effort: if `xcrun` fails to spawn, `child` is `None` and `drain` simply
    /// yields nothing, matching the sibling Android backend's behavior when `adb` is
    /// unavailable.
    pub fn spawn(udid: &str) -> Self {
        let buf = SharedLog::default();
        // Take the piped stdout before storing `child` — reading `child.stdout` after
        // moving `child` into `Self` would not borrow-check.
        let mut child = Command::new("xcrun")
            .args([
                "simctl", "spawn", udid, "log", "stream", "--style", "compact",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        let out = child.as_mut().and_then(|c| c.stdout.take());
        if let Some(out) = out {
            let sink = buf.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    sink.push(Stream::Stdout, line);
                }
                // EOF: the child exited (or was killed) — no more lines will arrive. Signal
                // readiness-finished so a waiter blocked in `wait_until_ready` stops waiting.
                sink.mark_done();
            });
        } else {
            // No child or no stdout pipe (spawn failed): nothing will ever be delivered, so
            // the stream is immediately finished — a readiness wait returns `false` at once
            // instead of blocking for the full timeout.
            buf.mark_done();
        }
        Self { child, buf }
    }

    /// Block until the stream is confirmed live (its first line arrived), or known dead / not
    /// spawned, or `timeout` elapses. Returns `true` only when the stream proved live. Used to
    /// gate `simctl launch` on an active subscription so launch-time log lines are not lost to
    /// the live-tail race.
    pub fn wait_until_ready(&self, timeout: Duration) -> bool {
        self.buf.wait_ready(timeout)
    }

    /// Take and clear all buffered lines since the last drain.
    pub fn drain(&self) -> Vec<(Stream, String)> {
        self.buf.drain()
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            // Reap it so it doesn't linger as a zombie once killed.
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::Stream;
    use std::time::Duration;

    #[test]
    fn drain_returns_and_clears_buffer() {
        let buf = SharedLog::default();
        buf.push(Stream::Stdout, "line-1".into());
        buf.push(Stream::Stdout, "line-2".into());
        assert_eq!(
            buf.drain(),
            vec![
                (Stream::Stdout, "line-1".to_string()),
                (Stream::Stdout, "line-2".to_string()),
            ]
        );
        assert!(buf.drain().is_empty(), "second drain is empty");
    }

    #[test]
    fn wait_ready_is_false_immediately_when_nothing_seen() {
        // No line, no EOF: the stream has not proven itself live, so a zero-timeout wait
        // returns `false` at once (it does not block for the cap).
        let buf = SharedLog::default();
        assert!(!buf.wait_ready(Duration::ZERO));
    }

    #[test]
    fn wait_ready_is_true_after_a_line() {
        // One delivered line proves the subscription is active — the stream is live.
        let buf = SharedLog::default();
        buf.push(Stream::Stdout, "some system line".into());
        assert!(buf.wait_ready(Duration::ZERO));
    }

    #[test]
    fn wait_ready_is_false_after_eof_without_a_line() {
        // The pump reached EOF (the `log stream` child died / produced nothing) before any
        // line: not live, and it must return immediately rather than burn the full cap.
        let buf = SharedLog::default();
        buf.mark_done();
        assert!(!buf.wait_ready(Duration::ZERO));
    }

    #[test]
    fn wait_ready_wakes_when_a_line_arrives() {
        // A line pushed from another thread wakes a blocked waiter well before the cap.
        let buf = SharedLog::default();
        let other = buf.clone();
        std::thread::spawn(move || other.push(Stream::Stdout, "some system line".into()));
        assert!(buf.wait_ready(Duration::from_secs(2)));
    }
}
