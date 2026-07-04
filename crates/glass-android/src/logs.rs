use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use glass_core::{GlassError, Result, Stream};

use crate::adb::Adb;

/// Drained by `Platform::drain_logs`. Same shape the X11/Wayland backends use.
pub type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// Map a logcat (threadtime) line's priority to a stream: `E`/`F` → stderr, else stdout.
pub fn classify_logcat_line(line: &str) -> Stream {
    let prio = line
        .split_whitespace()
        .find(|t| t.len() == 1 && matches!(*t, "V" | "D" | "I" | "W" | "E" | "F"));
    match prio {
        Some("E") | Some("F") => Stream::Stderr,
        _ => Stream::Stdout,
    }
}

/// A running `adb logcat --pid=<pid>` whose lines stream into a `LogSink`.
pub struct LogcatStream {
    child: Child,
    _reader: JoinHandle<()>,
}

impl LogcatStream {
    pub fn spawn(adb: &Adb, pid: u32, sink: LogSink) -> Result<Self> {
        // Build argv via the same serial-prefixing path the Adb client uses.
        let pid_arg = format!("--pid={pid}");
        let argv = crate::adb::build_argv(adb.serial(), &["logcat", "-v", "threadtime", &pid_arg]);
        let mut child = Command::new(adb.bin())
            .args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| GlassError::Backend(format!("failed to start adb logcat: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GlassError::Backend("adb logcat produced no stdout pipe".into()))?;
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|r| r.ok()) {
                let stream = classify_logcat_line(&line);
                if let Ok(mut g) = sink.lock() {
                    g.push((stream, line));
                }
            }
        });
        Ok(Self {
            child,
            _reader: reader,
        })
    }

    /// Kill the logcat process; the reader thread ends on pipe EOF.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LogcatStream {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::Stream;

    #[test]
    fn error_and_fatal_map_to_stderr() {
        assert_eq!(
            classify_logcat_line("06-15 12:00:00.0  1 1 E Tag: boom"),
            Stream::Stderr
        );
        assert_eq!(
            classify_logcat_line("06-15 12:00:00.0  1 1 F Tag: crash"),
            Stream::Stderr
        );
    }

    #[test]
    fn info_and_debug_map_to_stdout() {
        assert_eq!(
            classify_logcat_line("06-15 12:00:00.0  1 1 I Tag: hello"),
            Stream::Stdout
        );
        assert_eq!(
            classify_logcat_line("06-15 12:00:00.0  1 1 D Tag: detail"),
            Stream::Stdout
        );
    }

    #[test]
    fn unparseable_defaults_to_stdout() {
        assert_eq!(
            classify_logcat_line("--------- beginning of main"),
            Stream::Stdout
        );
    }
}
