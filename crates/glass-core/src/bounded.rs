//! Run a one-shot child process under a deadline.
//!
//! Every backend that drives an external tool (`adb`, `xcrun simctl`, `plutil`) needs the same
//! thing: the tool answers quickly, or it has hung and must not hang the agent's call with it.
//! `std::process` offers no wait-with-a-deadline, so this module supplies the one shape they all
//! use. A long-lived child — a log tail, an emulator, the on-device agent — is NOT this: those use
//! `Command::spawn` directly and are meant to outlive the call that started them.

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{GlassError, Result};

/// How often the parent checks whether the child has exited while waiting out its budget.
const POLL: Duration = Duration::from_millis(20);

/// How long to let a drain thread finish reading after the child has exited, before taking what it
/// has. Normally instant — the pipe closes with the child — but a grandchild that inherited the
/// write end keeps it open, and no output is worth stalling a completed call for.
const DRAIN_SETTLE: Duration = Duration::from_millis(200);

/// Bytes read from one of the child's pipes, shared with the thread still reading it.
type Drained = Arc<Mutex<Vec<u8>>>;

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
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GlassError::Backend(format!("{op}: failed to start: {e}")))?;

    let (stdout, out_thread) = drain(child.stdout.take());
    let (stderr, err_thread) = drain(child.stderr.take());

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(GlassError::Backend(format!("{op}: wait failed: {e}"))),
        }
        if Instant::now() >= deadline {
            return Err(timed_out(&mut child, op, budget, &stdout, &stderr));
        }
        std::thread::sleep(POLL);
    };

    settle(out_thread);
    settle(err_thread);
    Ok(Output {
        status,
        stdout: taken(&stdout),
        stderr: taken(&stderr),
    })
}

/// Read a pipe on its own thread, appending each chunk to a buffer the caller can read at any time.
///
/// Chunked rather than `read_to_end` so a timeout can report what the child had already said: a
/// killed child does not necessarily close the pipe — `sh -c "...; sleep 30"` leaves `sleep`
/// holding the write end, and `adb` likewise spawns subprocesses — so a thread blocked in
/// `read_to_end` would still be waiting for EOF with the bytes stuck in its local buffer.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> (Drained, Option<JoinHandle<()>>) {
    let buf: Drained = Arc::new(Mutex::new(Vec::new()));
    let Some(mut pipe) = pipe else {
        return (buf, None);
    };
    let sink = Arc::clone(&buf);
    let handle = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if let Ok(mut sink) = sink.lock() {
                sink.extend_from_slice(&chunk[..n]);
            }
        }
    });
    (buf, Some(handle))
}

/// Wait briefly for a drain thread to reach EOF, so a completed call reports its whole output.
/// Bounded by [`DRAIN_SETTLE`]: a grandchild holding the pipe open must not strand the caller.
fn settle(handle: Option<JoinHandle<()>>) {
    let Some(handle) = handle else { return };
    let deadline = Instant::now() + DRAIN_SETTLE;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}

/// Whatever a drain thread has collected so far. A poisoned lock reads as empty — one unreadable
/// buffer must not turn a real timeout into a panic.
fn taken(buf: &Drained) -> Vec<u8> {
    buf.lock().map(|b| b.clone()).unwrap_or_default()
}

/// Kill the child and describe the timeout, including whatever it managed to say first — a partial
/// `uiautomator dump` tells a reader more about the hang than silence does.
fn timed_out(
    child: &mut Child,
    op: &str,
    budget: Duration,
    stdout: &Drained,
    stderr: &Drained,
) -> GlassError {
    let _ = child.kill();
    let _ = child.wait();
    let out = String::from_utf8_lossy(&taken(stdout)).trim().to_string();
    let err = String::from_utf8_lossy(&taken(stderr)).trim().to_string();
    let said = match (out.is_empty(), err.is_empty()) {
        (true, true) => "it produced no output before the kill".to_string(),
        (false, true) => format!("stdout before the kill: {out}"),
        (true, false) => format!("stderr before the kill: {err}"),
        (false, false) => format!("before the kill — stdout: {out}; stderr: {err}"),
    };
    GlassError::Backend(format!(
        "{op}: no answer within {budget:?}, so the process was killed; {said}"
    ))
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
    fn output_larger_than_a_pipe_buffer_still_completes() {
        // 1 MiB, well past the ~64 KiB a pipe holds: a parent that waits without draining
        // deadlocks here, and the deadline never fires because the child never exits.
        let out = run_bounded(
            Command::new("/bin/sh").args(["-c", "yes ABCDEFGH | head -c 1048576"]),
            Duration::from_secs(20),
            "test:flood",
        )
        .expect("a draining implementation completes");
        assert_eq!(out.stdout.len(), 1_048_576);
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
