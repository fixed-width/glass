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
        let to = Duration::from_secs(30);
        stream.set_read_timeout(Some(to)).ok();
        stream.set_write_timeout(Some(to)).ok();
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
