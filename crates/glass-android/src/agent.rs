//! Host-side client for the on-device `glass-android-agent` (the `glass-android-agent`
//! repo): line-delimited JSON over a TCP socket that `adb forward` maps to the device's
//! `localabstract:glass-agent`. `AgentClient` is the request/response client; `AgentRegistry`
//! owns the device server's lifecycle. Everything degrades to the adb paths on failure.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use glass_core::{GlassError, Result};
use serde_json::{json, Value};

use crate::adb::Adb;

/// One absolute-display point in a pointer path (the agent's gesture element).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
    pub t_ms: u64,
}

/// The protocol version this client speaks (must match the agent's hello `proto`).
const PROTO: i64 = 1;

/// A live connection to the agent: a framed line reader/writer + a monotonic id.
struct Conn {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: i64,
}

impl Conn {
    /// Connect to `127.0.0.1:port`, read + version-check the hello banner.
    fn open(port: u16) -> Result<Conn> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| GlassError::Backend(format!("agent connect :{port}: {e}")))?;
        let reader = BufReader::new(
            stream.try_clone().map_err(|e| GlassError::Backend(format!("agent clone: {e}")))?,
        );
        let mut c = Conn { writer: stream, reader, next_id: 1 };
        let hello = c.read_line()?;
        let v: Value = serde_json::from_str(&hello)
            .map_err(|e| GlassError::Backend(format!("agent hello parse: {e}")))?;
        let proto = v.get("hello").and_then(|h| h.get("proto")).and_then(Value::as_i64);
        if proto != Some(PROTO) {
            return Err(GlassError::Backend(format!(
                "agent protocol mismatch: got {proto:?}, want {PROTO}"
            )));
        }
        Ok(c)
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .map_err(|e| GlassError::Backend(format!("agent read: {e}")))?;
        if n == 0 {
            return Err(GlassError::Backend("agent closed the connection".into()));
        }
        Ok(line.trim_end().to_string())
    }

    /// Send one request object (an `id` is injected) and return the response `Value`.
    /// Returns `(result, io_error)` — `io_error` is true when the failure was a
    /// transport I/O error (dropped connection) rather than a protocol-level error.
    fn call(&mut self, mut req: Value) -> std::result::Result<Value, (GlassError, bool)> {
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        let mut line = serde_json::to_string(&req).expect("serialize request");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.flush())
            .map_err(|e| (GlassError::Backend(format!("agent write: {e}")), true))?;
        let resp_line = self.read_line().map_err(|e| (e, true))?;
        let resp: Value = serde_json::from_str(&resp_line)
            .map_err(|e| (GlassError::Backend(format!("agent resp parse: {e}")), false))?;
        if resp.get("id").and_then(Value::as_i64) != Some(id) {
            return Err((
                GlassError::Backend(format!(
                    "agent response id mismatch (got {:?}, want {id})",
                    resp.get("id")
                )),
                false,
            ));
        }
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp.get("error").and_then(Value::as_str).unwrap_or("agent error");
            return Err((GlassError::Backend(format!("agent: {err}")), false));
        }
        Ok(resp)
    }
}

/// Request/response client to the agent. `connect` reconnects on a dropped socket once.
pub struct AgentClient {
    port: u16,
    conn: Mutex<Conn>,
}

impl AgentClient {
    pub fn connect(port: u16) -> Result<AgentClient> {
        Ok(AgentClient { port, conn: Mutex::new(Conn::open(port)?) })
    }

    /// Run a request, transparently reconnecting once if the socket dropped.
    fn call(&self, req: Value) -> Result<Value> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| GlassError::Backend("agent client lock poisoned".into()))?;
        match conn.call(req.clone()) {
            Ok(v) => Ok(v),
            Err((e, false)) => Err(e),
            Err((_, true)) => {
                // The agent's accept loop accepts a fresh connection after a drop.
                *conn = Conn::open(self.port)?;
                conn.call(req).map_err(|(e, _)| e)
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
            .ok_or_else(|| GlassError::Backend("agent clipboard_get: response missing `text`".into()))
    }
    pub fn clipboard_set(&self, text: &str) -> Result<()> {
        self.call(json!({"op": "clipboard_set", "text": text})).map(|_| ())
    }
    pub fn pointer(&self, gesture: &[Pt], button: &str) -> Result<()> {
        let g: Vec<Value> = gesture
            .iter()
            .map(|p| json!({"x": p.x, "y": p.y, "t_ms": p.t_ms}))
            .collect();
        self.call(json!({"op": "pointer", "gesture": g, "button": button})).map(|_| ())
    }
    pub fn key(&self, chord: &str) -> Result<()> {
        self.call(json!({"op": "key", "chord": chord})).map(|_| ())
    }
    pub fn text(&self, s: &str) -> Result<()> {
        self.call(json!({"op": "text", "text": s})).map(|_| ())
    }
}

/// `GLASS_ANDROID_AGENT_JAR`, if set + non-empty.
pub fn agent_jar(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    get("GLASS_ANDROID_AGENT_JAR").filter(|s| !s.is_empty())
}

/// The agent is used when not explicitly `off` and a jar is resolvable.
pub fn agent_enabled(get: &dyn Fn(&str) -> Option<String>) -> bool {
    let off = get("GLASS_ANDROID_AGENT").map(|v| v.eq_ignore_ascii_case("off")).unwrap_or(false);
    !off && agent_jar(get).is_some()
}

/// Parse the local port `adb forward tcp:0 …` prints on stdout.
fn parse_forward_port(out: &str) -> Option<u16> {
    out.trim().lines().next()?.trim().parse().ok()
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
/// process — no `pkill`) and the forwarded local port.
struct AgentProc {
    child: Child,
    port: u16,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure the agent server is running on `adb`'s device and return the forwarded local
    /// port. Idempotent: a second call returns the cached port. The jar is resolved from env.
    pub fn ensure(&self, adb: &Adb) -> Result<u16> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| GlassError::Backend("agent registry lock poisoned".into()))?;
        if let Some(p) = guard.as_ref() {
            return Ok(p.port);
        }
        let get = |k: &str| std::env::var(k).ok();
        let jar = agent_jar(&get)
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
        cmd.args(["shell", &format!("CLASSPATH={REMOTE_JAR} app_process / {MAIN}")])
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
                return Err(GlassError::Backend(format!("adb forward gave no port: {out:?}")));
            }
        };
        // Give the server a moment to bind + connect-check it.
        if let Err(e) = wait_for_agent(port).and_then(|c| c.ping()) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = adb.run(["forward", "--remove", &format!("tcp:{port}")]);
            return Err(e);
        }

        *guard = Some(AgentProc { child, port });
        Ok(port)
    }

    /// Kill the device agent (via the host child) and remove the forward. Best-effort.
    pub fn shutdown(&self) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(mut p) = guard.take() {
                let _ = p.child.kill();
                let _ = p.child.wait();
                let adb = Adb::from_env();
                let _ = adb.run(["forward", "--remove", &format!("tcp:{}", p.port)]);
            }
        }
    }
}

/// Poll until the agent accepts a connection (it takes ~1s to bind), up to ~5s.
fn wait_for_agent(port: u16) -> Result<AgentClient> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match AgentClient::connect(port) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() >= deadline => {
                return Err(GlassError::Backend(format!("agent never came up on :{port}: {e}")))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
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
    }

    /// Spawn a one-shot fake agent that sends `hello`, then for each request line writes
    /// the matching `responses[i]` (with the request's id spliced in). Returns the port.
    fn fake_agent(hello: &'static str, responses: Vec<&'static str>) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut w = sock.try_clone().unwrap();
            let mut r = BufReader::new(sock);
            writeln!(w, "{hello}").unwrap();
            w.flush().unwrap();
            for resp in responses {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let id = serde_json::from_str::<Value>(&line).unwrap()["id"].as_i64().unwrap();
                let mut out: Value = serde_json::from_str(resp).unwrap();
                out["id"] = json!(id);
                writeln!(w, "{out}").unwrap();
                w.flush().unwrap();
            }
        });
        port
    }

    #[test]
    fn connect_checks_proto() {
        let bad = fake_agent(r#"{"hello":{"proto":99}}"#, vec![]);
        assert!(AgentClient::connect(bad).is_err());
    }

    #[test]
    fn clipboard_roundtrip_and_ok() {
        let port = fake_agent(
            r#"{"hello":{"proto":1}}"#,
            vec![r#"{"ok":true}"#, r#"{"ok":true,"text":"hey"}"#],
        );
        let c = AgentClient::connect(port).unwrap();
        c.clipboard_set("hey").unwrap();
        assert_eq!(c.clipboard_get().unwrap(), "hey");
    }

    #[test]
    fn error_response_becomes_backend_error() {
        let port = fake_agent(r#"{"hello":{"proto":1}}"#, vec![r#"{"ok":false,"error":"nope"}"#]);
        let c = AgentClient::connect(port).unwrap();
        let e = c.ping().unwrap_err();
        assert!(e.to_string().contains("nope"));
    }

    #[test]
    fn clipboard_get_missing_text_errors() {
        let port = fake_agent(r#"{"hello":{"proto":1}}"#, vec![r#"{"ok":true}"#]);
        let c = AgentClient::connect(port).unwrap();
        assert!(c.clipboard_get().is_err());
    }

    #[test]
    fn clipboard_get_empty_is_ok() {
        let port = fake_agent(r#"{"hello":{"proto":1}}"#, vec![r#"{"ok":true,"text":""}"#]);
        let c = AgentClient::connect(port).unwrap();
        assert_eq!(c.clipboard_get().unwrap(), "");
    }

    #[test]
    fn pointer_and_key_send_ok() {
        let port = fake_agent(
            r#"{"hello":{"proto":1}}"#,
            vec![r#"{"ok":true}"#, r#"{"ok":true}"#, r#"{"ok":true}"#],
        );
        let c = AgentClient::connect(port).unwrap();
        c.pointer(&[Pt { x: 5, y: 10, t_ms: 0 }], "left").unwrap();
        c.key("ctrl+a").unwrap();
        c.text("hi").unwrap();
    }
}
