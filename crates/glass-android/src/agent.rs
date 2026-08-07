//! Host-side client for the on-device `glass-android-agent` (the `glass-android-agent`
//! repo): line-delimited JSON over a TCP socket that `adb forward` maps to the device's
//! `localabstract:glass-agent`. `AgentClient` is the request/response client; `AgentRegistry`
//! owns the device server's lifecycle. Everything degrades to the adb paths on failure.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| GlassError::Backend("agent client lock poisoned".into()))?;
        match conn.call(req.clone()) {
            Ok(v) => Ok(v),
            Err(f) if f.is_transport() => {
                // The agent's accept loop accepts a fresh connection after a drop.
                *conn = Conn::open(self.port)?;
                conn.call(req).map_err(CallFailure::into_error)
            }
            Err(f) => Err(f.into_error()),
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
        let g: Vec<Value> = gesture
            .iter()
            .map(|p| json!({"x": p.x, "y": p.y, "t_ms": p.t_ms}))
            .collect();
        self.call(json!({"op": "pointer", "gesture": g, "button": button}))
            .map(|_| ())
    }
    pub fn gesture(&self, paths: &[Vec<Pt>]) -> Result<()> {
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
        self.call(json!({ "op": "gesture", "pointers": pointers }))
            .map(|_| ())
    }
    pub fn key(&self, chord: &str) -> Result<()> {
        self.call(json!({"op": "key", "chord": chord})).map(|_| ())
    }
    pub fn text(&self, s: &str) -> Result<()> {
        self.call(json!({"op": "text", "text": s})).map(|_| ())
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
/// The requests are the point. The companion is an APK on an emulator, so a unit test can only
/// reach this protocol — and a client method that returns `Ok(())` having sent nothing looks
/// exactly like one that worked until something reads back what went out.
///
/// Lives beside the code rather than in `mod tests` so `input`'s injector tests can reach it too.
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
                out["id"] = json!(id);
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
/// process — no `pkill`), the forwarded local port, and the device serial it was bound to.
struct AgentProc {
    child: Child,
    port: u16,
    serial: Option<String>,
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
    /// The jar is resolved from env.
    pub fn ensure(&self, adb: &Adb) -> Result<u16> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| GlassError::Backend("agent registry lock poisoned".into()))?;

        // Cache hit: same serial (or both unset) — reuse the existing port.
        if let Some(p) = guard.as_ref()
            && p.serial.as_deref() == adb.serial()
        {
            return Ok(p.port);
        }
        // Serial changed (or first-ever call with a stale entry): tear down the stale agent.
        // Taking it out of the guard drops it, which kills + reaps the child via Drop.
        if let Some(stale) = guard.take() {
            let stale_adb = Adb::from_env();
            let stale_adb = match &stale.serial {
                Some(s) => stale_adb.with_serial(s.clone()),
                None => stale_adb,
            };
            let _ = stale_adb.run(["forward", "--remove", &format!("tcp:{}", stale.port)]);
            // stale drops here → Drop kills + reaps the child
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
            serial,
        });
        Ok(port)
    }

    /// Kill the device agent (via the host child) and remove the forward. Best-effort.
    pub fn shutdown(&self) {
        if let Ok(mut guard) = self.state.lock()
            && let Some(p) = guard.take()
        {
            let adb = Adb::from_env();
            let adb = match &p.serial {
                Some(s) => adb.with_serial(s.clone()),
                None => adb,
            };
            let _ = adb.run(["forward", "--remove", &format!("tcp:{}", p.port)]);
            // p drops here → Drop kills + reaps the child
        }
    }
}

/// How long the agent gets to bind its socket and start answering. It takes ~1s in practice.
const AGENT_BIND_BUDGET: Duration = Duration::from_secs(5);

/// How long to leave the agent alone between attempts.
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
    fn a_call_whose_socket_dropped_is_retried_on_a_fresh_connection() {
        // The device's accept loop takes a new connection after a drop, so a dead socket is not
        // a dead agent. Reporting one as a failed input would surface a transport hiccup as a
        // tap that did not happen.
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
        // The device takes about a second to bind, so a first attempt that fails is the ordinary
        // case rather than the failure. A deadline computed backwards — or a comparison the
        // wrong way round — makes that first attempt the answer.
        let port = agent_that_answers_after(1);
        wait_for_agent(port).expect("an agent that answers on the second try must be waited for");
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
