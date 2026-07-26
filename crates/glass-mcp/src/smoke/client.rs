//! The real transport: spawn this same binary as a stdio MCP server and speak
//! JSON-RPC to it, exactly as an MCP client would.
//!
//! `std` gives a pipe-backed `ChildStdout` no way to bound a single `read_line`
//! call with a timeout, so a dedicated reader thread owns the blocking read
//! loop and forwards each line over an `mpsc` channel; [`wait_for_response`]
//! bounds the wait on that channel with `recv_timeout` against a deadline that
//! spans the *whole* call. That's what makes a genuinely silent server — one
//! that accepts a request and then never writes another byte — fail with "no
//! response within Ns" instead of hanging the caller forever, not just a
//! server that's merely slow to produce the matching id. `send`'s write to the
//! child's stdin has no equivalent bound: a single JSON-RPC message here is
//! small enough to fit in the OS pipe buffer without blocking in practice, but
//! that write is not itself deadline-bounded.

use crate::smoke::transport::{CallResult, McpTransport};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A server that has not answered in this long is treated as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    /// Lines the reader thread has pulled off the child's stdout, oldest first.
    rx: Receiver<String>,
    /// Joined on drop so no thread outlives the client; `None` once joined.
    reader: Option<JoinHandle<()>>,
    next_id: i64,
    version: String,
    /// Per-request deadline. Fixed at `CALL_TIMEOUT` in production
    /// (`spawn`); overridable so tests can exercise the timeout path without
    /// a multi-minute wait.
    timeout: Duration,
}

impl StdioClient {
    /// Spawn `exe` as a stdio MCP server and complete the initialize handshake.
    pub fn spawn(exe: &std::path::Path, env: &[(&str, &str)]) -> Result<Self, String> {
        Self::spawn_with_timeout(exe, env, CALL_TIMEOUT)
    }

    fn spawn_with_timeout(
        exe: &std::path::Path,
        env: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not spawn {}: {e}", exe.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin on the spawned server")?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("no stdout on the spawned server")?,
        );
        let (tx, rx) = mpsc::channel();
        let reader = Some(spawn_stdout_reader(stdout, tx));
        let mut c = Self {
            child,
            stdin,
            rx,
            reader,
            next_id: 0,
            version: crate::VERSION.to_string(),
            timeout,
        };
        c.initialize()?;
        Ok(c)
    }

    fn initialize(&mut self) -> Result<(), String> {
        let init = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "glass-smoke", "version": crate::VERSION }
            }),
        )?;
        if init.get("result").is_none() {
            return Err(format!("initialize failed: {init}"));
        }
        if let Some(v) = init["result"]["serverInfo"]["version"].as_str() {
            self.version = v.to_string();
        }
        self.notify("notifications/initialized", serde_json::json!({}))
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let line = format!("{msg}\n");
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write to server: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flush to server: {e}"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Send a request and wait for its matching response, bounded by
    /// `self.timeout` for the whole round trip. A timeout or a closed pipe
    /// both kill the child before returning — a run that gives up on the
    /// server must not leave it running unattended.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )?;
        match wait_for_response(&self.rx, id, self.timeout) {
            Ok(v) => Ok(v),
            Err(e) => {
                self.kill_and_reap();
                Err(format!("{method}: {e}"))
            }
        }
    }

    /// Kill and reap the child. Safe to call more than once — `std::process::
    /// Child`'s `kill`/`wait` cache the exit status internally and turn a
    /// repeat call into a no-op rather than re-signaling a pid that could by
    /// then have been recycled for an unrelated process.
    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads newline-delimited JSON from `stdout` on a dedicated thread and
/// forwards each raw line to `tx`, until the pipe closes (EOF), a read fails,
/// or the receiving end is dropped. Owning the blocking read here — instead of
/// on the thread that's waiting for a specific response — is what lets that
/// caller bound its wait with `recv_timeout` rather than being at the mercy of
/// a `read_line` call `std` gives no way to time out directly on a pipe.
fn spawn_stdout_reader(mut stdout: BufReader<ChildStdout>, tx: Sender<String>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("smoke-stdout".into())
        .spawn(move || {
            loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return; // the client was dropped; nothing left to feed
                        }
                    }
                }
            }
        })
        .expect("spawn smoke stdout-reader thread")
}

/// Waits on `rx` for a JSON-RPC message whose `id` matches, skipping
/// notifications and responses to other in-flight calls. `budget` bounds the
/// *whole* wait, not each individual receive: `remaining` shrinks against a
/// fixed deadline rather than resetting to `budget` on every loop iteration,
/// so a server that keeps producing unrelated output without ever answering
/// this request still times out instead of having its deadline pushed back by
/// every line that arrives.
fn wait_for_response(rx: &Receiver<String>, id: i64, budget: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("no response within {}s", budget.as_secs()));
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                    && v.get("id").and_then(Value::as_i64) == Some(id)
                {
                    return Ok(v);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!("no response within {}s", budget.as_secs()));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("server closed stdout".to_string());
            }
        }
    }
}

impl McpTransport for StdioClient {
    fn call(&mut self, tool: &str, args: Value) -> Result<CallResult, String> {
        let v = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": args }),
        )?;
        if let Some(err) = v.get("error") {
            return Err(format!("{tool}: JSON-RPC error {err}"));
        }
        Ok(CallResult::from_mcp(&v["result"]))
    }

    fn server_version(&mut self) -> Result<String, String> {
        Ok(self.version.clone())
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // Idempotent even if `request` already did this on a timeout — see
        // `kill_and_reap`.
        self.kill_and_reap();
        // The reader thread's blocked `read_line` only returns once the
        // child's stdout write end closes, which the kill+wait above
        // guarantees (the kernel closes it as part of process teardown), so
        // joining here should be near-instant rather than a real wait.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawning_a_nonexistent_binary_reports_the_path() {
        let err =
            StdioClient::spawn(std::path::Path::new("/nonexistent/glass-mcp"), &[]).unwrap_err();
        assert!(
            err.contains("/nonexistent/glass-mcp"),
            "must name the path: {err}"
        );
    }

    #[test]
    fn wait_for_response_times_out_when_the_server_stays_silent() {
        // `_tx` is kept alive (not dropped), so this is "alive but silent",
        // not "pipe closed".
        let (_tx, rx) = mpsc::channel::<String>();
        let err = wait_for_response(&rx, 1, Duration::from_millis(30)).unwrap_err();
        assert!(
            err.contains("no response within"),
            "expected a timeout message, got {err}"
        );
    }

    #[test]
    fn wait_for_response_reports_a_closed_pipe_immediately() {
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx); // what the reader thread does once the pipe closes
        let start = Instant::now();
        let err = wait_for_response(&rx, 1, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("closed stdout"), "got {err}");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a closed channel must be reported immediately, not after the full budget: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wait_for_response_skips_unmatched_ids_and_returns_the_matching_one() {
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#.to_string())
            .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":5,"result":{"other":true}}"#.to_string())
            .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#.to_string())
            .unwrap();
        let v = wait_for_response(&rx, 7, Duration::from_secs(5)).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn wait_for_response_bounds_a_chatty_but_never_matching_server() {
        let (tx, rx) = mpsc::channel::<String>();
        // Simulates a server that keeps emitting unrelated output (heartbeats,
        // notifications) but never answers this request. If the deadline were
        // reset on every arriving line instead of spanning the whole call,
        // this would hang forever instead of timing out.
        thread::spawn(move || {
            loop {
                let noise = r#"{"jsonrpc":"2.0","method":"notifications/noise"}"#.to_string();
                if tx.send(noise).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let start = Instant::now();
        let err = wait_for_response(&rx, 1, Duration::from_millis(80)).unwrap_err();
        assert!(err.contains("no response within"), "got {err}");
        assert!(
            start.elapsed() < Duration::from_millis(600),
            "the deadline must span the whole call, not reset per line: took {:?}",
            start.elapsed()
        );
    }

    /// Not run by default: spawns a real, indefinitely-running process with no
    /// arguments (`yes`) and drives it through `StdioClient::spawn_with_timeout`
    /// end to end, so it depends on a Unix coreutil that isn't guaranteed on
    /// every platform this crate builds for. Verifies the actual guarantee —
    /// that a timed-out request kills the child rather than leaving it
    /// running — which `wait_for_response`'s unit tests above can't reach
    /// (they never touch a real `Child`).
    #[cfg(unix)]
    #[test]
    #[ignore = "manual: spawns a real never-responding process; \
                run via: cargo test -p glass-mcp smoke::client -- --ignored"]
    fn a_timed_out_request_kills_the_child() {
        let start = Instant::now();
        let err = StdioClient::spawn_with_timeout(
            std::path::Path::new("yes"),
            &[],
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(
            err.contains("no response within"),
            "expected a timeout, got {err}"
        );
        // If `kill_and_reap` failed to actually terminate `yes` (which never
        // exits on its own), `Child::wait` inside it would block for as long
        // as `yes` keeps running — effectively forever. Finishing quickly is
        // itself the evidence the child was really killed, not just that the
        // deadline fired.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "kill+reap of the child must not hang: {:?}",
            start.elapsed()
        );
    }
}
