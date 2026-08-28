//! Shared line-delimited-JSON TCP connection used by the agent client and the
//! a11y-service client. `Conn` opens a TCP socket, wraps it in a `BufReader` for
//! line reads, and exposes a `call` method that writes one JSON request line and
//! reads one JSON response line.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use glass_core::{Deadline, GlassError};
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
pub(crate) const STANDING_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeoutFault {
    ReadInstall,
    WriteInstall,
    ReadRestore,
    WriteRestore,
}

pub(crate) struct Conn {
    pub(crate) writer: TcpStream,
    pub(crate) reader: BufReader<TcpStream>,
    pub(crate) next_id: i64,
    poisoned: bool,
    #[cfg(test)]
    timeout_faults: std::collections::VecDeque<TimeoutFault>,
}

impl Conn {
    /// Connect to `127.0.0.1:port`, read + version-check the hello banner.
    pub(crate) fn open(port: u16) -> glass_core::Result<Conn> {
        Self::open_by(port, Deadline::UNBOUNDED)
    }

    /// Connect and complete the hello handshake within one caller deadline.
    pub(crate) fn open_by(port: u16, deadline: Deadline) -> glass_core::Result<Conn> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("agent connect"));
        }
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let stream_result = match deadline.remaining() {
            Some(wait) => TcpStream::connect_timeout(&address, wait),
            None => TcpStream::connect(address),
        };
        let stream = stream_result.map_err(|e| {
            if deadline.has_passed() {
                GlassError::caller_deadline_elapsed("agent connect")
            } else {
                GlassError::Backend(format!("agent connect :{port}: {e}"))
            }
        })?;
        // Timeouts so a stalled agent surfaces as a transport error the reconnect path handles,
        // rather than hanging the MCP thread forever. Each goes on the handle that does that
        // half's work — see `read_within` for what puts the read one on the reader.
        let setup_wait = deadline.remaining();
        if setup_wait.is_some_and(|wait| wait.is_zero()) {
            return Err(GlassError::caller_deadline_elapsed("agent connect"));
        }
        stream
            .set_write_timeout(Some(setup_wait.unwrap_or(STANDING_TIMEOUT)))
            .map_err(|e| GlassError::Backend(format!("agent write timeout install: {e}")))?;
        let read_half = stream
            .try_clone()
            .map_err(|e| GlassError::Backend(format!("agent clone: {e}")))?;
        read_half
            .set_read_timeout(Some(setup_wait.unwrap_or(STANDING_TIMEOUT)))
            .map_err(|e| GlassError::Backend(format!("agent read timeout install: {e}")))?;
        let reader = BufReader::new(read_half);
        let mut c = Conn {
            writer: stream,
            reader,
            next_id: 1,
            poisoned: false,
            #[cfg(test)]
            timeout_faults: Default::default(),
        };
        let hello = c.read_line().map_err(|e| {
            if deadline.has_passed() {
                GlassError::caller_deadline_elapsed("agent hello")
            } else {
                e
            }
        })?;
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed("agent hello"));
        }
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
        c.restore_timeouts()?;
        Ok(c)
    }

    pub(crate) fn ensure_usable(&self) -> glass_core::Result<()> {
        if self.poisoned {
            Err(GlassError::Backend(
                "agent connection is unusable after socket timeout restoration failed".into(),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
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
    ///
    /// Do not set this on `writer`: a read timeout does not carry across `try_clone` on Windows,
    /// where a bound set there left the read on the 30s standing one instead.
    pub(crate) fn read_within(&mut self, wait: Option<Duration>) -> glass_core::Result<()> {
        self.read_within_for(wait, false)
    }

    fn read_within_for(
        &mut self,
        wait: Option<Duration>,
        _restoring: bool,
    ) -> glass_core::Result<()> {
        self.ensure_usable()?;
        #[cfg(test)]
        self.maybe_timeout_fault(if _restoring {
            TimeoutFault::ReadRestore
        } else {
            TimeoutFault::ReadInstall
        })?;
        self.reader
            .get_ref()
            .set_read_timeout(Some(wait.unwrap_or(STANDING_TIMEOUT)))
            .map_err(|e| GlassError::Backend(format!("agent read timeout update: {e}")))
    }

    pub(crate) fn write_within(&mut self, wait: Option<Duration>) -> glass_core::Result<()> {
        self.write_within_for(wait, false)
    }

    fn write_within_for(
        &mut self,
        wait: Option<Duration>,
        _restoring: bool,
    ) -> glass_core::Result<()> {
        self.ensure_usable()?;
        #[cfg(test)]
        self.maybe_timeout_fault(if _restoring {
            TimeoutFault::WriteRestore
        } else {
            TimeoutFault::WriteInstall
        })?;
        self.writer
            .set_write_timeout(Some(wait.unwrap_or(STANDING_TIMEOUT)))
            .map_err(|e| GlassError::Backend(format!("agent write timeout update: {e}")))
    }

    #[cfg(test)]
    pub(crate) fn inject_timeout_fault(&mut self, fault: TimeoutFault) {
        self.timeout_faults.push_back(fault);
    }

    #[cfg(test)]
    fn maybe_timeout_fault(&mut self, fault: TimeoutFault) -> glass_core::Result<()> {
        if self.timeout_faults.front() == Some(&fault) {
            self.timeout_faults.pop_front();
            Err(GlassError::Backend(format!(
                "injected {fault:?} socket timeout failure"
            )))
        } else {
            Ok(())
        }
    }

    fn restore_timeouts(&mut self) -> glass_core::Result<()> {
        let read = self.read_within_for(None, true);
        let write = self.write_within_for(None, true);
        match (read, write) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Err(read), Err(write)) => Err(GlassError::Backend(format!(
                "socket timeout restoration failed twice: {read}; {write}"
            ))),
        }
    }

    /// Run one request under `deadline`. The outer error is a pre-dispatch setup failure; the
    /// inner result retains how far a dispatched request got.
    pub(crate) fn call_within(
        &mut self,
        req: Value,
        deadline: Deadline,
        op: &str,
    ) -> glass_core::Result<std::result::Result<Value, CallFailure>> {
        self.ensure_usable()?;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(op));
        }

        let wait = deadline.remaining();
        let install = self
            .read_within(wait)
            .and_then(|()| self.write_within(wait));
        if let Err(install_error) = install {
            if let Err(restore_error) = self.restore_timeouts() {
                self.poison();
                return Err(restore_error);
            }
            return Err(install_error);
        }

        let outcome = if deadline.has_passed() {
            Err(CallFailure::NotSent(GlassError::deadline_not_started(op)))
        } else {
            let outcome = self.call(req);
            if deadline.has_passed() {
                Err(match outcome {
                    Ok(_) => CallFailure::Refused(GlassError::caller_deadline_elapsed(op)),
                    Err(failure) => failure.with_error(GlassError::caller_deadline_elapsed(op)),
                })
            } else {
                outcome
            }
        };

        match self.restore_timeouts() {
            Ok(()) => Ok(outcome),
            Err(error) => {
                self.poison();
                Ok(Err(match outcome {
                    Ok(_) => CallFailure::Refused(error),
                    Err(failure) => failure.with_error(error),
                }))
            }
        }
    }

    /// Send one request object (an `id` is injected) and return the response `Value`.
    /// A failure is classified by how far the request got — see [`CallFailure`].
    pub(crate) fn call(&mut self, mut req: Value) -> std::result::Result<Value, CallFailure> {
        self.ensure_usable().map_err(CallFailure::NotSent)?;
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
    use glass_core::Deadline;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    const HELLO: &str = r#"{"hello":{"proto":1}}"#;
    const OK: &str = r#"{"ok":true}"#;

    fn delayed_hello(delay: Duration) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            std::thread::sleep(delay);
            let _ = writeln!(stream, "{HELLO}");
        });
        port
    }

    #[test]
    fn bounded_connection_setup_times_out_near_the_caller_deadline() {
        let started = Instant::now();
        let Err(err) = Conn::open_by(
            delayed_hello(Duration::from_secs(2)),
            Deadline::from_millis(150),
        ) else {
            panic!("a delayed hello must exceed the caller deadline");
        };
        assert!(matches!(err, GlassError::Bounded { .. }), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn successful_bounded_connection_setup_restores_standing_timeouts() {
        let conn = Conn::open_by(
            delayed_hello(Duration::from_millis(20)),
            Deadline::from_millis(500),
        )
        .unwrap();
        assert_eq!(conn.writer.write_timeout().unwrap(), Some(STANDING_TIMEOUT));
        assert_eq!(
            conn.reader.get_ref().read_timeout().unwrap(),
            Some(STANDING_TIMEOUT)
        );
    }

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
        conn.read_within(Some(Duration::from_millis(200)))
            .expect("install the bounded read timeout");

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
    fn an_answer_addressed_to_another_request_is_refused_rather_than_retried() {
        // The id is what matches an answer to its question, and a mismatch must classify as
        // `Refused`: `is_transport` is what decides whether the caller re-sends, and re-sending
        // an `ACTION_CLICK` whose answer merely went astray taps the control a second time.
        let (port, _seen) = fake_agent(HELLO, vec![r#"{"id":999,"ok":true}"#]);
        let mut conn = Conn::open(port).expect("the hello arrives");

        let Err(failure) = conn.call(json!({"op": "ping"})) else {
            panic!("an answer to a different request is not an answer to this one");
        };
        assert!(
            !failure.is_transport(),
            "a device that answered is not a transport failure, and must not be re-sent to"
        );
        assert!(!failure.nothing_sent());
        assert!(
            failure.into_error().to_string().contains("id mismatch"),
            "the error must say what did not line up"
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
