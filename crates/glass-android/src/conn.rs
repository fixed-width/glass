//! Shared line-delimited-JSON TCP connection used by the agent client and the
//! a11y-service client. `Conn` opens a TCP socket, wraps it in a `BufReader` for
//! line reads, and exposes a `call` method that writes one JSON request line and
//! reads one JSON response line.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use glass_core::GlassError;
use serde_json::{Value, json};

/// The protocol version this client speaks (must match the agent's hello `proto`).
pub(crate) const PROTO: i64 = 1;

/// Why one [`Conn::call`] failed, and what it says about delivery. `ACTION_CLICK` is not
/// idempotent, so re-sending one whose answer was merely lost actuates the control twice.
pub(crate) enum CallFailure {
    /// The write failed, so the request never reached the device.
    NotSent(GlassError),
    /// The request went out and no answer came back; it may or may not have run.
    AnswerLost(GlassError),
    /// The device answered — with a refusal, or with something this client could not read as
    /// a success. Either way it ran what it was going to run.
    Refused(GlassError),
}

impl CallFailure {
    /// The error to surface.
    pub(crate) fn into_error(self) -> GlassError {
        match self {
            CallFailure::NotSent(e) | CallFailure::AnswerLost(e) | CallFailure::Refused(e) => e,
        }
    }

    /// Whether the socket failed rather than the device answering.
    pub(crate) fn is_transport(&self) -> bool {
        matches!(self, CallFailure::NotSent(_) | CallFailure::AnswerLost(_))
    }

    /// Whether nothing reached the device, so re-sending cannot run the request twice.
    pub(crate) fn nothing_sent(&self) -> bool {
        matches!(self, CallFailure::NotSent(_))
    }

    /// This classification carrying `e` instead: a failure hit while recovering from this one
    /// says no more about delivery.
    pub(crate) fn with_error(&self, e: GlassError) -> CallFailure {
        match self {
            CallFailure::NotSent(_) => CallFailure::NotSent(e),
            CallFailure::AnswerLost(_) => CallFailure::AnswerLost(e),
            CallFailure::Refused(_) => CallFailure::Refused(e),
        }
    }
}

/// A live connection to the agent: a framed line reader/writer + a monotonic id.
/// How long a read blocks for the companion when no caller named a bound — long enough that a
/// stalled agent surfaces as a transport error the reconnect path handles, rather than hanging the
/// single-threaded MCP loop forever.
const STANDING_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Conn {
    pub(crate) writer: TcpStream,
    pub(crate) reader: BufReader<TcpStream>,
    pub(crate) next_id: i64,
}

impl Conn {
    /// Connect to `127.0.0.1:port`, read + version-check the hello banner.
    pub(crate) fn open(port: u16) -> glass_core::Result<Conn> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| GlassError::Backend(format!("agent connect :{port}: {e}")))?;
        // Set read/write timeouts so a stalled agent surfaces as a transport error (which
        // the existing reconnect path handles) rather than hanging the MCP thread forever.
        stream.set_read_timeout(Some(STANDING_TIMEOUT)).ok();
        stream.set_write_timeout(Some(STANDING_TIMEOUT)).ok();
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| GlassError::Backend(format!("agent clone: {e}")))?,
        );
        let mut c = Conn {
            writer: stream,
            reader,
            next_id: 1,
        };
        let hello = c.read_line()?;
        let v: Value = serde_json::from_str(&hello)
            .map_err(|e| GlassError::Backend(format!("agent hello parse: {e}")))?;
        let proto = v
            .get("hello")
            .and_then(|h| h.get("proto"))
            .and_then(Value::as_i64);
        if proto != Some(PROTO) {
            return Err(GlassError::Backend(format!(
                "agent protocol mismatch: got {proto:?}, want {PROTO}"
            )));
        }
        Ok(c)
    }

    pub(crate) fn read_line(&mut self) -> glass_core::Result<String> {
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

    /// Bound how long the next reads may block for, or restore the standing timeout when the
    /// caller named nothing.
    ///
    /// Per-call rather than per-connection: the socket outlives any one request, so a bound left
    /// behind would be applied to a later call that never agreed to it.
    pub(crate) fn read_within(&mut self, wait: Option<Duration>) {
        self.writer
            .set_read_timeout(Some(wait.unwrap_or(STANDING_TIMEOUT)))
            .ok();
    }

    /// Send one request object (an `id` is injected) and return the response `Value`.
    /// A failure is classified by how far the request got — see [`CallFailure`].
    pub(crate) fn call(&mut self, mut req: Value) -> std::result::Result<Value, CallFailure> {
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        let mut line = serde_json::to_string(&req).expect("serialize request");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.flush())
            .map_err(|e| CallFailure::NotSent(GlassError::Backend(format!("agent write: {e}"))))?;
        let resp_line = self.read_line().map_err(CallFailure::AnswerLost)?;
        let resp: Value = serde_json::from_str(&resp_line).map_err(|e| {
            CallFailure::Refused(GlassError::Backend(format!("agent resp parse: {e}")))
        })?;
        if resp.get("id").and_then(Value::as_i64) != Some(id) {
            return Err(CallFailure::Refused(GlassError::Backend(format!(
                "agent response id mismatch (got {:?}, want {id})",
                resp.get("id")
            ))));
        }
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("agent error");
            return Err(CallFailure::Refused(GlassError::Backend(format!(
                "agent: {err}"
            ))));
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake_agent;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    const HELLO: &str = r#"{"hello":{"proto":1}}"#;
    const OK: &str = r#"{"ok":true}"#;

    /// A listener that says hello and then answers nothing, holding the connection open — a
    /// companion that stopped responding without dropping the socket, which is the only case a
    /// read timeout is there for. A closed socket ends the read on its own.
    fn silent_after_hello() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = writeln!(stream, "{HELLO}");
                std::thread::sleep(Duration::from_secs(60));
            }
        });
        port
    }

    #[test]
    fn a_bounded_read_gives_up_at_the_bound_and_not_at_the_standing_timeout() {
        // A caller that named a deadline gets it. Without this the wait is the 30s standing
        // timeout, which is the single-threaded MCP loop blocked for half a minute on a
        // companion that has already stopped talking.
        let mut conn = Conn::open(silent_after_hello()).expect("the hello arrives");
        conn.read_within(Some(Duration::from_millis(200)));

        let started = Instant::now();
        let Err(failure) = conn.call(json!({"op": "ping"})) else {
            panic!("a companion that answers nothing cannot have answered");
        };
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} — the bound never reached the socket",
            started.elapsed()
        );
        assert!(
            failure.is_transport(),
            "a read that ran out of time is a transport failure, not a refusal"
        );
    }

    #[test]
    fn every_request_carries_an_id_of_its_own() {
        // The id is what matches an answer to its question. Ids that repeat — or that run
        // backwards into one already used — let a late answer satisfy a later call, and on this
        // protocol that means one tap's reply standing in for another's.
        let (port, seen) = fake_agent(HELLO, vec![OK, OK, OK]);
        let mut conn = Conn::open(port).expect("the hello arrives");
        for _ in 0..3 {
            conn.call(json!({"op": "ping"}))
                .map_err(CallFailure::into_error)
                .expect("the fake answers every ping");
        }

        let ids: Vec<i64> = seen
            .lock()
            .expect("seen lock")
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_i64))
            .collect();
        assert_eq!(ids, [1, 2, 3]);
    }
}
