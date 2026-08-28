//! Host-side client for the on-device `glass-android-agent` (the `glass-android-agent`
//! repo): line-delimited JSON over a TCP socket that `adb forward` maps to the device's
//! `localabstract:glass-agent`. `AgentClient` is the request/response client; `AgentRegistry`
//! owns the device server's lifecycle. Everything degrades to the adb paths on failure.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use glass_core::Deadline;
use glass_core::{GlassError, Result};
use serde_json::{Value, json};

use crate::adb::Adb;
use crate::conn::{CallFailure, Conn};

/// One absolute-display point in a pointer path (the agent's gesture element).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
    pub t_ms: u64,
}

/// Request/response client to the agent. `connect` reconnects on a dropped socket once.
pub struct AgentClient {
    port: u16,
    conn: Mutex<Conn>,
}

impl AgentClient {
    pub fn connect(port: u16) -> Result<AgentClient> {
        Ok(AgentClient {
            port,
            conn: Mutex::new(Conn::open(port)?),
        })
    }

    /// Run a request, transparently reconnecting once if the socket dropped.
    fn call(&self, req: Value) -> Result<Value> {
        self.call_by(req, Deadline::UNBOUNDED)
    }

    fn call_by(&self, req: Value, deadline: Deadline) -> Result<Value> {
        self.call_with_by(req, deadline, CallFailure::is_transport)
    }

    /// Run a side-effecting request, retrying only when the first attempt provably sent nothing.
    fn call_once_sent_by(&self, req: Value, deadline: Deadline) -> Result<Value> {
        self.call_with_by(req, deadline, CallFailure::nothing_sent)
    }

    fn call_with_by(
        &self,
        req: Value,
        deadline: Deadline,
        resend: fn(&CallFailure) -> bool,
    ) -> Result<Value> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("agent request"));
        }
        let mut conn = self.lock_by(deadline)?;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("agent request"));
        }
        if conn.ensure_usable().is_err() {
            *conn = Conn::open_by(self.port, deadline)?;
        }
        let first = conn
            .call_within(req.clone(), deadline, "agent request")
            .map_err(|e| CallFailure::NotSent(e).into_error())?;
        match first {
            Ok(v) => Ok(v),
            Err(f) if resend(&f) => {
                // The agent's accept loop accepts a fresh connection after a drop.
                if deadline.has_passed() {
                    return Err(f
                        .with_error(GlassError::caller_deadline_elapsed("agent request"))
                        .into_error());
                }
                *conn = Conn::open_by(self.port, deadline)?;
                if deadline.has_passed() {
                    return Err(GlassError::deadline_not_started("agent retry"));
                }
                let retried = conn
                    .call_within(req, deadline, "agent retry")
                    .map_err(|e| CallFailure::NotSent(e).into_error())?;
                retried.map_err(CallFailure::into_error)
            }
            Err(f) => Err(f.into_error()),
        }
    }

    fn lock_by(&self, deadline: Deadline) -> Result<MutexGuard<'_, Conn>> {
        if deadline.remaining().is_none() {
            return self
                .conn
                .lock()
                .map_err(|_| GlassError::Backend("agent client lock poisoned".into()));
        }
        loop {
            match self.conn.try_lock() {
                Ok(conn) => return Ok(conn),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(GlassError::Backend("agent client lock poisoned".into()));
                }
                Err(TryLockError::WouldBlock) => {
                    let Some(left) = deadline.remaining() else {
                        unreachable!("bounded lock wait became unbounded")
                    };
                    if left.is_zero() {
                        return Err(GlassError::deadline_not_started("agent request"));
                    }
                    std::thread::sleep(left.min(Duration::from_millis(1)));
                }
            }
        }
    }

    pub fn ping(&self) -> Result<()> {
        self.call(json!({"op": "ping"})).map(|_| ())
    }
    pub fn clipboard_get(&self) -> Result<String> {
        let v = self.call(json!({"op": "clipboard_get"}))?;
        v.get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                GlassError::Backend("agent clipboard_get: response missing `text`".into())
            })
    }
    pub fn clipboard_set(&self, text: &str) -> Result<()> {
        self.call(json!({"op": "clipboard_set", "text": text}))
            .map(|_| ())
    }
    pub fn pointer(&self, gesture: &[Pt], button: &str) -> Result<()> {
        self.pointer_by(gesture, button, Deadline::UNBOUNDED)
    }
    pub fn pointer_by(&self, gesture: &[Pt], button: &str, deadline: Deadline) -> Result<()> {
        let g: Vec<Value> = gesture
            .iter()
            .map(|p| json!({"x": p.x, "y": p.y, "t_ms": p.t_ms}))
            .collect();
        self.call_once_sent_by(
            json!({"op": "pointer", "gesture": g, "button": button}),
            deadline,
        )
        .map(|_| ())
    }
    pub fn gesture(&self, paths: &[Vec<Pt>]) -> Result<()> {
        self.gesture_by(paths, Deadline::UNBOUNDED)
    }
    pub fn gesture_by(&self, paths: &[Vec<Pt>], deadline: Deadline) -> Result<()> {
        let pointers: Vec<Value> = paths
            .iter()
            .map(|path| {
                Value::Array(
                    path.iter()
                        .map(|p| json!({ "x": p.x, "y": p.y, "t_ms": p.t_ms }))
                        .collect(),
                )
            })
            .collect();
        self.call_once_sent_by(json!({ "op": "gesture", "pointers": pointers }), deadline)
            .map(|_| ())
    }
    pub fn key(&self, chord: &str) -> Result<()> {
        self.key_by(chord, Deadline::UNBOUNDED)
    }
    pub fn key_by(&self, chord: &str, deadline: Deadline) -> Result<()> {
        self.call_once_sent_by(json!({"op": "key", "chord": chord}), deadline)
            .map(|_| ())
    }
    pub fn text(&self, s: &str) -> Result<()> {
        self.text_by(s, Deadline::UNBOUNDED)
    }
    pub fn text_by(&self, s: &str, deadline: Deadline) -> Result<()> {
        self.call_once_sent_by(json!({"op": "text", "text": s}), deadline)
            .map(|_| ())
    }
}

/// `GLASS_ANDROID_AGENT_JAR`, else `glass-agent.jar` dropped in the glass data dir or
/// next to the `glass-mcp` binary.
pub fn agent_jar(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut dirs = crate::sdk::artifact_data_dirs(get);
    dirs.extend(crate::sdk::exe_dir());
    crate::sdk::resolve_artifact(
        "GLASS_ANDROID_AGENT_JAR",
        "glass-agent.jar",
        &dirs,
        get,
        &|p| p.is_file(),
    )
}

/// The agent is used when not explicitly `off` and a jar is resolvable.
pub fn agent_enabled(get: &dyn Fn(&str) -> Option<String>) -> bool {
    let off = get("GLASS_ANDROID_AGENT")
        .map(|v| v.eq_ignore_ascii_case("off"))
        .unwrap_or(false);
    !off && agent_jar(get).is_some()
}

/// Parse the local port `adb forward tcp:0 …` prints on stdout.
///
/// The first line that reads as a port is it: the blank lines and the `* daemon …` noise adb
/// emits on a cold start read as no port at all, so neither needs a rule of its own.
pub(crate) fn parse_forward_port(out: &str) -> Option<u16> {
    out.lines().map(str::trim).find_map(|l| l.parse().ok())
}

/// A fake agent on a loopback socket: sends `hello`, then answers each request line with the
/// matching `responses[i]`, the request's own id spliced in. Returns the port and the requests it
/// read.
///
/// Records the requests because a client method that returns `Ok(())` having sent nothing is
/// otherwise indistinguishable from one that worked. Lives outside `mod tests` so `input`'s
/// injector tests can reach it.
#[cfg(test)]
pub(crate) fn fake_agent(
    hello: &'static str,
    responses: Vec<&'static str>,
) -> (u16, Arc<Mutex<Vec<Value>>>) {
    fake_agent_sessions(hello, vec![responses])
}

/// [`fake_agent`] across successive connections: `sessions[i]` scripts the i'th connection, whose
/// socket closes once its answers run out. Two sessions is what a dropped-socket reconnect needs.
#[cfg(test)]
pub(crate) fn fake_agent_sessions(
    hello: &'static str,
    sessions: Vec<Vec<&'static str>>,
) -> (u16, Arc<Mutex<Vec<Value>>>) {
    use std::io::{BufRead, BufReader, Write};

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let seen = Arc::new(Mutex::new(Vec::new()));

    let recorded = Arc::clone(&seen);
    std::thread::spawn(move || {
        for session in sessions {
            let Some(sock) = listener.incoming().flatten().next() else {
                return;
            };
            let mut w = sock.try_clone().expect("clone socket");
            let mut r = BufReader::new(sock);
            if writeln!(w, "{hello}").is_err() {
                return;
            }
            for resp in session {
                let mut line = String::new();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let req: Value = serde_json::from_str(&line).expect("request json");
                let id = req["id"].as_i64().expect("request id");
                recorded.lock().expect("seen lock").push(req);
                let mut out: Value = serde_json::from_str(resp).expect("response json");
                // A scripted response that names its own id keeps it. Otherwise it answers the
                // request it was sent, which is the ordinary case — but leaves no way to model
                // an answer addressed to a *different* request, which the client must refuse.
                if out.get("id").is_none() {
                    out["id"] = json!(id);
                }
                if writeln!(w, "{out}").is_err() {
                    break;
                }
            }
        }
    });
    (port, seen)
}

/// The `op` of each request the fake agent read, in order.
#[cfg(test)]
pub(crate) fn ops_seen(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    seen.lock()
        .expect("seen lock")
        .iter()
        .filter_map(|r| r.get("op").and_then(Value::as_str).map(str::to_string))
        .collect()
}

const REMOTE_JAR: &str = "/data/local/tmp/glass-agent.jar";
const SOCKET: &str = "glass-agent";
const MAIN: &str = "com.fixedwidth.glassagent.Main";

/// Owns the device-side agent server's lifecycle: push the jar, launch it via `app_process`,
/// set up `adb forward`, and tear it all down on shutdown. Shared (cloneable) and threaded
/// through the platform factory + the `Glass` shutdown hook, like `EmulatorRegistry`.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    state: Arc<Mutex<Option<AgentProc>>>,
}

/// A launched agent: the backgrounded `adb shell` child (killing it SIGHUPs the device
/// process — no `pkill`), the forwarded local port, and the adb client it was reached through.
///
/// Do not resolve a client at teardown instead: `Adb::from_env` reads the environment as it is
/// *then*, which need not be what launched the agent.
struct AgentProc {
    child: Child,
    port: u16,
    adb: Adb,
}

impl Drop for AgentProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure the agent server is running on `adb`'s device and return the forwarded local
    /// port. Idempotent: a second call returns the cached port when the device serial matches.
    /// If the serial changed (a different device), the stale agent is torn down first.
    ///
    /// `get` reads the environment — the jar's location is all it needs from there — passed in
    /// rather than read here, so a test can point it at a jar without touching the environment
    /// the whole process shares.
    pub fn ensure(&self, adb: &Adb, get: &dyn Fn(&str) -> Option<String>) -> Result<u16> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| GlassError::Backend("agent registry lock poisoned".into()))?;

        // Cache hit: same serial (or both unset) — reuse the existing port.
        if let Some(p) = guard.as_ref()
            && p.adb.serial() == adb.serial()
        {
            return Ok(p.port);
        }
        // Serial changed (or first-ever call with a stale entry): tear down the stale agent.
        // Taking it out of the guard drops it, which kills + reaps the child via Drop.
        if let Some(stale) = guard.take() {
            let _ = stale
                .adb
                .run(["forward", "--remove", &format!("tcp:{}", stale.port)]);
            // stale drops here → Drop kills + reaps the child
        }

        let jar = agent_jar(get)
            .ok_or_else(|| GlassError::Backend("GLASS_ANDROID_AGENT_JAR not set".into()))?;

        // Push the jar (idempotent).
        adb.run(["push", &jar, REMOTE_JAR])?;

        // Launch the server detached. The child is the host-side `adb shell`; killing it on
        // shutdown closes the connection and the device process exits (SIGHUP).
        let serial = adb.serial().map(str::to_string);
        let mut cmd = Command::new(adb.bin());
        if let Some(s) = &serial {
            cmd.args(["-s", s]);
        }
        cmd.args([
            "shell",
            &format!("CLASSPATH={REMOTE_JAR} app_process / {MAIN}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| GlassError::Backend(format!("launch agent: {e}")))?;

        // From here on, any failure must kill + reap the child (Child::drop does NOT kill),
        // so a failed ensure never leaks the host adb process / device app_process / rule.
        let out = match adb.run(["forward", "tcp:0", &format!("localabstract:{SOCKET}")]) {
            Ok(o) => o,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let port = match parse_forward_port(&out) {
            Some(p) => p,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GlassError::Backend(format!(
                    "adb forward gave no port: {out:?}"
                )));
            }
        };
        // Give the server a moment to bind + connect-check it.
        if let Err(e) = wait_for_agent(port).and_then(|c| c.ping()) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = adb.run(["forward", "--remove", &format!("tcp:{port}")]);
            return Err(e);
        }

        *guard = Some(AgentProc {
            child,
            port,
            adb: adb.clone(),
        });
        Ok(port)
    }

    /// Kill the device agent (via the host child) and remove the forward by `deadline`, which the
    /// rest of teardown shares (glass#422). Best-effort.
    ///
    /// Only the forward removal is under the deadline — dropping `p` then kills and reaps the
    /// agent with an unbounded `wait()`, the gap `AndroidPlatform::stop_app_until` names for
    /// logcat.
    pub fn shutdown(&self, deadline: Deadline) {
        if let Ok(mut guard) = self.state.lock()
            && let Some(p) = guard.take()
        {
            let removed = p.adb.run_until(
                ["forward", "--remove", &format!("tcp:{}", p.port)],
                deadline,
            );
            glass_core::note_if_skipped("removing the agent's adb forward", &removed);
            // p drops here → Drop kills + reaps the child
        }
    }
}

/// How long the agent gets to bind its socket and start answering. It takes ~1s in practice.
const AGENT_BIND_BUDGET: Duration = Duration::from_secs(5);

const AGENT_RETRY_PAUSE: Duration = Duration::from_millis(200);

/// Poll until the agent accepts a connection (it takes ~1s to bind).
fn wait_for_agent(port: u16) -> Result<AgentClient> {
    wait_for_agent_until(port, Instant::now() + AGENT_BIND_BUDGET)
}

/// [`wait_for_agent`] against a deadline the caller names, so a test can watch it give up without
/// waiting out the production budget.
fn wait_for_agent_until(port: u16, deadline: Instant) -> Result<AgentClient> {
    loop {
        match AgentClient::connect(port) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() >= deadline => {
                return Err(GlassError::Backend(format!(
                    "agent never came up on :{port}: {e}"
                )));
            }
            Err(_) => std::thread::sleep(AGENT_RETRY_PAUSE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::TimeoutFault;

    /// The deadline-bearing half of the agent's teardown — without it a wedged device leaks an
    /// `adb forward` into a server that outlives glass (glass#422).
    #[test]
    #[cfg(unix)]
    fn a_forward_removal_that_never_answers_gives_up_at_the_shared_deadline() {
        use crate::adb::{Answer, FakeAdb};
        use std::time::{Duration, Instant};

        let fake = FakeAdb::new(&[("*", Answer::Lingers)]);
        let reg = AgentRegistry::new();
        *reg.state.lock().unwrap() = Some(AgentProc {
            child: std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("a child to stand in for the agent"),
            port: 1234,
            adb: fake.adb().clone(),
        });

        let started = Instant::now();
        reg.shutdown(Deadline::at(Instant::now() + Duration::from_millis(300)));

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} on a device that never answers",
            started.elapsed()
        );
        assert!(
            fake.called("forward --remove"),
            "it still has to ask: {:?}",
            fake.calls()
        );
    }
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn enabled_unless_off_and_jar_present() {
        let get = |k: &str| match k {
            "GLASS_ANDROID_AGENT_JAR" => Some("/x/glass-agent.jar".to_string()),
            _ => None,
        };
        assert!(agent_enabled(&get));
        let off = |k: &str| match k {
            "GLASS_ANDROID_AGENT" => Some("off".to_string()),
            "GLASS_ANDROID_AGENT_JAR" => Some("/x/glass-agent.jar".to_string()),
            _ => None,
        };
        assert!(!agent_enabled(&off));
        let no_jar = |_: &str| None;
        assert!(!agent_enabled(&no_jar)); // no jar → disabled
    }

    #[test]
    fn parses_forward_port() {
        assert_eq!(super::parse_forward_port("41234\n"), Some(41234));
        assert_eq!(super::parse_forward_port(""), None);
        assert_eq!(
            super::parse_forward_port(
                "* daemon not running; starting now\n* daemon started successfully\n41234\n"
            ),
            Some(41234)
        );
    }

    const HELLO: &str = r#"{"hello":{"proto":1}}"#;
    const OK: &str = r#"{"ok":true}"#;

    type CountedRequests = Arc<Mutex<Vec<(usize, Value)>>>;

    fn counting_agent() -> (u16, CountedRequests, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{BufRead, BufReader};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let request_log = Arc::clone(&requests);
        let connection_count = Arc::clone(&connections);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let connection = connection_count.fetch_add(1, Ordering::SeqCst) + 1;
                let log = Arc::clone(&request_log);
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone socket");
                    let mut reader = BufReader::new(stream);
                    if writeln!(writer, "{HELLO}").is_err() {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).expect("request json");
                        let id = req["id"].clone();
                        log.lock().expect("request log").push((connection, req));
                        if writeln!(writer, "{}", json!({"id": id, "ok": true})).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, requests, connections)
    }

    /// Read one request on conn1 and lose only its answer. If the client reconnects, conn2 answers
    /// the replay so the caller cannot hang and the request log proves the duplicate dispatch.
    fn agent_that_loses_one_answer() -> (u16, CountedRequests, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader};

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let join = std::thread::spawn(move || {
            let (first, _) = listener.accept().expect("accept conn1");
            let mut first_writer = first.try_clone().expect("clone conn1");
            writeln!(first_writer, "{HELLO}").expect("write conn1 hello");
            let mut first_reader = BufReader::new(first);
            let mut line = String::new();
            first_reader
                .read_line(&mut line)
                .expect("read conn1 request");
            let request = serde_json::from_str(&line).expect("conn1 request json");
            request_log.lock().expect("request log").push((1, request));
            drop(first_reader);
            drop(first_writer);

            listener
                .set_nonblocking(true)
                .expect("make replay observation bounded");
            let until = Instant::now() + Duration::from_millis(300);
            loop {
                match listener.accept() {
                    Ok((second, _)) => {
                        let mut second_writer = second.try_clone().expect("clone conn2");
                        writeln!(second_writer, "{HELLO}").expect("write conn2 hello");
                        let mut second_reader = BufReader::new(second);
                        let mut line = String::new();
                        second_reader
                            .read_line(&mut line)
                            .expect("read conn2 request");
                        let request: Value =
                            serde_json::from_str(&line).expect("conn2 request json");
                        let id = request["id"].clone();
                        request_log.lock().expect("request log").push((2, request));
                        writeln!(second_writer, "{}", json!({"id": id, "ok": true}))
                            .expect("answer conn2 replay");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= until {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept conn2: {error}"),
                }
            }
        });
        (port, requests, join)
    }

    /// Start conn1's answer, let its caller time out, then finish that stale answer after the next
    /// request could have started. A safe client retires conn1 and sends only the distinct second
    /// mutation on conn2.
    fn agent_with_late_partial_answer() -> (u16, CountedRequests, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader};

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let join = std::thread::spawn(move || {
            let (first, _) = listener.accept().expect("accept conn1");
            let mut first_writer = first.try_clone().expect("clone conn1");
            writeln!(first_writer, "{HELLO}").expect("write conn1 hello");
            let mut first_reader = BufReader::new(first);
            let mut line = String::new();
            first_reader
                .read_line(&mut line)
                .expect("read conn1 request");
            let request: Value = serde_json::from_str(&line).expect("conn1 request json");
            let id = request["id"].clone();
            request_log.lock().expect("request log").push((1, request));
            write!(first_writer, r#"{{"id":{id},"#).expect("write partial conn1 answer");
            first_writer.flush().expect("flush partial conn1 answer");
            std::thread::sleep(Duration::from_millis(180));
            let _ = writeln!(first_writer, r#""ok":true}}"#);

            let (second, _) = listener.accept().expect("accept conn2");
            let mut second_writer = second.try_clone().expect("clone conn2");
            writeln!(second_writer, "{HELLO}").expect("write conn2 hello");
            let mut second_reader = BufReader::new(second);
            let mut line = String::new();
            second_reader
                .read_line(&mut line)
                .expect("read conn2 request");
            let request: Value = serde_json::from_str(&line).expect("conn2 request json");
            let id = request["id"].clone();
            request_log.lock().expect("request log").push((2, request));
            writeln!(second_writer, "{}", json!({"id": id, "ok": true}))
                .expect("answer conn2 request");
        });
        (port, requests, join)
    }

    #[test]
    fn read_timeout_install_failure_aborts_before_dispatch() {
        let (port, requests, _) = counting_agent();
        let client = AgentClient::connect(port).expect("connect");
        client
            .conn
            .lock()
            .expect("lock")
            .inject_timeout_fault(TimeoutFault::ReadInstall);

        let result = client.key_by("enter", Deadline::from_millis(1_000));

        assert!(
            result.is_err(),
            "timeout installation failure was discarded"
        );
        assert!(
            requests.lock().expect("request log").is_empty(),
            "the request was dispatched after timeout installation failed"
        );
    }

    #[test]
    fn write_timeout_install_failure_aborts_before_dispatch() {
        let (port, requests, _) = counting_agent();
        let client = AgentClient::connect(port).expect("connect");
        client
            .conn
            .lock()
            .expect("lock")
            .inject_timeout_fault(TimeoutFault::WriteInstall);

        let result = client.key_by("enter", Deadline::from_millis(1_000));

        assert!(
            result.is_err(),
            "timeout installation failure was discarded"
        );
        assert!(
            requests.lock().expect("request log").is_empty(),
            "the request was dispatched after timeout installation failed"
        );
    }

    #[test]
    fn unbounded_timeout_install_failure_aborts_before_dispatch() {
        let (port, requests, _) = counting_agent();
        let client = AgentClient::connect(port).expect("connect");
        client
            .conn
            .lock()
            .expect("lock")
            .inject_timeout_fault(TimeoutFault::ReadInstall);

        let result = client.key("enter");

        assert!(
            result.is_err(),
            "timeout installation failure was discarded"
        );
        assert!(
            requests.lock().expect("request log").is_empty(),
            "the unbounded request was dispatched after timeout installation failed"
        );
    }

    #[test]
    fn restoration_failure_poisoned_connection_reconnects_before_next_request() {
        use std::sync::atomic::Ordering;

        for fault in [TimeoutFault::ReadRestore, TimeoutFault::WriteRestore] {
            let (port, requests, connections) = counting_agent();
            let client = AgentClient::connect(port).expect("connect");
            client
                .conn
                .lock()
                .expect("lock")
                .inject_timeout_fault(fault);

            let first = client.key_by("enter", Deadline::from_millis(1_000));
            assert!(
                first.is_err(),
                "{fault:?} timeout restoration failure was discarded"
            );
            client.ping().expect("the next request reconnects");

            let seen = requests.lock().expect("request log");
            assert_eq!(seen.len(), 2, "{fault:?}");
            assert_eq!(seen[0].0, 1, "{fault:?}");
            assert_eq!(seen[1].0, 2, "{fault:?}: poisoned connection reused");
            assert_eq!(connections.load(Ordering::SeqCst), 2, "{fault:?}");
        }
    }

    #[test]
    fn restoration_failure_after_reply_is_not_retried_as_the_same_mutation() {
        for fault in [TimeoutFault::ReadRestore, TimeoutFault::WriteRestore] {
            let (port, requests, _) = counting_agent();
            let client = AgentClient::connect(port).expect("connect");
            client
                .conn
                .lock()
                .expect("lock")
                .inject_timeout_fault(fault);

            let result = client.key_by("enter", Deadline::from_millis(1_000));

            assert!(
                result.is_err(),
                "{fault:?} timeout restoration failure was discarded"
            );
            let mutation_request_count = requests
                .lock()
                .expect("request log")
                .iter()
                .filter(|(_, request)| request["op"] == "key")
                .count();
            assert_eq!(mutation_request_count, 1, "{fault:?}");
        }
    }

    #[test]
    fn mutating_requests_do_not_replay_when_their_answer_is_lost() {
        enum Mutation {
            Pointer,
            Gesture,
            Key,
            Text,
        }

        let path = vec![Pt {
            x: 5,
            y: 10,
            t_ms: 0,
        }];
        for (expected_op, mutation) in [
            ("pointer", Mutation::Pointer),
            ("gesture", Mutation::Gesture),
            ("key", Mutation::Key),
            ("text", Mutation::Text),
        ] {
            let (port, requests, join) = agent_that_loses_one_answer();
            let client = AgentClient::connect(port).expect("connect");

            let result = match mutation {
                Mutation::Pointer => client.pointer(&path, "left"),
                Mutation::Gesture => client.gesture(std::slice::from_ref(&path)),
                Mutation::Key => client.key("enter"),
                Mutation::Text => client.text("hello"),
            };

            assert!(result.is_err(), "{expected_op} replay hid the lost answer");
            join.join().expect("fake agent");
            let seen = requests.lock().expect("request log");
            assert_eq!(seen.len(), 1, "{expected_op} was replayed on conn2");
            assert_eq!(seen[0].0, 1, "{expected_op}");
            assert_eq!(seen[0].1["op"], expected_op, "{expected_op}");
        }
    }

    #[test]
    fn answer_lost_retires_partial_stream_before_the_next_distinct_mutation() {
        let (port, requests, join) = agent_with_late_partial_answer();
        let client = AgentClient::connect(port).expect("connect");

        let first = client
            .key_by("a", Deadline::from_millis(100))
            .expect_err("conn1 finishes its answer after the caller deadline");
        assert_eq!(
            first.bound_owner(),
            Some(glass_core::Whose::Caller),
            "{first}"
        );
        assert_eq!(
            first.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched),
            "{first}"
        );
        client
            .key_by("b", Deadline::from_millis(1_000))
            .expect("the distinct mutation uses a clean conn2");

        join.join().expect("fake agent");
        let seen = requests.lock().expect("request log");
        assert_eq!(seen.len(), 2, "a mutation was replayed or lost: {seen:?}");
        assert_eq!((seen[0].0, seen[0].1["op"].as_str()), (1, Some("key")));
        assert_eq!(seen[0].1["chord"], "a");
        assert_eq!((seen[1].0, seen[1].1["op"].as_str()), (2, Some("key")));
        assert_eq!(seen[1].1["chord"], "b");
    }

    #[test]
    fn connect_checks_proto() {
        let (bad, _) = fake_agent(r#"{"hello":{"proto":99}}"#, vec![]);
        assert!(AgentClient::connect(bad).is_err());
    }

    #[test]
    fn clipboard_roundtrip_and_ok() {
        let (port, seen) = fake_agent(HELLO, vec![OK, r#"{"ok":true,"text":"hey"}"#]);
        let c = AgentClient::connect(port).unwrap();
        c.clipboard_set("hey").unwrap();
        assert_eq!(c.clipboard_get().unwrap(), "hey");
        assert_eq!(ops_seen(&seen), ["clipboard_set", "clipboard_get"]);
    }

    #[test]
    fn error_response_becomes_backend_error() {
        let (port, _) = fake_agent(HELLO, vec![r#"{"ok":false,"error":"nope"}"#]);
        let c = AgentClient::connect(port).unwrap();
        let e = c.ping().unwrap_err();
        assert!(e.to_string().contains("nope"));
    }

    #[test]
    fn clipboard_get_missing_text_errors() {
        let (port, _) = fake_agent(HELLO, vec![OK]);
        let c = AgentClient::connect(port).unwrap();
        assert!(c.clipboard_get().is_err());
    }

    #[test]
    fn clipboard_get_empty_is_ok() {
        let (port, _) = fake_agent(HELLO, vec![r#"{"ok":true,"text":""}"#]);
        let c = AgentClient::connect(port).unwrap();
        assert_eq!(c.clipboard_get().unwrap(), "");
    }

    /// Every input method, checked by what reached the wire rather than by its return value.
    ///
    /// The device is what carries these out, so one that answered `Ok` having sent nothing would
    /// be indistinguishable here until something reads the request back.
    #[test]
    fn each_input_method_sends_its_own_request() {
        let (port, seen) = fake_agent(HELLO, vec![OK, OK, OK, OK]);
        let c = AgentClient::connect(port).unwrap();
        let path = vec![
            Pt {
                x: 5,
                y: 10,
                t_ms: 0,
            },
            Pt {
                x: 7,
                y: 12,
                t_ms: 40,
            },
        ];

        c.pointer(&path, "left").unwrap();
        c.gesture(&[path.clone(), path]).unwrap();
        c.key("ctrl+a").unwrap();
        c.text("hi").unwrap();

        assert_eq!(ops_seen(&seen), ["pointer", "gesture", "key", "text"]);
        let sent = seen.lock().unwrap();
        assert_eq!(
            sent[0]["gesture"],
            json!([{"x": 5, "y": 10, "t_ms": 0}, {"x": 7, "y": 12, "t_ms": 40}])
        );
        assert_eq!(sent[0]["button"], "left");
        // One array per pointer, each carrying that pointer's whole path — the shape
        // multi-touch needs, and the one a flattened list would quietly lose.
        assert_eq!(sent[1]["pointers"].as_array().map(Vec::len), Some(2));
        assert_eq!(sent[1]["pointers"][1][1]["x"], 7);
        assert_eq!(sent[2]["chord"], "ctrl+a");
        assert_eq!(sent[3]["text"], "hi");
    }

    #[test]
    fn agent_call_restores_socket_timeouts_after_a_bounded_request() {
        let (port, _) = fake_agent(HELLO, vec![OK]);
        let client = AgentClient::connect(port).unwrap();
        client
            .key_by("enter", Deadline::from_millis(1_000))
            .unwrap();
        let conn = client.conn.lock().unwrap();
        assert_eq!(
            conn.writer.write_timeout().unwrap(),
            Some(crate::conn::STANDING_TIMEOUT)
        );
        assert_eq!(
            conn.reader.get_ref().read_timeout().unwrap(),
            Some(crate::conn::STANDING_TIMEOUT)
        );
    }

    #[test]
    fn deadline_expiring_while_waiting_for_connection_lock_dispatches_nothing() {
        let (port, seen) = fake_agent(HELLO, vec![OK]);
        let client = Arc::new(AgentClient::connect(port).unwrap());
        let held = client.conn.lock().unwrap();
        let caller = Arc::clone(&client);
        let join = std::thread::spawn(move || caller.key_by("enter", Deadline::from_millis(100)));
        std::thread::sleep(Duration::from_millis(180));
        drop(held);
        let err = join.join().unwrap().unwrap_err();
        assert!(matches!(err, GlassError::Bounded { .. }), "{err}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn reconnect_hello_is_bounded_and_dispatches_no_retry_after_expiry() {
        use std::io::{BufRead, BufReader};

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let retried = Arc::new(Mutex::new(Vec::<String>::new()));
        let retry_log = Arc::clone(&retried);
        std::thread::spawn(move || {
            let (first, _) = listener.accept().unwrap();
            let mut first_writer = first.try_clone().unwrap();
            writeln!(first_writer, "{HELLO}").unwrap();
            let mut line = String::new();
            BufReader::new(first).read_line(&mut line).unwrap();
            drop(first_writer);

            let (second, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(400));
            let mut second_writer = second.try_clone().unwrap();
            let _ = writeln!(second_writer, "{HELLO}");
            let mut line = String::new();
            if BufReader::new(second).read_line(&mut line).unwrap_or(0) != 0 {
                retry_log.lock().unwrap().push(line);
            }
        });

        let client = AgentClient::connect(port).unwrap();
        let started = Instant::now();
        let err = client
            .call_by(json!({"op": "ping"}), Deadline::from_millis(150))
            .unwrap_err();
        assert!(matches!(err, GlassError::Bounded { .. }), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(retried.lock().unwrap().is_empty());
    }

    #[test]
    fn a_call_whose_socket_dropped_is_retried_on_a_fresh_connection() {
        // The device's accept loop takes a new connection after a drop, so a dead socket is not
        // a dead agent — reporting one would surface a hiccup as a tap that never happened.
        let (port, seen) = fake_agent_sessions(HELLO, vec![vec![], vec![OK]]);
        let c = AgentClient::connect(port).unwrap();
        c.ping()
            .expect("a dropped socket must be reconnected, not reported");
        assert_eq!(ops_seen(&seen), ["ping"]);
    }

    /// A listener that closes the first `stumbles` connections without a hello — an agent whose
    /// socket is up but which is not answering yet — and then serves one properly.
    fn agent_that_answers_after(stumbles: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            let mut closed = 0;
            for mut stream in listener.incoming().flatten() {
                if closed < stumbles {
                    closed += 1;
                    continue; // dropping the stream closes it, having said nothing
                }
                let _ = writeln!(stream, "{HELLO}");
                return;
            }
        });
        port
    }

    #[test]
    fn an_agent_that_is_not_answering_yet_is_retried_rather_than_given_up_on() {
        // The device takes about a second to bind, so a failed first attempt is the ordinary
        // case; a backwards deadline makes it the answer instead.
        let port = agent_that_answers_after(1);
        wait_for_agent(port).expect("an agent that answers on the second try must be waited for");
    }

    #[test]
    #[cfg(unix)]
    fn ensuring_the_agent_pushes_it_launches_it_and_forwards_a_port_to_it() {
        use crate::adb::{Answer, FakeAdb, still_running};

        let (agent_port, _) = fake_agent(HELLO, vec![OK]);
        let forwarded = Answer::says(format!("{agent_port}\n"));
        let (lingers, silent) = (Answer::Lingers, Answer::Silent);
        // Specific rules first: the catch-all would otherwise answer everything.
        let fake = FakeAdb::scripted(&[
            ("forward tcp:0 *", vec![&forwarded]),
            ("shell CLASSPATH=*", vec![&lingers]),
            ("*", vec![&silent]),
        ]);
        // Any path will do: nothing checks the jar exists before pushing it.
        let jar = fake.adb().bin().to_string();
        let get = move |k: &str| match k {
            "GLASS_ANDROID_AGENT_JAR" => Some(jar.clone()),
            _ => None,
        };

        let registry = AgentRegistry::new();
        let port = registry
            .ensure(fake.adb(), &get)
            .expect("the agent answers on the port adb forwarded");

        assert_eq!(
            port, agent_port,
            "the forwarded port is the one handed back"
        );
        assert!(fake.called("push"), "{:?}", fake.calls());
        assert!(fake.called("app_process"), "{:?}", fake.calls());

        // A second call reuses the running agent. Pushing and relaunching per call would
        // restart the companion under whatever was mid-gesture against it.
        assert_eq!(registry.ensure(fake.adb(), &get).unwrap(), agent_port);
        assert_eq!(
            fake.calls().iter().filter(|c| c.contains("push")).count(),
            1,
            "{:?}",
            fake.calls()
        );

        // The launch is a child that stays up; killing it is what SIGHUPs the device process.
        let child = fake.wait_read("linger.pid", Duration::from_secs(5));
        assert!(!child.is_empty(), "the launch should still be running");
        assert!(still_running(&child));

        registry.shutdown(Deadline::UNBOUNDED);
        assert!(fake.called("forward --remove"), "{:?}", fake.calls());
        assert!(
            !still_running(&child),
            "the adb shell holding the device agent outlived shutdown"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_that_cannot_be_forwarded_a_port_leaves_no_child_behind() {
        // Every failure after the launch has to kill and reap the child itself: `Child::drop`
        // does not, so a bailout that forgets leaves one adb shell — and one device-side
        // app_process — per attempt.
        use crate::adb::{Answer, FakeAdb, still_running};

        let (lingers, quiet) = (Answer::Lingers, Answer::says(""));
        let fake = FakeAdb::scripted(&[
            ("forward tcp:0 *", vec![&quiet]),
            ("shell CLASSPATH=*", vec![&lingers]),
            ("*", vec![&Answer::Silent]),
        ]);
        let jar = fake.adb().bin().to_string();
        let get = move |k: &str| match k {
            "GLASS_ANDROID_AGENT_JAR" => Some(jar.clone()),
            _ => None,
        };

        let registry = AgentRegistry::new();
        let err = registry
            .ensure(fake.adb(), &get)
            .expect_err("a forward that names no port cannot be connected to");
        assert!(err.to_string().contains("no port"), "{err}");

        let child = fake.wait_read("linger.pid", Duration::from_secs(5));
        assert!(!child.is_empty(), "the launch should have happened");
        assert!(!still_running(&child), "the failed launch leaked its child");
    }

    #[test]
    fn an_agent_that_never_answers_is_given_up_on_at_the_deadline() {
        let port = agent_that_answers_after(usize::MAX);
        let started = Instant::now();
        let Err(err) = wait_for_agent_until(port, Instant::now() + Duration::from_millis(300))
        else {
            panic!("an agent that never answers must not be waited on forever");
        };
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} — the deadline never ended the loop",
            started.elapsed()
        );
        assert!(
            err.to_string().contains(&port.to_string()),
            "the error must name the port: {err}"
        );
    }
}
