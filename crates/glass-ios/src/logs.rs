//! Streams the device's unified log via `xcrun simctl spawn <udid> log stream` into a
//! drainable buffer, mirroring the `LogcatStream` pattern in `glass-android`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use glass_core::Stream;

/// Thread-safe line buffer shared between the pump thread and `drain`.
#[derive(Clone, Default)]
pub struct SharedLog(Arc<Mutex<Vec<(Stream, String)>>>);

impl SharedLog {
    /// Append a line. A poisoned lock (some other thread already panicked while holding it)
    /// simply drops the line rather than panicking here too, matching `glass-android`'s
    /// `LogSink`.
    pub fn push(&self, stream: Stream, line: String) {
        if let Ok(mut g) = self.0.lock() {
            g.push((stream, line));
        }
    }

    /// Take and clear all buffered lines.
    pub fn drain(&self) -> Vec<(Stream, String)> {
        self.0
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
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
            });
        }
        Self { child, buf }
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
}
