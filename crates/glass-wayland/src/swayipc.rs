//! Minimal i3/sway IPC client: enough to read the window tree and run commands
//! (e.g. focus a container). Wire format: magic "i3-ipc", u32 LE payload length,
//! u32 LE message type, then a JSON payload.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use glass_core::{Deadline, GlassError, Result, Whose};
use serde::Deserialize;

const MAGIC: &[u8; 6] = b"i3-ipc";
const MSG_RUN_COMMAND: u32 = 0;
const MSG_GET_OUTPUTS: u32 = 3;
const MSG_GET_TREE: u32 = 4;

/// One node from `get_tree` we care about (recursive).
#[derive(Debug, Deserialize)]
pub struct Node {
    pub id: i64,
    #[serde(default, rename = "type")]
    pub node_type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub rect: Rect,
    #[serde(default)]
    pub foreign_toplevel_identifier: Option<String>,
    /// The X11 window id, for a view sway manages through Xwayland (absent for native
    /// Wayland views). sway names this field `window`, as i3 does.
    #[serde(default, rename = "window")]
    pub x11_window: Option<u32>,
    #[serde(default)]
    pub window_properties: Option<WindowProperties>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub floating_nodes: Vec<Node>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn intersects(&self, other: &Self) -> bool {
        [
            (self.x, self.width, other.x, other.width),
            (self.y, self.height, other.y, other.height),
        ]
        .into_iter()
        .all(|(start, extent, other_start, other_extent)| {
            let end = i64::from(start) + i64::from(extent);
            let other_end = i64::from(other_start) + i64::from(other_extent);
            i64::from(start.max(other_start)) < end.min(other_end)
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct Output {
    pub name: String,
    pub active: bool,
    pub rect: Rect,
    pub scale: f64,
    pub transform: String,
}

#[derive(Debug, Deserialize)]
pub struct WindowProperties {
    #[serde(default)]
    pub class: Option<String>,
}

/// A flattened app window: any node with a foreign-toplevel identifier.
#[derive(Debug)]
pub struct Window {
    pub con_id: i64,
    pub title: Option<String>,
    pub class: Option<String>,
    pub rect: Rect,
    pub focused: bool,
    pub visible: bool,
    pub pid: Option<i64>,
    pub shell: Option<String>,
    pub output: Option<String>,
    pub identifier: String,
    /// X11 window id when the view came in through Xwayland; `None` for a native Wayland view.
    pub x11_window: Option<u32>,
}

impl Node {
    /// Depth-first collect of windows that have a foreign-toplevel identifier.
    pub fn windows(&self) -> Vec<Window> {
        let mut out = Vec::new();
        self.collect(None, &mut out);
        out
    }
    fn collect(&self, output: Option<&str>, out: &mut Vec<Window>) {
        let output = if self.node_type.as_deref() == Some("output") {
            self.name.as_deref()
        } else {
            output
        };
        if let Some(id) = &self.foreign_toplevel_identifier {
            let class = self.app_id.clone().or_else(|| {
                self.window_properties
                    .as_ref()
                    .and_then(|w| w.class.clone())
            });
            out.push(Window {
                con_id: self.id,
                title: self.name.clone(),
                class,
                rect: Rect {
                    x: self.rect.x,
                    y: self.rect.y,
                    width: self.rect.width,
                    height: self.rect.height,
                },
                focused: self.focused,
                visible: self.visible,
                pid: self.pid,
                shell: self.shell.clone(),
                output: output.map(str::to_owned),
                identifier: id.clone(),
                x11_window: self.x11_window,
            });
        }
        for c in self.nodes.iter().chain(self.floating_nodes.iter()) {
            c.collect(output, out);
        }
    }
}

/// How long a single IPC round trip may take before glass gives up on the compositor.
///
/// sway answers over a unix socket in well under a millisecond, so this is not a performance
/// budget — it is the difference between a wedged compositor erroring out and hanging glass. It
/// matters most during teardown, which the MCP server runs against a deadline it cannot cancel:
/// a blocking read with no timeout there would keep the app, sway and Xwayland alive past the
/// point where glass has given up and exited.
const IPC_TIMEOUT: Duration = Duration::from_millis(500);

/// A connected sway IPC client.
pub struct Ipc {
    sock: UnixStream,
    /// Where to reconnect after a failed request. `None` for a socket handed in directly (tests),
    /// which therefore cannot recover.
    path: Option<PathBuf>,
    /// Set once any I/O on `sock` fails. A failed read may have consumed part of a reply, so the
    /// stream is no longer at a message boundary and everything after it would be parsed from the
    /// wrong offset — including a `run_command` reply that could then read as success. The stream
    /// is replaced rather than resynchronized: a fresh connection starts at a message boundary by
    /// construction.
    broken: bool,
}

/// No socket in the runtime dir at all, as opposed to one that answered badly — doctor tells the
/// two apart when it reports why a compositor never became reachable.
pub(crate) const NO_IPC_SOCKET: &str = "sway IPC socket not found";

impl Ipc {
    /// Find the `sway-ipc.*.sock` in the private runtime dir and connect.
    pub fn connect(runtime_dir: &Path) -> Result<Ipc> {
        let path = find_ipc_socket(runtime_dir)
            .ok_or_else(|| GlassError::Backend(NO_IPC_SOCKET.into()))?;
        let sock = UnixStream::connect(&path)
            .map_err(|e| GlassError::Backend(format!("connect sway IPC: {e}")))?;
        let mut ipc = Ipc::from_stream(sock)?;
        ipc.path = Some(path);
        Ok(ipc)
    }

    /// Query on a disposable socket so an optional scope probe can expire during a reply.
    pub fn observation_by(&self, deadline: Deadline) -> Option<(Vec<Window>, Vec<Output>)> {
        use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

        if deadline.has_passed() {
            return None;
        }
        let address = SocketAddrUnix::new(self.path.as_deref()?).ok()?;
        let socket = net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC.union(SocketFlags::NONBLOCK),
            None,
        )
        .ok()?;
        net::connect(&socket, &address).ok()?;
        let stream = UnixStream::from(socket);
        stream.set_nonblocking(false).ok()?;
        let mut ipc = Self::from_stream(stream).ok()?;
        let windows = ipc.windows_by(deadline).ok()?;
        let reply = ipc
            .request_by(MSG_GET_OUTPUTS, b"", deadline, "sway outputs")
            .ok()?;
        let outputs = serde_json::from_slice(&reply).ok()?;
        Some((windows, outputs))
    }

    /// Replace a desynchronized stream with a fresh one. A timed-out request is not evidence that
    /// the compositor is gone — it may simply have been slow — so the connection is rebuilt on the
    /// next request rather than staying dead for the rest of the session.
    fn reconnect(&mut self) -> Result<()> {
        let path = self.path.as_ref().ok_or_else(|| {
            GlassError::Backend(
                "sway IPC connection is broken (an earlier request failed) and cannot be \
                 reopened"
                    .into(),
            )
        })?;
        let sock = UnixStream::connect(path)
            .map_err(|e| GlassError::Backend(format!("reconnect sway IPC: {e}")))?;
        set_timeouts(&sock)?;
        self.sock = sock;
        self.broken = false;
        Ok(())
    }

    /// Wrap an already-connected socket, applying the read/write timeouts every request relies
    /// on to stay bounded.
    fn from_stream(sock: UnixStream) -> Result<Ipc> {
        set_timeouts(&sock)?;
        Ok(Ipc {
            sock,
            path: None,
            broken: false,
        })
    }

    fn request_by(
        &mut self,
        msg_type: u32,
        payload: &[u8],
        deadline: Deadline,
        operation: &str,
    ) -> Result<Vec<u8>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(operation));
        }
        if self.broken {
            self.reconnect()?;
        }
        let mut buf = Vec::with_capacity(14 + payload.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        buf.extend_from_slice(&msg_type.to_ne_bytes());
        buf.extend_from_slice(payload);
        self.io_by(deadline, |sock| sock.write_all(&buf), "write", operation)?;

        let mut header = [0u8; 14];
        self.read_reply_by(&mut header, deadline, operation)?;
        if &header[0..6] != MAGIC {
            // Also a desync: whatever is in the stream is not a reply glass can resynchronize to.
            self.broken = true;
            return Err(GlassError::Backend("sway IPC bad magic".into()));
        }
        let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
        let mut reply = vec![0u8; len];
        self.read_reply_by(&mut reply, deadline, operation)?;
        if deadline.has_passed() {
            Err(GlassError::caller_deadline_elapsed(operation))
        } else {
            Ok(reply)
        }
    }

    fn read_reply_by(
        &mut self,
        mut remaining: &mut [u8],
        deadline: Deadline,
        operation: &str,
    ) -> Result<()> {
        use std::io::ErrorKind;

        // read_exact can spend the socket timeout again after every partial read.
        while !remaining.is_empty() {
            let received = self.io_by(
                deadline,
                |sock| match sock.read(remaining) {
                    Ok(0) => Err(ErrorKind::UnexpectedEof.into()),
                    Ok(count) => Ok(count),
                    Err(error) => match error.kind() {
                        ErrorKind::Interrupted => Ok(0),
                        _ => Err(error),
                    },
                },
                "read",
                operation,
            )?;
            remaining = &mut remaining[received..];
        }
        Ok(())
    }

    /// Run one socket operation, marking the connection broken if it fails. A timeout counts:
    /// the bytes are still in flight, so the next request would read them as its own reply.
    fn io_by<T>(
        &mut self,
        deadline: Deadline,
        op: impl FnOnce(&mut UnixStream) -> std::io::Result<T>,
        what: &str,
        operation: &str,
    ) -> Result<T> {
        let now = std::time::Instant::now();
        let (timeout, owner) = deadline.budget(IPC_TIMEOUT, now);
        if timeout.is_zero() {
            self.broken = true;
            return Err(GlassError::caller_deadline_elapsed(operation));
        }
        self.sock
            .set_read_timeout(Some(timeout))
            .and_then(|()| self.sock.set_write_timeout(Some(timeout)))
            .map_err(|error| GlassError::Backend(format!("set sway IPC timeout: {error}")))?;
        op(&mut self.sock).map_err(|error| {
            self.broken = true;
            if owner == Whose::Caller
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                )
            {
                GlassError::caller_deadline_elapsed(operation)
            } else {
                GlassError::Backend(format!("sway IPC {what}: {error}"))
            }
        })
    }

    /// `get_tree` -> the app windows (those with a foreign-toplevel identifier).
    pub fn windows(&mut self) -> Result<Vec<Window>> {
        self.windows_by(Deadline::UNBOUNDED)
    }

    pub fn windows_by(&mut self, deadline: Deadline) -> Result<Vec<Window>> {
        let reply = self.request_by(MSG_GET_TREE, b"", deadline, "sway window query")?;
        let root: Node = serde_json::from_slice(&reply)
            .map_err(|e| GlassError::Backend(format!("parse get_tree: {e}")))?;
        let windows = root.windows();
        if deadline.has_passed() {
            Err(GlassError::caller_deadline_elapsed("sway window query"))
        } else {
            Ok(windows)
        }
    }

    /// Run a sway command (e.g. `[con_id=N] focus`). Errors if sway reports the
    /// command failed (no silent fallback).
    pub fn run_command(&mut self, cmd: &str) -> Result<()> {
        self.run_command_by(cmd, Deadline::UNBOUNDED)
    }

    pub fn run_command_by(&mut self, cmd: &str, deadline: Deadline) -> Result<()> {
        let reply = self.request_by(
            MSG_RUN_COMMAND,
            cmd.as_bytes(),
            deadline,
            "sway window command",
        )?;
        let result = check_command_reply(&reply, cmd);
        if deadline.has_passed() {
            Err(GlassError::caller_deadline_elapsed("sway window command"))
        } else {
            result
        }
    }
}

/// Bound every request in both directions, so a compositor that stops answering cannot hang a
/// teardown the MCP server is running against a deadline it cannot cancel.
fn set_timeouts(sock: &UnixStream) -> Result<()> {
    sock.set_read_timeout(Some(IPC_TIMEOUT))
        .and_then(|()| sock.set_write_timeout(Some(IPC_TIMEOUT)))
        .map_err(|e| GlassError::Backend(format!("set sway IPC timeout: {e}")))
}

/// One entry of a `RUN_COMMAND` reply (one per command in the payload).
#[derive(Debug, Deserialize)]
struct CommandOutcome {
    success: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Parse a `RUN_COMMAND` reply (a JSON array of outcomes) and error if any
/// sub-command failed.
fn check_command_reply(reply: &[u8], cmd: &str) -> Result<()> {
    let outcomes: Vec<CommandOutcome> = serde_json::from_slice(reply)
        .map_err(|e| GlassError::Backend(format!("parse run_command reply: {e}")))?;
    for o in outcomes {
        if !o.success {
            return Err(GlassError::Backend(format!(
                "sway command {cmd:?} failed: {}",
                o.error.as_deref().unwrap_or("unknown")
            )));
        }
    }
    Ok(())
}

/// The IPC socket sway creates in `XDG_RUNTIME_DIR`: `sway-ipc.<uid>.<pid>.sock`.
fn find_ipc_socket(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name();
        let n = n.to_string_lossy();
        (n.starts_with("sway-ipc.") && n.ends_with(".sock")).then(|| e.path())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame a reply the way sway does, so a test peer is indistinguishable from the compositor.
    fn frame(msg_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        out.extend_from_slice(&msg_type.to_ne_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A client wired to a peer that answers the next request with `reply`, then goes quiet.
    fn served(msg_type: u32, payload: &[u8]) -> (Ipc, UnixStream) {
        let (ours, mut theirs) = UnixStream::pair().expect("socketpair");
        theirs.write_all(&frame(msg_type, payload)).expect("reply");
        (Ipc::from_stream(ours).expect("wrap"), theirs)
    }

    /// The happy path over a real socket. Also the only thing that catches a `read`/`write` whose
    /// failures are dropped, or a magic check inverted into accepting garbage: both leave the
    /// reply buffer unread, and the parse then fails on bytes that never arrived.
    #[test]
    fn a_well_framed_reply_is_read_off_the_socket_and_parsed() {
        let json = br#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1,"height":1},
          "nodes":[{"id":7,"name":"app","app_id":"app","focused":true,
            "rect":{"x":1,"y":2,"width":3,"height":4},
            "foreign_toplevel_identifier":"ftid"}],"floating_nodes":[]}"#;
        let (mut ipc, _peer) = served(MSG_GET_TREE, json);
        let wins = ipc.windows().expect("a framed reply should parse");
        assert_eq!(wins.len(), 1);
        assert_eq!(wins[0].identifier, "ftid");
    }

    #[test]
    fn an_observation_timeout_leaves_the_session_ipc_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sway-ipc.1.2.sock");
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        let mut session = Ipc::connect(dir.path()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();

        let started = std::time::Instant::now();
        assert!(session.observation_by(Deadline::from_millis(100)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(70));
        listener.set_nonblocking(true).unwrap();
        let (mut optional, _) = listener.accept().unwrap();
        optional
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = [0; 14];
        optional.read_exact(&mut request).unwrap();
        assert_eq!(&request[..], frame(MSG_GET_TREE, b"").as_slice());

        peer.write_all(&frame(MSG_GET_TREE, br#"{"id":1}"#))
            .unwrap();
        assert!(session.windows().unwrap().is_empty());
    }

    /// Anything that is not a reply header is a desync, not a reply to interpret: the bytes in the
    /// stream belong to something else, and reading on would parse the next request's reply from
    /// the wrong offset.
    #[test]
    fn a_reply_with_the_wrong_magic_is_rejected() {
        let (ours, mut theirs) = UnixStream::pair().expect("socketpair");
        let mut bad = frame(MSG_GET_TREE, b"{}");
        bad[0..6].copy_from_slice(b"NOTi3!");
        theirs.write_all(&bad).expect("reply");
        let mut ipc = Ipc::from_stream(ours).expect("wrap");
        let err = ipc.windows().expect_err("garbage must not read as a tree");
        assert!(err.to_string().contains("bad magic"), "{err}");
    }

    #[test]
    fn run_command_reports_success_from_a_real_reply() {
        let (mut ipc, _peer) = served(MSG_RUN_COMMAND, br#"[{"success":true}]"#);
        ipc.run_command("[con_id=1] focus").expect("accepted");
    }

    /// `sway-ipc.<uid>.<pid>.sock` and nothing else in the same directory: the wayland socket, a
    /// half-matching name, and sway's own lock all sit alongside it.
    #[test]
    fn the_ipc_socket_is_picked_out_of_the_runtime_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        for decoy in [
            "wayland-1",
            "wayland-1.lock",
            "sway-ipc.partial",
            "other.sock",
        ] {
            std::fs::write(dir.path().join(decoy), b"").expect("decoy");
        }
        let real = dir.path().join("sway-ipc.1000.42.sock");
        std::fs::write(&real, b"").expect("socket");
        assert_eq!(find_ipc_socket(dir.path()), Some(real));
    }

    #[test]
    fn a_runtime_dir_with_no_ipc_socket_finds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("wayland-1"), b"").expect("decoy");
        assert_eq!(find_ipc_socket(dir.path()), None);
    }

    #[test]
    fn parses_get_tree_and_flattens_app_windows() {
        let json = r#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1280,"height":720},
          "nodes":[{"id":2,"name":"HEADLESS-1","rect":{"x":0,"y":0,"width":1280,"height":720},
            "nodes":[{"id":7,"name":"glass-testapp-1","focused":true,
              "rect":{"x":480,"y":240,"width":320,"height":240},
              "foreign_toplevel_identifier":"abc123",
              "window_properties":{"class":"glass-testapp"}}],
            "floating_nodes":[]}],
          "floating_nodes":[]}"#;
        let root: Node = serde_json::from_str(json).unwrap();
        let wins = root.windows();
        assert_eq!(wins.len(), 1);
        let w = &wins[0];
        assert_eq!(w.con_id, 7);
        assert_eq!(w.title.as_deref(), Some("glass-testapp-1"));
        assert_eq!(w.class.as_deref(), Some("glass-testapp"));
        assert_eq!(
            (w.rect.x, w.rect.y, w.rect.width, w.rect.height),
            (480, 240, 320, 240)
        );
        assert!(w.focused);
        assert_eq!(w.identifier, "abc123");
    }

    /// An Xwayland view carries the X11 window id sway manages it under. Recovering a toplevel
    /// the compositor never surfaced needs that id to tell "sway already has this window" from
    /// "this window is missing", so it must survive the flattening.
    #[test]
    fn keeps_the_x11_window_id_of_an_xwayland_view() {
        let json = r#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1,"height":1},
          "nodes":[{"id":7,"name":"glass-testapp","window":4194304,"shell":"xwayland",
            "rect":{"x":0,"y":0,"width":320,"height":240},
            "foreign_toplevel_identifier":"abc123"}],
          "floating_nodes":[]}"#;
        let root: Node = serde_json::from_str(json).unwrap();
        assert_eq!(root.windows()[0].x11_window, Some(4_194_304));
    }

    /// A native Wayland view has no X11 window id; it must read as absent rather than as some
    /// placeholder id that could collide with a real X window.
    #[test]
    fn native_wayland_view_has_no_x11_window_id() {
        let json = r#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1,"height":1},
          "nodes":[{"id":7,"name":"app","app_id":"app","shell":"xdg_shell",
            "rect":{"x":0,"y":0,"width":320,"height":240},
            "foreign_toplevel_identifier":"abc123"}],
          "floating_nodes":[]}"#;
        let root: Node = serde_json::from_str(json).unwrap();
        assert_eq!(root.windows()[0].x11_window, None);
    }

    #[test]
    fn ignores_nodes_without_foreign_toplevel_identifier() {
        let json = r#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1,"height":1},"nodes":[],"floating_nodes":[]}"#;
        let root: Node = serde_json::from_str(json).unwrap();
        assert!(root.windows().is_empty());
    }

    #[test]
    fn command_reply_ok_when_all_succeed() {
        assert!(check_command_reply(br#"[{"success":true}]"#, "[con_id=1] focus").is_ok());
    }

    /// A compositor that accepts the request and never answers must not hang glass. Teardown runs
    /// against a deadline the MCP server cannot cancel, so a blocking read here would leave sway,
    /// Xwayland and the app alive past the point glass gave up and exited.
    #[test]
    fn a_compositor_that_never_answers_times_out_instead_of_hanging() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut ipc = Ipc::from_stream(ours).expect("wrap");
            let _ = tx.send(ipc.windows().err());
        });
        // Bounded from outside the request, on purpose. An unbounded read does not take too long,
        // it never returns — so a `t.elapsed()` assertion written after the call is never reached,
        // and the whole suite hangs instead of reporting the missing timeout.
        let outcome = rx
            .recv_timeout(IPC_TIMEOUT * 4)
            .expect("the request should give up on its own rather than block forever");
        let err = outcome.expect("a silent peer must not produce a window list");
        assert!(err.to_string().contains("sway IPC"), "{err}");
        drop(theirs); // held open until now: an EOF would end the read without a timeout
    }

    #[test]
    fn a_live_window_list_read_uses_the_callers_remaining_deadline() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let mut ipc = Ipc::from_stream(ours).expect("wrap");
        let started = std::time::Instant::now();
        let deadline = Deadline::at(started + Duration::from_millis(20));

        let err = ipc
            .windows_by(deadline)
            .expect_err("a silent compositor must consume only the caller's remaining budget");

        assert_eq!(err.bound_owner(), Some(Whose::Caller));
        assert_eq!(err.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(
            err.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(theirs);
    }

    #[test]
    fn partial_sway_headers_and_bodies_share_the_callers_deadline() {
        for complete_header in [false, true] {
            let (ours, mut peer) = UnixStream::pair().unwrap();
            let mut ipc = Ipc::from_stream(ours).unwrap();
            let writer = std::thread::spawn(move || {
                let mut request = [0; 14];
                if peer.read_exact(&mut request).is_err() {
                    return;
                }
                let reply = frame(MSG_GET_TREE, br#"{"id":1,"floating_nodes":[]}"#);
                let (prefix, chunks) = reply.split_at(if complete_header { 14 } else { 0 });
                if peer.write_all(prefix).is_err() {
                    return;
                }
                for chunk in chunks.chunks(1) {
                    if peer.write_all(chunk).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(45));
                }
            });

            let started = std::time::Instant::now();
            let error = ipc.windows_by(Deadline::from_millis(150)).unwrap_err();
            let elapsed = started.elapsed();
            let broken = ipc.broken;
            drop(ipc);
            writer.join().unwrap();
            assert_eq!(error.bound_owner(), Some(Whose::Caller));
            assert!(broken, "the incomplete reply must retire the connection");
            assert!(
                elapsed < Duration::from_millis(400),
                "reply took {elapsed:?}"
            );
        }
    }

    /// After a failed read the stream is no longer at a message boundary: the reply glass gave up
    /// on can still arrive and would be read as the *next* request's reply. A `run_command` that
    /// picked up a stale `{"success":true}` would report a close request as delivered when it was
    /// not, which is exactly the misreport teardown is supposed to avoid. The live client answers
    /// this by reconnecting; this socket has no path to reconnect to, so it reports the failure
    /// instead of reading the stale bytes.
    #[test]
    fn a_broken_connection_refuses_further_requests_instead_of_reading_a_stale_reply() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let mut ipc = Ipc::from_stream(ours).expect("wrap");
        assert!(ipc.windows().is_err(), "first request times out");
        // The peer answers late, exactly as a recovering compositor would.
        let mut theirs = theirs;
        let payload = br#"[{"success":true}]"#;
        let mut late = Vec::new();
        late.extend_from_slice(MAGIC);
        late.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        late.extend_from_slice(&MSG_RUN_COMMAND.to_ne_bytes());
        late.extend_from_slice(payload);
        theirs.write_all(&late).expect("late reply");
        let err = ipc
            .run_command("[con_id=1] kill")
            .expect_err("a broken connection must not report success from a stale reply");
        assert!(err.to_string().contains("cannot be"), "{err}");
    }

    #[test]
    fn command_reply_errors_with_sways_message() {
        let reply = br#"[{"success":false,"error":"No matching node."}]"#;
        let err =
            check_command_reply(reply, "[con_id=9] resize set width 1 px height 1 px").unwrap_err();
        assert!(matches!(err, GlassError::Backend(_)));
        assert!(err.to_string().contains("No matching node."), "{err}");
    }
}
