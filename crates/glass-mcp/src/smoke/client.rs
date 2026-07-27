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
//!
//! Teardown is bounded the same way — it runs *because* something already went
//! wrong, so a stall there costs the whole report rather than one check: the
//! reader joins are bounded by [`READER_JOIN_TIMEOUT`], and a failed kill or
//! reap is reported rather than discarded.

use crate::smoke::transport::{CallResult, McpTransport};
use serde_json::Value;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A server that has not answered in this long is treated as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long teardown waits for a reader thread to exit before abandoning it: only something
/// else still holding the child's write end — a grandchild, say — reaches this bound, and an
/// unbounded join there hangs forever.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

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

    /// Drop the receiving end of the reader thread's channel so its next `send` fails and it
    /// returns — teardown only, and what stops a child that survived the kill and keeps writing
    /// from filling a queue nobody will read.
    fn disconnect(&mut self) {
        let (_closed, rx) = mpsc::channel();
        self.rx = rx;
    }

    /// Test-only: recover the sink to assert on what was written.
    #[cfg(test)]
    fn into_sink(self) -> W {
        self.sink
    }
}

/// A reader thread and a signal that it has exited. `JoinHandle::join` cannot be bounded, so the
/// thread holds a sender it never sends on: the channel disconnects when the thread drops it on
/// the way out, which turns an unbounded join into a `recv_timeout`.
#[derive(Debug)]
struct Reader {
    /// Which pipe this thread drains, for the message a stalled join produces.
    what: &'static str,
    handle: JoinHandle<()>,
    exited: Receiver<Infallible>,
}

impl Reader {
    /// Join the thread, waiting at most `budget`; a thread still blocked in `read` at the
    /// deadline is reported and abandoned, leaving its pipe open but returning control.
    fn join_bounded(self, budget: Duration) -> Result<(), String> {
        match self.exited.recv_timeout(budget) {
            Ok(never) => match never {},
            Err(RecvTimeoutError::Disconnected) => {
                let _ = self.handle.join();
                Ok(())
            }
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "the {} reader was still blocked after {budget:?} and was abandoned",
                self.what
            )),
        }
    }
}

#[derive(Debug)]
pub struct StdioClient {
    child: Child,
    session: Session<ChildStdin>,
    /// Joined on drop so no thread outlives the client; `None` once joined.
    reader: Option<Reader>,
    /// The server's last stderr lines. Without these a degrade warning or a panic leaves
    /// only "server closed stdout" or a silent timeout, with nothing saying why.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Joined before the tail is read, so the tail includes the last thing the server said.
    stderr_reader: Option<Reader>,
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
        let mut msg = e;
        if let Err(t) = self.kill_and_reap() {
            msg.push_str(&format!(" — {t}"));
        }
        msg.push_str(&self.stderr_note());
        msg
    }

    /// The tail of the server's stderr, as a suffix for a failure detail — empty when it said
    /// nothing there and the reader shut down cleanly. Call only after [`Self::kill_and_reap`]:
    /// with the child reaped its stderr write end is closed, so the join is prompt and the tail
    /// includes the last thing the server managed to say — typically the panic that killed it.
    /// A reader still blocked past the bound is named in the suffix, since the tail is then only
    /// what happened to have arrived.
    fn stderr_note(&mut self) -> String {
        let stalled = self
            .stderr_reader
            .take()
            .and_then(|r| r.join_bounded(READER_JOIN_TIMEOUT).err());
        let tail = self.stderr_tail.lock().ok().and_then(|t| {
            (!t.is_empty()).then(|| t.iter().cloned().collect::<Vec<_>>().join(" ; "))
        });
        match (tail, stalled) {
            (Some(t), None) => format!(" — server stderr: {t}"),
            (Some(t), Some(w)) => format!(" — server stderr (partial, {w}): {t}"),
            (None, Some(w)) => format!(" — no server stderr: {w}"),
            (None, None) => String::new(),
        }
    }

    /// Kill and reap the child, reporting a failure rather than discarding it: every join in
    /// teardown waits on a pipe this call closes. Safe to call more than once:
    /// `std::process::Child` caches the exit status after a successful `wait` on Unix and holds
    /// a handle to one specific kernel object on Windows, so a repeat `kill`/`wait` cannot
    /// signal a recycled pid.
    fn kill_and_reap(&mut self) -> Result<(), String> {
        self.child
            .kill()
            .map_err(|e| format!("could not kill the server: {e}"))?;
        self.child
            .wait()
            .map_err(|e| format!("could not reap the server: {e}"))?;
        Ok(())
    }
}

/// Reads newline-delimited JSON from `stdout` on a dedicated thread and
/// forwards each raw line to `tx`, until the pipe closes (EOF), a read fails,
/// or the receiving end is dropped. Owning the blocking read here is what lets
/// the waiting caller bound its wait with `recv_timeout`.
fn spawn_stdout_reader(
    mut stdout: BufReader<ChildStdout>,
    tx: Sender<String>,
) -> Result<Reader, String> {
    spawn_reader("stdout", move || {
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
}

/// Spawn a named reader thread wired to an exit signal, so [`Reader::join_bounded`] can bound a
/// join that would otherwise wait on a `read` nothing guarantees will return.
fn spawn_reader(
    what: &'static str,
    body: impl FnOnce() + Send + 'static,
) -> Result<Reader, String> {
    let (exit_tx, exited) = mpsc::channel::<Infallible>();
    let handle = thread::Builder::new()
        .name(format!("smoke-{what}"))
        .spawn(move || {
            // Moved in, never sent on: dropping it as this thread ends is the exit signal.
            let _exit_tx = exit_tx;
            body();
        })
        .map_err(|e| format!("could not spawn the {what}-reader thread: {e}"))?;
    Ok(Reader {
        what,
        handle,
        exited,
    })
}

/// Keeps the last [`STDERR_TAIL_LINES`] lines the server wrote to stderr. Draining the pipe
/// on its own thread also stops a server that logs heavily from blocking on a full stderr
/// pipe while the client waits for a response that can then never come.
fn spawn_stderr_reader(
    stderr: ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> Result<Reader, String> {
    spawn_reader("stderr", move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let Ok(mut tail) = tail.lock() else {
                return; // a poisoned lock means the client is already unwinding
            };
            push_bounded(&mut tail, line);
        }
    })
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
        // Idempotent even if `on_failure` already did this — see `kill_and_reap`. A `drop` has
        // nowhere to report a failure, so the bounded joins below are what keep one from hanging.
        let _ = self.kill_and_reap();
        // A reader's blocked `read` returns once the child's write end closes, which the
        // kill+reap above normally guarantees — so these joins must stay after it.
        self.session.disconnect();
        for reader in [self.reader.take(), self.stderr_reader.take()]
            .into_iter()
            .flatten()
        {
            let _ = reader.join_bounded(READER_JOIN_TIMEOUT);
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

    /// A handshake that times out against a real process which never exits and floods stdout
    /// (`yes`, a standard coreutil present on every Unix this crate builds for), driven through
    /// `spawn_with_timeout` end to end — a shape `wait_for_response`'s unit tests cannot reach,
    /// since they never touch a `Child`.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_request_kills_the_child() {
        let start = Instant::now();
        let err = StdioClient::spawn_with_timeout(
            std::path::Path::new("yes"),
            &[],
            Duration::from_millis(100),
        )
        .expect_err("`yes` answers no JSON-RPC request");
        assert!(
            err.contains("no response within"),
            "expected a timeout, got {err}"
        );
        // The bound covers the whole failure path — deadline, kill, both reader joins — so it
        // shows that path terminates, not that the child was reaped: a blocked reader costs
        // `READER_JOIN_TIMEOUT` and no more. `a_timed_out_call_reaps_the_server` pins the reaping.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the failure path must stay bounded: {:?}",
            start.elapsed()
        );
    }

    /// Teardown must stop the reader thread feeding a channel nobody will read again: a child
    /// that outlived the kill and keeps writing would otherwise grow the queue without bound.
    #[test]
    fn disconnect_closes_the_channel_the_reader_thread_sends_on() {
        let (tx, rx) = mpsc::channel();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        tx.send("before".to_string())
            .expect("the reader can send while the session holds the receiver");
        s.disconnect();
        assert!(
            tx.send("after".to_string()).is_err(),
            "the reader must find the channel closed, which is what makes it return"
        );
    }

    /// A thread still blocked in `read` must be reported and abandoned rather than waited out,
    /// or the run produces no report at all.
    #[test]
    fn a_blocked_reader_is_reported_rather_than_waited_out() {
        let (release, blocked) = mpsc::channel::<()>();
        let reader = spawn_reader("test", move || {
            let _ = blocked.recv(); // returns only when the test drops `release`
        })
        .expect("spawn the reader thread");
        let start = Instant::now();
        let err = reader
            .join_bounded(Duration::from_millis(50))
            .expect_err("a thread that never exits must not be joined");
        assert!(err.contains("still blocked"), "got {err}");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the join must give up at its bound, not wait for the thread: {:?}",
            start.elapsed()
        );
        drop(release);
    }

    /// The ordinary case: a reader whose pipe closed is joined, so no thread outlives the client.
    #[test]
    fn a_finished_reader_is_joined() {
        let reader = spawn_reader("test", || {}).expect("spawn the reader thread");
        reader
            .join_bounded(Duration::from_secs(5))
            .expect("a thread that has exited must join");
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

    /// The handshake is not finished when the result arrives: an MCP client must follow it with
    /// `notifications/initialized`. Dropping that line changes nothing this suite would otherwise
    /// notice — the other initialize tests read only `server_version` — and cargo-mutants cannot
    /// see it either, since it replaces whole function bodies rather than deleting statements.
    #[test]
    fn initialize_follows_the_result_with_the_initialized_notification() {
        let (tx, rx) = mpsc::channel();
        tx.send("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n".into())
            .unwrap();
        let mut s = Session::new(Vec::new(), rx, Duration::from_secs(5));
        s.initialize().expect("initialize");

        let raw = s.into_sink();
        let sent = String::from_utf8_lossy(&raw);
        let msgs: Vec<Value> = sent
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is one JSON message"))
            .collect();
        assert_eq!(msgs.len(), 2, "the request and the notification: {sent}");
        assert_eq!(msgs[1]["method"], "notifications/initialized");
        assert!(
            msgs[1].get("id").is_none(),
            "a notification carries no id: {sent}"
        );
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

    /// The tail keeps the LAST lines: a server that logs heavily before panicking must not push
    /// its panic out of the buffer. The counts are written out rather than derived from
    /// [`STDERR_TAIL_LINES`] — a bound checked against itself holds at any value, including one
    /// too small to carry anything useful.
    #[test]
    fn the_stderr_tail_keeps_the_last_lines_up_to_the_bound() {
        let mut tail = VecDeque::new();
        for i in 0..25 {
            push_bounded(&mut tail, format!("line {i}"));
        }
        let newest: Vec<String> = (5..25).map(|i| format!("line {i}")).collect();
        assert_eq!(Vec::from(tail), newest);
    }

    /// What the tail is for: a server panic reaching the failure detail whole. A Rust panic is
    /// three lines — the `panicked at` header, the message, and the backtrace note — so a bound
    /// that keeps fewer still reports something, just never the cause.
    #[test]
    fn the_tail_carries_a_whole_panic_past_a_chatty_server() {
        let panic_lines = [
            "thread 'main' panicked at src/main.rs:1:1:",
            "the server fell over",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
        ];
        let mut tail = VecDeque::new();
        for i in 0..50 {
            push_bounded(&mut tail, format!("chatter {i}"));
        }
        for line in panic_lines {
            push_bounded(&mut tail, line.to_string());
        }

        let kept = Vec::from(tail);
        for line in panic_lines {
            assert!(
                kept.contains(&line.to_string()),
                "the tail dropped {line:?}: {kept:?}"
            );
        }
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
        spawn_stub_with_timeout(exe, STUB_TIMEOUT)
    }

    #[cfg(unix)]
    fn spawn_stub_with_timeout(exe: &std::path::Path, timeout: Duration) -> StdioClient {
        let mut last = None;
        for _ in 0..100 {
            match StdioClient::spawn_with_timeout(exe, &[], timeout) {
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

    /// A call that times out must leave no server behind, which only the pid can show: with
    /// teardown bounded, a surviving child costs `READER_JOIN_TIMEOUT` and no elapsed-time check
    /// tells it apart. The stub answers nothing and spawns no grandchild, so it is still running
    /// when the deadline fires and its pid is the only one in play.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_call_reaps_the_server() {
        let (_dir, exe) = write_stub(&stub_answering("    :"));
        let mut client = spawn_stub_with_timeout(&exe, Duration::from_millis(200));
        let pid = client.child_id();
        assert!(pid_is_alive(pid), "the stub must run before the call");

        let e = client
            .call("glass_stop", serde_json::json!({}))
            .expect_err("a stub that never answers must time the call out");
        assert!(e.contains("no response within"), "expected a timeout: {e}");
        assert!(!pid_is_alive(pid), "pid {pid} survived a timed-out call");
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
