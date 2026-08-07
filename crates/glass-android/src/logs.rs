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

/// Classify and file each line of `stdout` into `sink` until the pipe ends.
fn drain_into(stdout: std::process::ChildStdout, sink: LogSink) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(|r| r.ok()) {
            let stream = classify_logcat_line(&line);
            if let Ok(mut g) = sink.lock() {
                g.push((stream, line));
            }
        }
    })
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
        Ok(Self {
            child,
            _reader: drain_into(stdout, sink),
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

    /// A `LogcatStream` over a stand-in child that prints `lines` and then stays up until killed,
    /// plus its pid and the sink it drains into. `lines` must carry no single quote.
    #[cfg(unix)]
    fn stream_over_a_child_printing(lines: &[&str]) -> (LogcatStream, u32, LogSink) {
        let sink: LogSink = Arc::new(Mutex::new(Vec::new()));
        let said: Vec<String> = lines.iter().map(|l| format!("'{l}'")).collect();
        // `exec` so the shell becomes the sleep: the kill under test reaches the process whose
        // pid this returns, with no grandchild left orphaned behind it.
        let script = format!("printf '%s\\n' {}; exec sleep 30", said.join(" "));
        let mut child = Command::new("/bin/sh")
            .args(["-c", &script])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the stand-in");
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped stdout");
        let stream = LogcatStream {
            child,
            _reader: drain_into(stdout, Arc::clone(&sink)),
        };
        (stream, pid, sink)
    }

    #[cfg(unix)]
    fn stream_over_a_sleeping_child() -> (LogcatStream, u32) {
        let (stream, pid, _) = stream_over_a_child_printing(&[]);
        (stream, pid)
    }

    #[test]
    #[cfg(unix)]
    fn each_line_reaches_the_sink_classified_by_its_priority() {
        // What `glass_logs` hands the caller. An error line arriving as stdout sends whoever
        // reads it straight past the thing that broke — and until now the drain was built by
        // every test here and run by none of them.
        let (_stream, _pid, sink) = stream_over_a_child_printing(&[
            "06-15 12:00:00.0  1 1 I Tag: starting up",
            "06-15 12:00:00.0  1 1 E Tag: boom",
        ]);

        // The drain reads on its own thread, so wait for it rather than racing it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sink.lock().expect("sink").len() < 2 && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(10));
        }

        let lines = sink.lock().expect("sink").clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].0, Stream::Stdout);
        assert!(lines[0].1.contains("starting up"), "{lines:?}");
        assert_eq!(
            lines[1].0,
            Stream::Stderr,
            "an E line is the app's error output: {lines:?}"
        );
        assert!(lines[1].1.contains("boom"), "{lines:?}");
    }

    /// Whether `pid` is still a live process — `stop` reaps what it kills, so a stopped child's
    /// pid is gone rather than left as a zombie that would still answer.
    #[cfg(unix)]
    fn still_running(pid: u32) -> bool {
        crate::adb::still_running(&pid.to_string())
    }

    #[test]
    #[cfg(unix)]
    fn stopping_the_stream_kills_the_logcat_process() {
        let (mut stream, pid) = stream_over_a_sleeping_child();
        assert!(
            still_running(pid),
            "the stand-in should be up to begin with"
        );
        stream.stop();
        assert!(
            !still_running(pid),
            "logcat outlived stop(), holding the device's log open behind it"
        );
    }

    #[test]
    #[cfg(unix)]
    fn dropping_the_stream_kills_the_logcat_process() {
        // Every teardown path reaches this through Drop rather than by calling stop, so a Drop
        // that does nothing leaks one `adb logcat` per app the session ever started.
        let (stream, pid) = stream_over_a_sleeping_child();
        drop(stream);
        assert!(
            !still_running(pid),
            "logcat outlived the stream that owned it"
        );
    }

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
