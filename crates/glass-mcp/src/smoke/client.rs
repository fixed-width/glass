//! The real transport: spawn this same binary as a stdio MCP server and speak
//! JSON-RPC to it, exactly as an MCP client would.
//!
//! `std` gives a pipe-backed `ChildStdout` no way to bound a single `read_line`
//! call with a timeout, so a dedicated reader thread owns the blocking read
//! loop and forwards each line over an `mpsc` channel; [`wait_for_response`]
//! bounds the wait on that channel with `recv_timeout` against a deadline that
//! spans the *whole* call, so a server that accepts a request and then never
//! writes another byte fails with "no response within Ns" instead of hanging
//! the caller forever. `send`'s write to the child's stdin has no equivalent
//! bound.

use crate::smoke::transport::{CallResult, McpTransport};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A server that has not answered in this long is treated as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How many of the server's last stderr lines to keep for a failure detail: enough for a
/// panic message or a degrade warning, bounded so a chatty server cannot grow it without limit.
const STDERR_TAIL_LINES: usize = 20;

/// The JSON-RPC half of the client: ids, framing, the initialize handshake, and tool calls.
/// Generic over its sink so a test can drive it with an in-memory buffer and a channel, with no
/// child process. Process concerns — killing a hung server, collecting its stderr — belong to
/// [`StdioClient`], which wraps this.
#[derive(Debug)]
pub(super) struct Session<W: Write> {
    sink: W,
    /// Lines the reader thread has pulled off the server's stdout, oldest first.
    rx: Receiver<String>,
    next_id: i64,
    /// Never seeded from this binary's own `crate::VERSION`: client and server are the same
    /// executable here, so a seeded value would look like something the server answered.
    version: Option<String>,
    /// Per-request deadline, fixed at `CALL_TIMEOUT` in production (`StdioClient::spawn`) and
    /// overridable so tests can exercise the timeout path without a multi-minute wait.
    timeout: Duration,
}

impl<W: Write> Session<W> {
    pub(super) fn new(sink: W, rx: Receiver<String>, timeout: Duration) -> Self {
        Self {
            sink,
            rx,
            next_id: 0,
            version: None,
            timeout,
        }
    }

    /// The version the server reported at `initialize`; `None` when it reported none.
    pub(super) fn server_version(&self) -> Option<String> {
        self.version.clone()
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let line = format!("{msg}\n");
        self.sink
            .write_all(line.as_bytes())
            .map_err(|e| format!("write to server: {e}"))?;
        self.sink
            .flush()
            .map_err(|e| format!("flush to server: {e}"))
    }

    pub(super) fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Send a request and wait for its matching response, bounded by `self.timeout` for the whole
    /// round trip. Killing the child on failure is the caller's job.
    pub(super) fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )?;
        wait_for_response(&self.rx, id, self.timeout).map_err(|e| format!("{method}: {e}"))
    }

    pub(super) fn initialize(&mut self) -> Result<(), String> {
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
        // A missing `serverInfo.version` is recorded, not fatal: no check asserts on it, so
        // aborting here would throw away every check's evidence over a missing label.
        self.version = init["result"]["serverInfo"]["version"]
            .as_str()
            .map(str::to_string);
        self.notify("notifications/initialized", serde_json::json!({}))
    }

    pub(super) fn call_tool(&mut self, tool: &str, args: Value) -> Result<CallResult, String> {
        let v = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": args }),
        )?;
        if let Some(err) = v.get("error") {
            return Err(format!("{tool}: JSON-RPC error {err}"));
        }
        Ok(CallResult::from_mcp(&v["result"]))
    }

    /// Test-only: recover the sink to assert on what was written.
    #[cfg(test)]
    fn into_sink(self) -> W {
        self.sink
    }
}

#[derive(Debug)]
pub struct StdioClient {
    child: Child,
    session: Session<ChildStdin>,
    /// Joined on drop so no thread outlives the client; `None` once joined.
    reader: Option<JoinHandle<()>>,
    /// The server's last stderr lines. Without these a degrade warning or a panic leaves
    /// only "server closed stdout" or a silent timeout, with nothing saying why.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Joined before the tail is read, so the tail includes the last thing the server said.
    stderr_reader: Option<JoinHandle<()>>,
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
            .stderr(Stdio::piped());
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
        let stderr = child
            .stderr
            .take()
            .ok_or("no stderr on the spawned server")?;
        let (tx, rx) = mpsc::channel();
        let reader = Some(spawn_stdout_reader(stdout, tx)?);
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_reader = Some(spawn_stderr_reader(stderr, stderr_tail.clone())?);
        let mut c = Self {
            child,
            session: Session::new(stdin, rx, timeout),
            reader,
            stderr_tail,
            stderr_reader,
        };
        if let Err(e) = c.session.initialize() {
            return Err(c.on_failure(e));
        }
        Ok(c)
    }

    /// The version the server reported at `initialize`; `None` when it reported none.
    pub fn server_version(&self) -> Option<String> {
        self.session.server_version()
    }

    /// The spawned server's pid, so a test can assert it was reaped.
    #[cfg(all(test, unix))]
    fn child_id(&self) -> u32 {
        self.child.id()
    }

    /// Kill the child, then append its stderr to `e`. Every session failure lands here — a
    /// timeout, a closed pipe, or a JSON-RPC error response — so a single rejected call tears the
    /// server down and the checks that follow will find it gone.
    fn on_failure(&mut self, e: String) -> String {
        self.kill_and_reap();
        let note = self.stderr_note();
        format!("{e}{note}")
    }

    /// The tail of the server's stderr, as a suffix for a failure detail — empty when it
    /// said nothing there. Call only after [`Self::kill_and_reap`]: with the child reaped its
    /// stderr write end is closed, so joining the reader here is prompt and the tail includes
    /// the last thing the server managed to say — typically the panic that killed it.
    fn stderr_note(&mut self) -> String {
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        let Ok(tail) = self.stderr_tail.lock() else {
            return String::new();
        };
        if tail.is_empty() {
            return String::new();
        }
        format!(
            " — server stderr: {}",
            tail.iter().cloned().collect::<Vec<_>>().join(" ; ")
        )
    }

    /// Kill and reap the child. Safe to call more than once: `std::process::Child` caches
    /// the exit status after a successful `wait` on Unix and holds a handle to one specific
    /// kernel object on Windows, so a repeat `kill`/`wait` cannot signal a recycled pid.
    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads newline-delimited JSON from `stdout` on a dedicated thread and
/// forwards each raw line to `tx`, until the pipe closes (EOF), a read fails,
/// or the receiving end is dropped. Owning the blocking read here is what lets
/// the waiting caller bound its wait with `recv_timeout`.
fn spawn_stdout_reader(
    mut stdout: BufReader<ChildStdout>,
    tx: Sender<String>,
) -> Result<JoinHandle<()>, String> {
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
        .map_err(|e| format!("could not spawn the stdout-reader thread: {e}"))
}

/// Keeps the last [`STDERR_TAIL_LINES`] lines the server wrote to stderr. Draining the pipe
/// on its own thread also stops a server that logs heavily from blocking on a full stderr
/// pipe while the client waits for a response that can then never come.
fn spawn_stderr_reader(
    stderr: ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("smoke-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let Ok(mut tail) = tail.lock() else {
                    return; // a poisoned lock means the client is already unwinding
                };
                push_bounded(&mut tail, line);
            }
        })
        .map_err(|e| format!("could not spawn the stderr-reader thread: {e}"))
}

/// Keep the newest [`STDERR_TAIL_LINES`] lines: push, then drop from the front when that puts
/// the tail over the bound. Outside the reader thread's closure so a test can reach it — a wrong
/// comparison here silently shrinks the tail that carries a server panic into a failure detail.
fn push_bounded(tail: &mut VecDeque<String>, line: String) {
    tail.push_back(line);
    if tail.len() > STDERR_TAIL_LINES {
        tail.pop_front();
    }
}

/// Waits on `rx` for a JSON-RPC message whose `id` matches, skipping
/// notifications and responses to other in-flight calls. `budget` bounds the
/// *whole* wait, not each individual receive: `remaining` shrinks against a
/// fixed deadline rather than resetting on every loop iteration, so a server
/// that keeps producing unrelated output without ever answering this request
/// still times out.
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
        match self.session.call_tool(tool, args) {
            Ok(r) => Ok(r),
            Err(e) => Err(self.on_failure(e)),
        }
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // Idempotent even if `on_failure` already did this — see `kill_and_reap`.
        self.kill_and_reap();
        // The reader thread's blocked `read_line` returns only once the child's stdout write
        // end closes, which the kill+wait above guarantees — so this join must stay after it.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // Same reasoning for the stderr reader; `stderr_note` may already have taken it.
        if let Some(reader) = self.stderr_reader.take() {
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
        // A server that keeps emitting unrelated output but never answers this request. If
        // the deadline reset on every arriving line, this would hang instead of timing out.
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

    /// A server that talks only on stderr and never answers on stdout — the shape of one
    /// that panics or degrades at startup. `sh` with no arguments reads commands from stdin,
    /// so it rejects each JSON-RPC line on stderr and never writes a response. Without the
    /// stderr capture the failure would read "no response within Ns" and nothing more.
    #[cfg(unix)]
    #[test]
    fn a_failure_carries_the_servers_stderr_into_the_message() {
        let err = StdioClient::spawn_with_timeout(
            std::path::Path::new("sh"),
            &[],
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(
            err.contains("server stderr"),
            "the failure must carry what the server said on stderr: {err}"
        );
    }

    /// Spawns a real, indefinitely-running process (`yes`, a standard coreutil present on
    /// every Unix this crate builds for) and drives it through `spawn_with_timeout` end to
    /// end. Verifies the actual guarantee — that a timed-out request kills the child rather
    /// than leaving it running — which `wait_for_response`'s unit tests cannot reach: they
    /// never touch a real `Child`.
    #[cfg(unix)]
    #[test]
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
        // If `kill_and_reap` had not terminated `yes` (which never exits on its own), the
        // `Child::wait` inside it would block for as long as `yes` keeps running. Finishing
        // quickly is the evidence the child was really killed, not just that the deadline fired.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "kill+reap of the child must not hang: {:?}",
            start.elapsed()
        );
    }

    /// Ids must advance: a counter that goes backwards or stalls makes `wait_for_response`
    /// match a stale reply, or never match at all.
    #[test]
    fn request_ids_advance_by_one() {
        let (tx, rx) = mpsc::channel();
        tx.send("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n".into())
            .unwrap();
        tx.send("{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n".into())
            .unwrap();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        s.request("first", serde_json::json!({})).expect("first");
        s.request("second", serde_json::json!({})).expect("second");

        let raw = s.into_sink();
        let sent = String::from_utf8_lossy(&raw);
        let ids: Vec<i64> = sent
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v.get("id").and_then(Value::as_i64))
            .collect();
        assert_eq!(ids, vec![1, 2], "sent: {sent}");
    }

    /// A notification is a message with no id. Returning Ok without writing one means the
    /// server never hears it.
    #[test]
    fn notify_writes_a_message_with_no_id() {
        let (_tx, rx) = mpsc::channel();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        s.notify("notifications/initialized", serde_json::json!({}))
            .expect("notify");

        let raw = s.into_sink();
        let sent = String::from_utf8_lossy(&raw);
        let v: Value = serde_json::from_str(sent.trim()).expect("one json line");
        assert_eq!(v["method"], "notifications/initialized");
        assert!(
            v.get("id").is_none(),
            "a notification carries no id: {sent}"
        );
    }

    /// The version in the report must be what the server answered, not a default or a
    /// stand-in — client and server are the same executable, so a seeded value would look right.
    #[test]
    fn initialize_captures_the_servers_reported_version() {
        let (tx, rx) = mpsc::channel();
        tx.send(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"serverInfo\":{\"version\":\"9.9.9-test\"}}}\n"
                .into(),
        )
        .unwrap();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        s.initialize().expect("initialize");
        assert_eq!(s.server_version().as_deref(), Some("9.9.9-test"));
    }

    #[test]
    fn a_server_reporting_no_version_records_none_rather_than_failing() {
        let (tx, rx) = mpsc::channel();
        tx.send("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n".into())
            .unwrap();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        s.initialize()
            .expect("a missing version is recorded, not fatal");
        assert_eq!(s.server_version(), None);
    }

    /// `call_tool` must carry the tool's envelope through, not a default-constructed result —
    /// every check reads what this returns.
    #[test]
    fn call_tool_returns_the_servers_envelope() {
        let (tx, rx) = mpsc::channel();
        tx.send(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"ok\\\":true,\\\"tool\\\":\\\"glass_stop\\\",\\\"result\\\":{}}\"}],\"isError\":false}}\n"
                .into(),
        )
        .unwrap();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        let r = s
            .call_tool("glass_stop", serde_json::json!({}))
            .expect("call");
        assert_eq!(
            r.envelope.as_ref().expect("envelope")["tool"],
            serde_json::json!("glass_stop")
        );
    }

    /// A JSON-RPC error is not a tool result; reading it as one would let a failed call be
    /// graded as a pass.
    #[test]
    fn call_tool_rejects_a_json_rpc_error() {
        let (tx, rx) = mpsc::channel();
        tx.send(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"no such tool\"}}\n"
                .into(),
        )
        .unwrap();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        let e = s
            .call_tool("glass_nope", serde_json::json!({}))
            .unwrap_err();
        assert!(e.contains("glass_nope"), "must name the tool: {e}");
        assert!(
            e.contains("no such tool"),
            "must carry the server's message: {e}"
        );
    }

    /// The tail keeps the LAST lines: a server that logs heavily before panicking must not
    /// push its panic out of the buffer, and the bound must not collapse the tail.
    #[test]
    fn the_stderr_tail_keeps_the_last_lines_up_to_the_bound() {
        let mut tail = VecDeque::new();
        for i in 0..(STDERR_TAIL_LINES + 5) {
            push_bounded(&mut tail, format!("line {i}"));
        }
        let newest: Vec<String> = (5..STDERR_TAIL_LINES + 5)
            .map(|i| format!("line {i}"))
            .collect();
        assert_eq!(Vec::from(tail), newest);
    }

    /// What a stub server reports at `initialize` — unlike this crate's own version, which the
    /// client already holds and so could not be told apart from one the server answered.
    #[cfg(unix)]
    const STUB_VERSION: &str = "7.7.7-stub";

    /// A stub MCP server that answers `initialize`, answers one `tools/call` by running
    /// `on_call`, and exits when its stdin closes.
    #[cfg(unix)]
    fn stub_answering(on_call: &str) -> String {
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
  *'"method":"initialize"'*)
    printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"serverInfo":{{"version":"{STUB_VERSION}"}}}}}}'
    ;;
  *'"method":"tools/call"'*)
{on_call}
    ;;
  esac
done
"#
        )
    }

    /// A stub MCP server that answers `initialize` and then stops reading its stdin: dropping
    /// the client closes that pipe, so a stub still in `read` would exit on its own and a `Drop`
    /// that reaps nothing would look correct. The `sleep` bounds how long a leaked child runs.
    #[cfg(unix)]
    fn stub_outliving_its_stdin() -> String {
        format!(
            r#"#!/bin/sh
IFS= read -r line
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"serverInfo":{{"version":"{STUB_VERSION}"}}}}}}'
exec sleep 10
"#
        )
    }

    /// Writes `script` as an executable in a fresh temporary directory and returns both. The
    /// caller must hold the directory: dropping it deletes the script, including while a
    /// failing assertion unwinds.
    #[cfg(unix)]
    fn write_stub(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a temporary directory for the stub server");
        let exe = dir.path().join("stub-server.sh");
        std::fs::write(&exe, script).expect("write the stub server");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
            .expect("make the stub server executable");
        (dir, exe)
    }

    /// A stub answers in milliseconds; bounding the session well under `spawn`'s production
    /// budget keeps a handshake that never completes a fast failure rather than a two-minute stall.
    #[cfg(unix)]
    const STUB_TIMEOUT: Duration = Duration::from_secs(5);

    /// Spawn a stub server, retrying past a transient ETXTBSY: a sibling test thread's fork
    /// can momentarily hold the freshly written script's fd open, racing our exec (same
    /// rationale as the glass-x11 and glass-ios fixture tests).
    #[cfg(unix)]
    fn spawn_stub(exe: &std::path::Path) -> StdioClient {
        let mut last = None;
        for _ in 0..100 {
            match StdioClient::spawn_with_timeout(exe, &[], STUB_TIMEOUT) {
                Err(m) if m.contains("Text file busy") => {
                    thread::sleep(Duration::from_millis(10));
                    last = Some(m);
                }
                r => return r.expect("spawn the stub server"),
            }
        }
        panic!("ETXTBSY persisted after 100 retries: {last:?}")
    }

    /// A reaped child is gone from the process table; a leaked one — running or a zombie —
    /// still answers signal 0.
    #[cfg(unix)]
    fn pid_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("run kill -0")
            .success()
    }

    /// The version in the report must be the one that came back over the wire: client and
    /// server are the same executable in a real run, so only a spawned server reporting a
    /// version nothing else holds can tell a relayed value from an invented one.
    #[cfg(unix)]
    #[test]
    fn a_spawned_server_reports_the_version_it_sent() {
        let (_dir, exe) = write_stub(&stub_answering("    :"));
        let client = spawn_stub(&exe);
        assert_eq!(client.server_version().as_deref(), Some(STUB_VERSION));
    }

    /// Every check grades what `call` returns, so it must carry the server's envelope out of
    /// the process rather than anything the client could have built without asking.
    #[cfg(unix)]
    #[test]
    fn a_spawned_server_call_returns_the_servers_envelope() {
        let reply = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"ok\":true,\"tool\":\"glass_stop\",\"result\":{\"stopped\":true}}"}],"isError":false}}"#;
        let (_dir, exe) = write_stub(&stub_answering(&format!("    printf '%s\\n' '{reply}'")));
        let mut client = spawn_stub(&exe);
        let r = client
            .call("glass_stop", serde_json::json!({}))
            .expect("the stub answers the call");
        assert_eq!(
            r.envelope,
            Some(serde_json::json!({
                "ok": true, "tool": "glass_stop", "result": { "stopped": true }
            }))
        );
    }

    /// A JSON-RPC error response reaches `on_failure` like any other session failure, so it
    /// tears the server down too — a contract no glass tool reaches today, and so one that
    /// nothing but this test holds in place.
    #[cfg(unix)]
    #[test]
    fn a_json_rpc_error_response_kills_the_server() {
        let reply = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no such tool"}}"#;
        let on_call = format!(
            "    printf '%s\\n' 'stub refused the call' >&2\n    printf '%s\\n' '{reply}'\n    exit 0"
        );
        let (_dir, exe) = write_stub(&stub_answering(&on_call));
        let mut client = spawn_stub(&exe);
        let pid = client.child_id();

        let e = client
            .call("glass_nope", serde_json::json!({}))
            .unwrap_err();
        assert!(e.contains("glass_nope"), "must name the tool: {e}");
        assert!(
            e.contains("stub refused the call"),
            "must carry the server's stderr: {e}"
        );
        assert!(!pid_is_alive(pid), "pid {pid} survived a rejected call");
    }

    /// Dropping the client must reap the server: a leaked one outlives the run and holds
    /// whatever the run was driving.
    #[cfg(unix)]
    #[test]
    fn dropping_the_client_reaps_the_spawned_server() {
        let (_dir, exe) = write_stub(&stub_outliving_its_stdin());
        let client = spawn_stub(&exe);
        let pid = client.child_id();
        // Signal 0 must reach the stub while it still runs, or a pid that is not ours — one we
        // may not signal, or that never existed — would read as reaped below.
        assert!(
            pid_is_alive(pid),
            "the stub must run before the client is dropped"
        );
        drop(client);
        assert!(!pid_is_alive(pid), "pid {pid} outlived the client");
    }
}
