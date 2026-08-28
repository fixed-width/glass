use std::cell::Cell;
use std::os::fd::AsFd;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, BoundDispatch, Deadline, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region,
    Result, Stream, TEARDOWN_BUDGET, Whose, WindowGeometry, WindowHint, WindowId, WindowInfo,
    WindowOp,
};
use glass_pipe_unix::LineTap;
use glass_proc_linux::{APP_REAP_GRACE, Asked, CLOSE_GRACE, proc_tree_pids};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

// glass-mcp gives the whole of teardown `glass_core::TEARDOWN_BUDGET` and then exits regardless, on a
// `spawn_blocking` thread that cannot be cancelled — so an ask-then-signal ladder that fills the
// budget would never get to the signal, and the app would outlive glass.
//
// This binds the ladder only. What follows it in the same budget — the private a11y bus, then a
// private Xvfb — still reaps at `REAP_GRACE` each, so a helper that ignores SIGTERM can take the
// whole teardown past the budget; that is pre-existing and not what this assertion is about.
const _: () = assert!(
    ASK_BUDGET.as_millis() + CLOSE_GRACE.as_millis() + APP_REAP_GRACE.as_millis()
        < TEARDOWN_BUDGET.as_millis(),
    "sending the close request, waiting it out and the signal ladder must all finish inside \
     glass_core::TEARDOWN_BUDGET"
);

/// How long the close request itself may take before glass gives up on the display and goes
/// straight to signalling. Sending it is a handful of X round trips — sub-millisecond on a
/// healthy server — so this is a liveness bound, not a budget the ask is expected to use.
const ASK_BUDGET: Duration = Duration::from_millis(400);

const XT_MOTION: u8 = 6; // MotionNotify
const XT_BTN_PRESS: u8 = 4; // ButtonPress
const XT_BTN_RELEASE: u8 = 5; // ButtonRelease
const XT_KEY_PRESS: u8 = 2; // KeyPress
const XT_KEY_RELEASE: u8 = 3; // KeyRelease

#[derive(Default)]
struct X11Dispatch(Cell<bool>);

impl X11Dispatch {
    fn mark(&self) {
        self.0.set(true);
    }

    fn deadline_error(&self, op: &str) -> GlassError {
        if self.0.get() {
            GlassError::caller_deadline_elapsed(op)
        } else {
            GlassError::deadline_not_started(op)
        }
    }

    fn classify(&self, op: &str, error: GlassError) -> GlassError {
        if self.0.get()
            && error.bound_owner() == Some(Whose::Caller)
            && error.bound_dispatch() == Some(BoundDispatch::NotDispatched)
        {
            GlassError::caller_deadline_elapsed(op)
        } else {
            error
        }
    }
}

fn run_x11_call_by<T>(
    deadline: Deadline,
    op: &str,
    call: impl FnOnce(&X11Dispatch) -> Result<T>,
) -> Result<T> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started(op));
    }
    let dispatch = X11Dispatch::default();
    let answer = call(&dispatch).map_err(|error| dispatch.classify(op, error))?;
    if deadline.has_passed() {
        return Err(dispatch.deadline_error(op));
    }
    Ok(answer)
}

fn run_x11_type_by<S: glass_core::TypeSink>(
    sink: &mut S,
    text: &str,
    dwell: Duration,
    deadline: Deadline,
) -> Result<()> {
    glass_core::run_type_by(sink, text, dwell, deadline)
}

fn run_clicks_by(
    count: u32,
    mut deadline_passed: impl FnMut() -> bool,
    mut button: impl FnMut(bool) -> Result<()>,
    cleanup: impl FnOnce(bool, bool) -> Result<()>,
) -> Result<()> {
    let mut button_down = false;
    let outcome = (|| {
        for _ in 0..count.max(1) {
            if deadline_passed() {
                return Err(GlassError::deadline_not_started("pointer input"));
            }
            button(true)?;
            button_down = true;
            if deadline_passed() {
                return Err(GlassError::deadline_not_started("pointer input"));
            }
            button(false)?;
            button_down = false;
        }
        Ok(())
    })();
    cleanup(button_down, outcome.is_err())?;
    outcome
}

fn run_scroll_buttons_by(
    pos_btn: u8,
    neg_btn: u8,
    delta: i32,
    mut deadline_passed: impl FnMut() -> bool,
    mut button: impl FnMut(bool, u8) -> Result<()>,
) -> Result<()> {
    let (btn, times) = if delta >= 0 {
        (pos_btn, delta)
    } else {
        (neg_btn, -delta)
    };
    for _ in 0..times {
        if deadline_passed() {
            return Err(GlassError::deadline_not_started("pointer input"));
        }
        button(true, btn)?;
        button(false, btn)?;
    }
    Ok(())
}

use crate::command::build_command;

type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// The Linux/X11 backend. Connects to an X display, launches and locates the
/// target app's top-level window, and drives it via X requests + XTEST.
pub struct X11Platform {
    conn: RustConnection,
    /// The X screen glass connected to; indexes `setup().roots` for the display
    /// (root window) size, used to reject captures that reach off-screen.
    screen_num: usize,
    root: Window,
    display: String,
    child: Option<Child>,
    window: Option<Window>,
    logs: LogSink,
    /// The launched app's stdout/stderr readers, dropped in `kill_child`. The app's write ends
    /// are inherited by everything it spawns, so an EOF-only reader parks on a survivor's pipe
    /// (glass#477).
    taps: Vec<LineTap>,
    // A private Xvfb we spawned (default path); kept alive so Drop tears it down.
    xvfb: Option<crate::xvfb::Xvfb>,
    // A private a11y-enabled D-Bus session bus we spawned for the launched app;
    // kept alive so Drop tears it down. Set on each a11y-enabled launch (any sandbox level).
    dbus: Option<glass_dbus_linux::PrivateBus>,
    // Background thread that owns the CLIPBOARD selection and serves pastes.
    clipboard_owner: Option<crate::clipboard::ClipboardOwner>,
}

/// What display the X11 backend should use, derived from `GLASS_DISPLAY`.
#[derive(Debug, PartialEq, Eq)]
enum DisplayTarget {
    /// Attach to an explicit display, e.g. `:0` (real desktop) or `:42`.
    Attach(String),
    /// None given — spawn a private headless Xvfb.
    Spawn,
}

/// Decide from the `GLASS_DISPLAY` value. Blank/unset spawns; ambient `$DISPLAY`
/// is intentionally never consulted.
fn display_target(glass_display: Option<&str>) -> DisplayTarget {
    match glass_display.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => DisplayTarget::Attach(normalize_display(d)),
        None => DisplayTarget::Spawn,
    }
}

/// Accept both `:42` and bare `42`.
pub(crate) fn normalize_display(d: &str) -> String {
    if d.starts_with(':') {
        d.to_string()
    } else {
        format!(":{d}")
    }
}

/// True when an X11 reply error means the target window's resource no longer
/// exists: `BadWindow` (the id is not a window) or `BadDrawable` (the id is not
/// a drawable — what `GetGeometry`/`TranslateCoordinates` report for a destroyed
/// window). Other protocol/connection errors are genuine backend failures.
fn is_window_gone(err: &ReplyError) -> bool {
    matches!(
        err,
        ReplyError::X11Error(x) if matches!(x.error_kind, ErrorKind::Window | ErrorKind::Drawable)
    )
}

/// Start a reader over one of the launch's streams, or reap the launch and say why it could not.
///
/// The failed start consumed the pipe, so the app's next write to that stream takes `SIGPIPE` — a
/// death mid-run with nothing in the logs to say why.
///
/// The group, not the leader: `command.rs` spawns the app with `process_group(0)`, and under a
/// sandbox the leader is `bwrap` with the real app as its child — a leader-only reap leaves that
/// child holding the write end of the very pipe this could not read. (`xvfb.rs` can reap the
/// leader because the X server is not spawned into its own group.)
fn tap_or_reap<R: std::io::Read + AsFd + Send + 'static>(
    stream: R,
    tag: Stream,
    name: &str,
    logs: &LogSink,
    child: &mut Child,
    spec: &AppSpec,
) -> Result<LineTap> {
    LineTap::start(stream, tag, name, logs.clone()).map_err(|e| {
        glass_proc_linux::reap_group(child, glass_proc_linux::REAP_GRACE);
        GlassError::AppNotStarted(format!(
            "started {:?} but could not read its output ({e}); the app was stopped rather than \
             left to write into a pipe nobody drains — free up threads and file descriptors on \
             the host",
            spec.run
        ))
    })
}

impl X11Platform {
    /// Connect using `$DISPLAY`.
    pub fn new() -> Result<Self> {
        Self::connect(None)
    }

    /// Build from the environment: attach to `GLASS_DISPLAY` if set, else spawn a
    /// private headless Xvfb. Never consults ambient `$DISPLAY`, so the launch
    /// environment can't accidentally point glass at the real desktop (`:0`).
    pub fn from_env() -> Result<Self> {
        match display_target(std::env::var("GLASS_DISPLAY").ok().as_deref()) {
            DisplayTarget::Attach(d) => Self::connect(Some(&d)),
            DisplayTarget::Spawn => {
                let screen =
                    std::env::var("GLASS_XVFB_SCREEN").unwrap_or_else(|_| "1280x800x24".into());
                let xvfb = crate::xvfb::Xvfb::start(&screen)?;
                // stderr (stdout is the MCP channel); lets the user watch via VNC.
                eprintln!(
                    "glass: spawned a private headless X11 display {} \
                     (set GLASS_DISPLAY to attach to your own)",
                    xvfb.display
                );
                let mut p = Self::connect(Some(&xvfb.display))?;
                p.xvfb = Some(xvfb);
                Ok(p)
            }
        }
    }

    /// Connect to a specific display (e.g. `Some(":99")`), or `$DISPLAY` if `None`.
    pub fn connect(display: Option<&str>) -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(display).map_err(|e| GlassError::Backend(format!("X connect: {e}")))?;
        let root = conn.setup().roots[screen_num].root;
        let display = display
            .map(|s| s.to_string())
            .or_else(|| std::env::var("DISPLAY").ok())
            .unwrap_or_else(|| ":0".to_string());
        Ok(Self {
            conn,
            screen_num,
            root,
            display,
            child: None,
            window: None,
            logs: Arc::new(Mutex::new(Vec::new())),
            taps: Vec::new(),
            xvfb: None,
            dbus: None,
            clipboard_owner: None,
        })
    }

    fn require_window(&self) -> Result<Window> {
        self.window.ok_or(GlassError::WindowNotFound)
    }

    /// The active window's resource is gone (it was closed/destroyed and the X
    /// server rejected an op against its id). Forget the stale id so the next op
    /// reports the friendly `WindowNotFound` rather than another raw protocol
    /// error, and return that error.
    fn note_window_gone(&mut self) -> GlassError {
        self.window = None;
        GlassError::WindowNotFound
    }

    /// Configure the active window and `.check()` the request so the server's
    /// (asynchronous) reply is observed here: a closed window yields
    /// `BadWindow`/`BadDrawable`, which we translate into `WindowNotFound` after
    /// forgetting the stale id. `label` names the op for genuine backend errors.
    fn configure_active(
        &mut self,
        win: Window,
        aux: &ConfigureWindowAux,
        label: &str,
    ) -> Result<()> {
        let cookie = self
            .conn
            .configure_window(win, aux)
            .map_err(|e| GlassError::Backend(format!("{label}: {e}")))?;
        cookie.check().map_err(|e| {
            if is_window_gone(&e) {
                self.note_window_gone()
            } else {
                GlassError::Backend(format!("{label}: {e}"))
            }
        })
    }

    /// Absolute geometry of the active target window (origin in root coords).
    /// If the active window has been closed, the X server's `BadWindow`/
    /// `BadDrawable` is translated into `WindowNotFound` and the stale id is
    /// forgotten (so the next op reports the same friendly error, not a fresh
    /// raw one).
    pub(crate) fn window_geometry(&mut self) -> Result<WindowGeometry> {
        let win = self.require_window()?;
        self.geometry_of_raw(win).map_err(|e| {
            if is_window_gone(&e) {
                self.note_window_gone()
            } else {
                GlassError::Backend(format!("get_geometry reply: {e}"))
            }
        })
    }

    /// Absolute geometry of a specific window (origin in root coordinates). Used
    /// for arbitrary (non-active) windows during enumeration, where a stale id
    /// is just a backend error rather than a reason to clear the active window.
    fn geometry_of(&self, win: Window) -> Result<WindowGeometry> {
        self.geometry_of_raw(win)
            .map_err(|e| GlassError::Backend(format!("get_geometry reply: {e}")))
    }

    /// Read a window's absolute geometry, preserving the typed X11 error so
    /// callers can distinguish "window gone" from other backend failures.
    fn geometry_of_raw(&self, win: Window) -> std::result::Result<WindowGeometry, ReplyError> {
        let geo = self.conn.get_geometry(win)?.reply()?;
        let abs = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)?
            .reply()?;
        Ok(WindowGeometry {
            x: abs.dst_x as i32,
            y: abs.dst_y as i32,
            width: geo.width as u32,
            height: geo.height as u32,
        })
    }

    /// Send everything buffered so far, without waiting for a reply. The input sinks commit
    /// each step this way: a client that renders per frame sees a gesture unfold, where one
    /// flush at the end delivers it as a single jump.
    fn commit(&self) -> Result<()> {
        self.conn
            .flush()
            .map_err(|e| GlassError::Backend(format!("flush: {e}")))
    }

    /// Intern an atom by name (small helper for the multi-window scans).
    fn intern(&self, name: &[u8]) -> Result<Atom> {
        Ok(self
            .conn
            .intern_atom(false, name)
            .map_err(|e| {
                GlassError::Backend(format!("intern {}: {e}", String::from_utf8_lossy(name)))
            })?
            .reply()
            .map_err(|e| GlassError::Backend(format!("intern reply: {e}")))?
            .atom)
    }

    /// Every mapped top-level window matching the app's PID set (the `WindowHint`
    /// is a startup disambiguator, not a list filter). Dedups `_NET_CLIENT_LIST` ∪
    /// root children, mirroring `scan_for_window`.
    fn scan_all_windows(&self, pids: &[u32]) -> Result<Vec<Window>> {
        let pid_atom = self.intern(b"_NET_WM_PID")?;
        let client_list_atom = self.intern(b"_NET_CLIENT_LIST")?;
        let root_children = self
            .conn
            .query_tree(self.root)
            .map_err(|e| GlassError::Backend(format!("query_tree: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("query_tree reply: {e}")))?
            .children;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for win in self
            .client_list_windows(client_list_atom)
            .into_iter()
            .chain(root_children)
        {
            if !seen.insert(win) {
                continue;
            }
            if self.window_matches(win, pids, pid_atom, None)? {
                out.push(win);
            }
        }
        Ok(out)
    }

    fn spawn(&mut self, spec: &AppSpec) -> Result<()> {
        // `start_app` sets `self.dbus` before calling `spawn`, so reading it here
        // injects the private session-bus address into the launched app's env.
        // For sandboxed launches, also bind the private bus dir into bwrap so the
        // sandboxed app can reach the advertised unix:path= sockets.
        let a11y = self.dbus.as_ref().map(|b| glass_core::A11yBind {
            addr: b.session_bus_address(),
            dir: b.runtime_dir(),
        });
        let mut cmd = build_command(spec, &self.display, a11y);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| GlassError::AppNotStarted(format!("spawn {:?}: {e}", spec.run)))?;
        // `Stdio::piped()` above guarantees both are `Some`. Skipping one that is not would
        // silently stop capturing it, the fallback this crate's contract forbids.
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        self.taps = vec![
            tap_or_reap(
                stdout,
                Stream::Stdout,
                "glass-app-stdout",
                &self.logs,
                &mut child,
                spec,
            )?,
            tap_or_reap(
                stderr,
                Stream::Stderr,
                "glass-app-stderr",
                &self.logs,
                &mut child,
                spec,
            )?,
        ];
        self.child = Some(child);
        Ok(())
    }

    /// Ask every mapped top-level window of the app rooted at `root_pid` to close.
    ///
    /// The ask is a `WM_DELETE_WINDOW` client message — the same one a window manager sends
    /// when the user clicks the close button, and the only way a toolkit app gets to run its
    /// shutdown path (see `glass_proc_linux::CLOSE_GRACE` for what a signal does instead).
    /// Nothing restricts who sends it, whether or not a window manager is running: `wmctrl -c`
    /// does the same thing.
    ///
    /// Only windows that advertise `WM_DELETE_WINDOW` in `WM_PROTOCOLS` are asked. A window
    /// that does not has no handler for the message and will not act on it, so counting it
    /// would make teardown wait out the whole grace for a shutdown nobody was asked to perform.
    ///
    /// Every step here is a blocking X round trip, and x11rb has no per-request timeout: an X
    /// server that is alive but not answering stalls teardown for as long as it stays that way.
    /// That is true of every X call glass makes (capture and input included), not something this
    /// path introduces a way around — bounding it needs a cancellable X path, not a socket
    /// timeout, which would desynchronize the protocol stream on a spurious trip.
    ///
    /// The set is the launched process tree's, never the window-hint's. A hint can match a
    /// window belonging to some *other* process — that is the point of the fallback, and it is
    /// safe for discovery, which only reads — but closing one would destroy the state of an app
    /// glass did not launch, which teardown of a spawned child has never done. A launch found
    /// only by hint is therefore signalled, and says so rather than passing for an app with no
    /// window.
    fn request_close(&self, root_pid: u32) -> Asked {
        let (protocols, delete) = match (
            self.intern(b"WM_PROTOCOLS"),
            self.intern(b"WM_DELETE_WINDOW"),
        ) {
            (Ok(p), Ok(d)) => (p, d),
            (Err(e), _) | (_, Err(e)) => {
                return Asked::blocked(format!("glass could not reach the X server: {e}"));
            }
        };
        // `scan_all_windows` is the same enumeration `list_windows` uses, so the set asked to
        // close is exactly the set glass reports as the app's windows — mapped top-levels whose
        // `_NET_WM_PID` is in the launched process tree.
        let wins = match self.scan_all_windows(&proc_tree_pids(root_pid)) {
            Ok(wins) => wins,
            Err(e) => return Asked::blocked(format!("glass could not enumerate its windows: {e}")),
        };
        if wins.is_empty() {
            // No window in the launched process tree. If glass is nonetheless driving one, it
            // was matched by the window hint and belongs to a process outside that tree, which
            // teardown deliberately leaves alone (see above) — worth saying, because the app on
            // screen is about to be signalled rather than asked.
            return match self.window {
                Some(_) => Asked::blocked(
                    "the app's window was matched by the window hint, not by process id, so it \
                     belongs to a process glass did not launch and is not sent a close request",
                ),
                None => Asked::none(),
            };
        }
        let total = wins.len();
        let asked = wins
            .into_iter()
            .filter(|&win| self.accepts_delete(win, protocols, delete))
            .filter(|&win| self.send_delete(win, protocols, delete).is_ok())
            .count();
        // Writing the request out is not enough: the ask runs on its own connection, which is
        // dropped the moment this returns, and a request still unprocessed when its sender
        // disconnects can be discarded — the app then waits out the whole grace for a message
        // nobody will deliver, and is signalled instead of asked. Measured on a loaded machine:
        // 4 of 10 teardowns lost the request that way, with the app's event loop demonstrably
        // running throughout. `sync` sends everything buffered and waits for the server to
        // answer a request queued behind it, so the close request has provably been processed
        // before the connection can go away.
        if let Err(e) = self.conn.sync() {
            return Asked::blocked(format!("glass could not deliver the close request: {e}"));
        }
        Asked::counted(total, asked, |unaskable| {
            format!(
                "{unaskable} of its {total} window(s) do not advertise the WM_DELETE_WINDOW \
                 protocol, so there is nothing to send them"
            )
        })
    }

    /// Whether `win` advertises `WM_DELETE_WINDOW` in its `WM_PROTOCOLS` property. A read
    /// failure reads as "no": an unasked window costs a graceful shutdown, a wrongly-asked one
    /// costs a grace period waiting for a message the app is entitled to ignore.
    fn accepts_delete(&self, win: Window, protocols: Atom, delete: Atom) -> bool {
        self.conn
            .get_property(false, win, protocols, AtomEnum::ATOM, 0, 32)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|reply| {
                reply
                    .value32()
                    .is_some_and(|mut a| a.any(|at| at == delete))
            })
    }

    /// Send one `WM_DELETE_WINDOW` client message to `win`, in the shape ICCCM 4.2.8 specifies:
    /// 32-bit format, the protocol atom first, a timestamp second, delivered with an empty event
    /// mask. The timestamp is `CURRENT_TIME` because there is no triggering event to take one
    /// from — glass is not reacting to a user action the way a window manager is.
    ///
    /// A window destroyed between the scan and this call is NOT reported here: `send_event`
    /// returns a `VoidCookie`, so the `BadWindow` comes back asynchronously and is dropped. Only
    /// a connection-level write failure surfaces. That costs at most a wait for a window that
    /// was already gone, which `await_close` short-circuits.
    fn send_delete(&self, win: Window, protocols: Atom, delete: Atom) -> Result<()> {
        let event = ClientMessageEvent::new(32, win, protocols, [delete, CURRENT_TIME, 0, 0, 0]);
        self.conn
            .send_event(false, win, EventMask::NO_EVENT, event)
            .map_err(|e| GlassError::Backend(format!("send WM_DELETE_WINDOW: {e}")))?;
        Ok(())
    }

    /// Kill and reap the launched child (if any) and forget its window. Used by
    /// `stop_app` and by `start_app`'s failure path so a launch that never finds
    /// a window does not orphan the process.
    ///
    /// The app is asked to close first and only signalled if it does not go: a signalled
    /// toolkit app runs no shutdown path at all, so anything it would have flushed on exit is
    /// lost and it can come back reporting a crash. The signal ladder stays as the fallback —
    /// it is what actually guarantees the process tree is gone.
    /// [`Self::request_close`] under a deadline, so a display that has stopped answering cannot
    /// hold teardown open.
    ///
    /// Every step of the ask is a blocking X round trip and x11rb has no per-request timeout, so
    /// the bound has to come from outside the connection: the ask runs on its own thread with its
    /// own connection to the same display, and the caller stops waiting after [`ASK_BUDGET`]. The
    /// second connection is what makes abandoning it safe — the thread cannot leave this backend's
    /// connection stopped mid-request — and any client may send a close request, so the ask is no
    /// less valid from there. A socket timeout would not work in its place: a spurious trip
    /// desynchronizes the X protocol stream, which has no message boundary to resynchronize to.
    ///
    /// An abandoned thread stays blocked until the display answers or glass exits. That is the
    /// cost of x11rb having no cancellation, and it buys the guarantee that matters here — the
    /// signal ladder still runs, so the app does not outlive the glass process that gave up on it.
    fn request_close_bounded(&self, root_pid: u32) -> Asked {
        let (display, active) = (self.display.clone(), self.window);
        let (tx, rx) = std::sync::mpsc::channel();
        let asker = std::thread::spawn(move || {
            let asked = match X11Platform::connect(Some(&display)) {
                Ok(mut asker) => {
                    // Carry the active window over so the ask sees what this backend is driving:
                    // `request_close` reports a launch that was found only by window hint.
                    asker.window = active;
                    asker.request_close(root_pid)
                }
                Err(e) => Asked::blocked(format!("glass could not reach the X server: {e}")),
            };
            let _ = tx.send(asked);
        });
        match rx.recv_timeout(ASK_BUDGET) {
            Ok(asked) => asked,
            // The display is alive but slow: the ask is still running in the thread, so it is
            // abandoned — joining would block teardown for a round trip x11rb cannot cancel.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Asked::blocked(format!(
                "the X server did not answer within {ASK_BUDGET:?}, so the app could not be asked \
                 to close"
            )),
            // The sender went with the thread, so it unwound: it arrived at once, not after a
            // full budget that never happened. It is glass's own ask thread that died, not the
            // X server, which may be healthy (glass#458); the thread is already gone, so the join
            // is immediate and carries what it said.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Asked::blocked(format!(
                "glass's own close-request thread ended without an answer ({}) — that is glass \
                 failing to ask, not the X server refusing; the app was not asked to close",
                crate::xvfb::ended_thread_payload(asker)
                    .unwrap_or_else(|| "it carried no message".to_string())
            )),
        }
    }

    fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Snapshot the launch's process tree before any of it exits. Waiting on the child
            // reaps it, which reparents its descendants to init and takes them out of the
            // subtree — after that there is no way to enumerate them again.
            let tree = proc_tree_pids(child.id());
            let asked = self.request_close_bounded(child.id());
            let closed_itself = asked.await_child_exit(&mut child, CLOSE_GRACE);
            // The sweep runs either way: an app that closed itself can still have forked children
            // it never cleaned up, and on the graceful path the signals land on processes that
            // are already gone and cost nothing.
            // Teardown reports what it asked, not what survived — doctor's deep probe is the
            // caller that reads this (glass#380).
            let _ = glass_proc_linux::reap_launch(&mut child, &tree, APP_REAP_GRACE);
            glass_proc_linux::disclose_teardown(&asked.outcome(closed_itself));
        }
        self.window = None;
        // Reaped first, so each tap's final drain sees what the app wrote on its way out.
        // Dropping them releases the pipes anything the launch left running still holds
        // (glass#477).
        self.taps.clear();
        // Drop the private a11y bus, reaping its dbus-daemon / at-spi children. Also
        // covers `start_app`'s failure path (which calls `kill_child`), so a launch
        // that never finds a window doesn't leave the bus running until Drop.
        self.dbus = None;
        // Dropping the owner stops its thread and releases the CLIPBOARD selection.
        self.clipboard_owner = None;
    }

    /// Poll the window tree until a top-level window matches a pid in the process
    /// tree rooted at the spawned child (via `_NET_WM_PID`) and/or the hint,
    /// or `timeout_ms` elapses.
    ///
    /// When sandbox wraps the app in `bwrap`, the spawned child is the bwrap
    /// process. The actual app is bwrap's child. `proc_tree_pids` collects the
    /// full descendant set so `_NET_WM_PID` matching works for both direct
    /// launches and bwrap-wrapped launches.
    fn discover_window(&mut self, spec: &AppSpec) -> Result<Window> {
        let root_pid = self.child.as_ref().map(|c| c.id());
        let pid_atom = self.intern(b"_NET_WM_PID")?;
        let client_list_atom = self.intern(b"_NET_CLIENT_LIST")?;

        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.max(1));
        loop {
            // Re-collect the pid set each iteration: a sandboxed launch's bwrap
            // child (the real app) appears in /proc shortly after bwrap starts.
            let pids: Vec<u32> = root_pid.map(proc_tree_pids).unwrap_or_default();
            if let Some(win) =
                self.scan_for_window(&pids, pid_atom, client_list_atom, spec.window_hint.as_ref())?
            {
                self.window = Some(win);
                return Ok(win);
            }
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(GlassError::app_exited_during_discovery(
                    status.code(),
                    spec.sandbox,
                ));
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Timeout(spec.timeout_ms));
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn scan_for_window(
        &self,
        pids: &[u32],
        pid_atom: Atom,
        client_list_atom: Atom,
        hint: Option<&WindowHint>,
    ) -> Result<Option<Window>> {
        let root_children = self
            .conn
            .query_tree(self.root)
            .map_err(|e| GlassError::Backend(format!("query_tree: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("query_tree reply: {e}")))?
            .children;
        // _NET_CLIENT_LIST (the WM's managed, possibly-reparented clients) first,
        // then root's direct children (no-WM / bare Xvfb fallback). Dedup so a
        // non-reparented window present in both is only checked once.
        let mut seen = std::collections::HashSet::new();
        for win in self
            .client_list_windows(client_list_atom)
            .into_iter()
            .chain(root_children)
        {
            if !seen.insert(win) {
                continue;
            }
            if self.window_matches(win, pids, pid_atom, hint)? {
                return Ok(Some(win));
            }
        }
        Ok(None)
    }

    /// The WM's managed client windows from `_NET_CLIENT_LIST` on the root, or an
    /// empty list if the property is absent or unreadable (non-EWMH / no WM).
    fn client_list_windows(&self, atom: Atom) -> Vec<Window> {
        self.conn
            .get_property(false, self.root, atom, AtomEnum::WINDOW, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|it| it.collect()))
            .unwrap_or_default()
    }

    fn window_matches(
        &self,
        win: Window,
        pids: &[u32],
        pid_atom: Atom,
        hint: Option<&WindowHint>,
    ) -> Result<bool> {
        let mapped = self
            .conn
            .get_window_attributes(win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|a| a.map_state == MapState::VIEWABLE)
            .unwrap_or(false);
        if !mapped {
            return Ok(false);
        }
        if !pids.is_empty()
            && let Some(reply) = self
                .conn
                .get_property(false, win, pid_atom, AtomEnum::CARDINAL, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
            && let Some(win_pid) = reply.value32().and_then(|mut v| v.next())
            && pids.contains(&win_pid)
        {
            return Ok(true);
        }
        if let Some(hint) = hint {
            let name = self.window_name(win);
            let class = self.window_class(win);
            let class_ref = class.as_ref().map(|(i, c)| (i.as_str(), c.as_str()));
            if hint_matches(name.as_deref(), class_ref, hint) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn window_name(&self, win: Window) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&reply.value).into_owned())
        }
    }

    /// Fetch and parse `WM_CLASS` as `(instance, class)`. The property is two
    /// NUL-separated strings (`instance\0class\0`); if only one is present, it
    /// is used for both.
    fn window_class(&self, win: Window) -> Option<(String, String)> {
        let reply = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            return None;
        }
        let mut parts = reply
            .value
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned());
        let instance = parts.next()?;
        let class = parts.next().unwrap_or_else(|| instance.clone());
        Some((instance, class))
    }

    fn warp(&self, ox: i32, oy: i32, x: i32, y: i32) -> Result<()> {
        let (rx, ry) = crate::coords::window_to_root(ox, oy, x, y);
        self.conn
            .xtest_fake_input(XT_MOTION, 0, x11rb::CURRENT_TIME, self.root, rx, ry, 0)
            .map_err(|e| GlassError::Backend(format!("xtest motion: {e}")))?;
        Ok(())
    }

    fn button(&self, kind: u8, detail: u8) -> Result<()> {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| GlassError::Backend(format!("xtest button: {e}")))?;
        Ok(())
    }

    fn scroll_button(
        &self,
        pos_btn: u8,
        neg_btn: u8,
        delta: i32,
        deadline: Deadline,
    ) -> Result<()> {
        run_scroll_buttons_by(
            pos_btn,
            neg_btn,
            delta,
            || deadline.has_passed(),
            |down, btn| self.button(if down { XT_BTN_PRESS } else { XT_BTN_RELEASE }, btn),
        )
    }

    /// Find a keycode (and whether Shift is needed) that produces `keysym`.
    fn keycode_for(&self, keysym: u32) -> Result<(u8, bool)> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|e| GlassError::Backend(format!("get_keyboard_mapping: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("keyboard mapping reply: {e}")))?;
        keycode_in(
            &mapping.keysyms,
            mapping.keysyms_per_keycode as usize,
            min,
            max,
            keysym,
        )
        .ok_or_else(|| GlassError::InvalidKey(format!("no keycode for keysym 0x{keysym:x}")))
    }

    fn modifier_keycode(&self, m: glass_core::keys::Modifier) -> Result<u8> {
        use glass_core::keys::Modifier;
        let keysym = match m {
            Modifier::Shift => 0xffe1,   // Shift_L
            Modifier::Control => 0xffe3, // Control_L
            Modifier::Alt => 0xffe9,     // Alt_L
            Modifier::Super => 0xffeb,   // Super_L
        };
        Ok(self.keycode_for(keysym)?.0)
    }

    fn tap_keycode(&self, keycode: u8) -> Result<()> {
        self.conn
            .xtest_fake_input(
                XT_KEY_PRESS,
                keycode,
                x11rb::CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )
            .map_err(|e| GlassError::Backend(format!("xtest key press: {e}")))?;
        self.conn
            .xtest_fake_input(
                XT_KEY_RELEASE,
                keycode,
                x11rb::CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )
            .map_err(|e| GlassError::Backend(format!("xtest key release: {e}")))?;
        Ok(())
    }

    /// Press each modifier's keycode down; returns the keycodes (for release).
    fn press_mods(&self, mods: &[glass_core::keys::Modifier]) -> Result<Vec<u8>> {
        let mut kcs = Vec::new();
        for m in mods {
            kcs.push(self.modifier_keycode(*m)?);
        }
        for kc in &kcs {
            self.conn
                .xtest_fake_input(XT_KEY_PRESS, *kc, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(|e| GlassError::Backend(format!("xtest mod press: {e}")))?;
        }
        Ok(kcs)
    }

    /// Release the given modifier keycodes (reverse order).
    fn release_mods(&self, kcs: &[u8]) -> Result<()> {
        for kc in kcs.iter().rev() {
            self.conn
                .xtest_fake_input(XT_KEY_RELEASE, *kc, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(|e| GlassError::Backend(format!("xtest mod release: {e}")))?;
        }
        Ok(())
    }

    fn key_with_mods(
        &self,
        keysym: u32,
        extra_shift: bool,
        mods: &[glass_core::keys::Modifier],
    ) -> Result<()> {
        let (keycode, needs_shift) = self.keycode_for(keysym)?;
        let mut mods = mods.to_vec();
        if (needs_shift || extra_shift) && !mods.contains(&glass_core::keys::Modifier::Shift) {
            mods.push(glass_core::keys::Modifier::Shift);
        }
        let kcs = self.press_mods(&mods)?;
        self.tap_keycode(keycode)?;
        self.release_mods(&kcs)
    }

    /// Raise `win` and give it X keyboard focus. XTEST key events are routed by
    /// the server to the focused window; in the WM-less headless Xvfb there is no
    /// window manager to assign focus, so glass must do it for synthetic keys to
    /// land. Used on launch, on select, and by `WindowOp::Focus`.
    ///
    /// `.check()`s the raise so a closed window's `BadWindow`/`BadDrawable` is
    /// observed here and translated into `WindowNotFound` (forgetting the stale
    /// id), consistent with `configure_active`/`window_geometry` — rather than
    /// surfacing an opaque `Backend(...)`.
    fn focus_window(&mut self, win: Window) -> Result<()> {
        let cookie = self
            .conn
            .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
            .map_err(|e| GlassError::Backend(format!("raise: {e}")))?;
        cookie.check().map_err(|e| {
            if is_window_gone(&e) {
                self.note_window_gone()
            } else {
                GlassError::Backend(format!("raise: {e}"))
            }
        })?;
        self.conn
            .set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .map_err(|e| GlassError::Backend(format!("set_input_focus: {e}")))?;
        self.commit()
    }

    /// Resolve a window-relative `region` (or the whole window) against `geo`
    /// into an absolute root-coordinate capture rectangle, rejecting a zero-area
    /// region and fitting the result to the (headless) display: a rectangle
    /// reaching off-screen is clipped to its visible portion (flagged in the
    /// returned [`ClippedRect`]) rather than issuing a doomed `GetImage`.
    fn resolve_capture_rect(
        &self,
        geo: &WindowGeometry,
        region: Option<&Region>,
    ) -> Result<crate::coords::ClippedRect> {
        let (w, h) = match region {
            Some(r) => (r.width, r.height),
            None => (geo.width, geo.height),
        };
        if w == 0 || h == 0 {
            return Err(GlassError::CaptureFailed("window has zero area".into()));
        }
        // A window (or region) reaching past the headless display makes GetImage
        // cover non-viewable area, which X rejects with a bare BadMatch. Clip to
        // the on-display portion here (a partial frame beats none); the root
        // window's pixel size is the display size.
        let display = {
            let root = &self.conn.setup().roots[self.screen_num];
            (
                u32::from(root.width_in_pixels),
                u32::from(root.height_in_pixels),
            )
        };
        crate::coords::clip_capture_to_display(geo, region, display)
    }

    /// `GetImage` a `w`x`h` rectangle at root-coordinate `(sx,sy)` and decode it to
    /// an RGBA `Frame`. Always reads from the ROOT drawable (not a specific
    /// window's own drawable) so overlapping popovers (separate override-redirect
    /// top-levels) are included in the capture.
    fn capture_screen_rect(&self, sx: i32, sy: i32, w: u32, h: u32) -> Result<Frame> {
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                sx as i16,
                sy as i16,
                w as u16,
                h as u16,
                !0u32,
            )
            .map_err(|e| GlassError::CaptureFailed(format!("get_image: {e}")))?
            .reply()
            .map_err(|e| GlassError::CaptureFailed(format!("get_image reply: {e}")))?;
        let bpp = self
            .conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == image.depth)
            .map(|f| f.bits_per_pixel as usize / 8)
            .ok_or_else(|| {
                GlassError::CaptureFailed(format!("no pixmap format for depth {}", image.depth))
            })?;
        let rgba = crate::pixels::xdata_to_rgba(&image.data, w, h, bpp)?;
        Frame::new(w, h, rgba)
    }
}

/// The diagnostic for a capture that was clipped to the display, or `None` when the whole
/// requested rectangle was read.
fn clip_note(rect: &crate::coords::ClippedRect) -> Option<String> {
    rect.clipped.then(|| {
        format!(
            "glass: capture reached past the headless display and was clipped to the visible \
             {}x{} region at ({},{}); returning a partial frame",
            rect.w, rect.h, rect.sx, rect.sy
        )
    })
}

// The process-tree walk (`/proc`-based) that maps the spawned child to the
// real app's descendants now lives in the shared `glass-proc-linux` crate
// (`proc_tree_pids`), used by both the X11 and Wayland backends.

/// Lets `glass_core::run_drag` drive an X11 drag through the backend's existing
/// XTEST primitives. Each method self-commits with `XFlush`; modifier keycodes
/// are held between `modifiers(true)`/`modifiers(false)`.
struct X11DragSink<'a> {
    p: &'a X11Platform,
    dispatch: &'a X11Dispatch,
    ox: i32,
    oy: i32,
    b: u8,
    mods: &'a [glass_core::keys::Modifier],
    kcs: Vec<u8>,
}

impl glass_core::DragSink for X11DragSink<'_> {
    fn place(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_to(x, y)
    }
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.p.warp(self.ox, self.oy, x, y)?;
        self.dispatch.mark();
        self.p.commit()
    }
    fn button(&mut self, down: bool) -> Result<()> {
        let kind = if down { XT_BTN_PRESS } else { XT_BTN_RELEASE };
        self.p.button(kind, self.b)?;
        self.dispatch.mark();
        self.p.commit()
    }
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if down {
            self.kcs = self.p.press_mods(self.mods)?;
        } else {
            self.p.release_mods(&self.kcs)?;
        }
        if !self.kcs.is_empty() {
            self.dispatch.mark();
        }
        self.p.commit()
    }
}

/// `TypeSink` for X11: each character is typed via XTEST and committed with its own `XFlush`
/// (like the chord sink), so `run_type`'s per-character commit reaches the server before the
/// next. A heavy client (e.g. a browser) drops a string whose key events are all flushed once
/// at the end. `idx` tracks position for the untypable-char error, which must never embed the
/// char value (it would leak typed content into the unredacted audit log).
struct X11TypeSink<'a> {
    p: &'a X11Platform,
    dispatch: &'a X11Dispatch,
    idx: usize,
}

impl glass_core::TypeSink for X11TypeSink<'_> {
    fn character(&mut self, c: char) -> Result<()> {
        let keysym = glass_core::keys::keysym_for_char(c).ok_or_else(|| {
            GlassError::InvalidKey(format!("char at index {} has no X11 keysym", self.idx))
        })?;
        self.idx += 1;
        self.p.key_with_mods(keysym, false, &[])?;
        self.dispatch.mark();
        self.p.commit()
    }
}

/// Lets `glass_core::run_chord` drive an X11 key chord through the existing XTEST primitives. Each
/// method self-commits with `XFlush`; the modifier keycodes are held between `modifiers(true)` and
/// `modifiers(false)`, so a frame-based client sees the modifier held across the key's frame.
struct X11ChordSink<'a> {
    p: &'a X11Platform,
    dispatch: &'a X11Dispatch,
    mods: &'a [glass_core::keys::Modifier],
    keycode: u8,
    kcs: Vec<u8>,
}

impl glass_core::ChordSink for X11ChordSink<'_> {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if down {
            self.kcs = self.p.press_mods(self.mods)?;
        } else {
            self.p.release_mods(&self.kcs)?;
        }
        if !self.kcs.is_empty() {
            self.dispatch.mark();
        }
        self.p.commit()
    }
    fn key(&mut self, down: bool) -> Result<()> {
        let kind = if down { XT_KEY_PRESS } else { XT_KEY_RELEASE };
        self.p
            .conn
            .xtest_fake_input(
                kind,
                self.keycode,
                x11rb::CURRENT_TIME,
                self.p.root,
                0,
                0,
                0,
            )
            .map_err(|e| GlassError::Backend(format!("xtest key: {e}")))?;
        self.dispatch.mark();
        self.p.commit()
    }
}

/// Lets `glass_core::run_scroll` drive an X11 scroll through the existing XTEST primitives. The
/// modifier keycodes are held between `modifiers(true)` and `modifiers(false)`, so a frame-based
/// client sees the modifier held across the wheel's frame; each method self-commits with `XFlush`.
struct X11ScrollSink<'a> {
    p: &'a X11Platform,
    dispatch: &'a X11Dispatch,
    ox: i32,
    oy: i32,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    mods: &'a [glass_core::keys::Modifier],
    kcs: Vec<u8>,
    deadline: Deadline,
}

impl glass_core::ScrollSink for X11ScrollSink<'_> {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if down {
            self.kcs = self.p.press_mods(self.mods)?;
        } else {
            self.p.release_mods(&self.kcs)?;
        }
        if !self.kcs.is_empty() {
            self.dispatch.mark();
        }
        self.p.commit()
    }
    fn wheel(&mut self) -> Result<()> {
        self.p.warp(self.ox, self.oy, self.x, self.y)?;
        self.dispatch.mark();
        // 4=up,5=down,6=left,7=right; click |delta| times.
        self.p.scroll_button(5, 4, self.dy, self.deadline)?;
        self.p.scroll_button(7, 6, self.dx, self.deadline)?;
        self.p.commit()
    }
}

impl Platform for X11Platform {
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        ensure_sandbox_available(spec.sandbox, glass_sandbox_linux::availability)?;
        glass_sandbox_linux::run_build(spec)?;
        // Opt-in private, isolated a11y bus (its own XDG_RUNTIME_DIR — never the host
        // /run/user/UID/at-spi/) so the launched app publishes an AT-SPI tree. The caller
        // explicitly opted in, so a bus that can't start fails the launch with the real cause
        // rather than silently degrading; nothing is leaked on this early return, since
        // `PrivateBus::start` reaps its own partial children and no app child exists yet. For
        // sandboxed launches, `spawn` binds the private bus dir into the bwrap run.
        self.dbus = if spec.a11y {
            Some(glass_dbus_linux::PrivateBus::start().map_err(|e| {
                glass_core::GlassError::AccessibilityUnavailable(format!(
                    "a11y:true was requested but the private a11y bus could not start: {e}"
                ))
            })?)
        } else {
            None
        };
        if let Err(e) = self.spawn(spec) {
            self.kill_child(); // reap the private bus (and any child) on a failed spawn
            return Err(e);
        }
        match self
            .discover_window(spec)
            .and_then(|_| self.window_geometry())
        {
            Ok(geo) => {
                // Give the launched window keyboard focus so synthetic keys reach
                // it (no WM in the headless Xvfb assigns focus). Best-effort: a
                // focus failure must not fail an otherwise-successful launch.
                if let Some(win) = self.window
                    && let Err(e) = self.focus_window(win)
                {
                    eprintln!("glass: focus-on-launch failed (keys may not reach the window): {e}");
                }
                Ok(geo)
            }
            Err(e) => {
                // Window never appeared (or geometry failed): don't orphan the child.
                self.kill_child();
                Err(e)
            }
        }
    }

    /// Ignores the deadline — the close-then-signal ladder above is asserted against
    /// `TEARDOWN_BUDGET` instead.
    fn stop_app_by(&mut self, _deadline: glass_core::Deadline) -> Result<()> {
        self.kill_child();
        Ok(())
    }

    fn capture_frame_by(&mut self, region: Option<&Region>, deadline: Deadline) -> Result<Frame> {
        run_x11_call_by(deadline, "capture", |dispatch| {
            // `window_geometry()` itself calls `require_window()`, so it doubles as
            // the "is there an active window" guard — no separate binding needed.
            let geo = self.window_geometry()?;
            let rect = self.resolve_capture_rect(&geo, region)?;
            if let Some(note) = clip_note(&rect) {
                eprintln!("{note}");
            }
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started("capture"));
            }
            // Capture from ROOT over the window's screen region so overlapping popovers
            // (separate override-redirect top-levels) are included, not just this window's
            // own (possibly-obscured) drawable.
            let frame = self.capture_screen_rect(rect.sx, rect.sy, rect.w, rect.h)?;
            dispatch.mark();
            Ok(frame)
        })
    }

    fn capture_window_by(
        &mut self,
        id: WindowId,
        region: Option<&Region>,
        deadline: Deadline,
    ) -> Result<Frame> {
        run_x11_call_by(deadline, "window capture", |dispatch| {
            // Mirror select_window's WindowId -> Window mapping/validation, but never
            // touch `self.window` — this must not retarget the active window.
            let pids: Vec<u32> = self
                .child
                .as_ref()
                .map(|c| proc_tree_pids(c.id()))
                .unwrap_or_default();
            let target = id.0 as Window;
            if !self.scan_all_windows(&pids)?.contains(&target) {
                return Err(GlassError::WindowNotFound);
            }
            let geo = self.geometry_of(target)?;
            if let Some(r) = region {
                // `region` must fit the TARGET window's own geometry, not just the
                // shared Xvfb display — otherwise an over-large region that still
                // lands inside the display would silently capture pixels outside
                // this window (desktop / other windows) instead of erroring.
                r.check_fits(geo.width, geo.height)?;
            }
            let rect = self.resolve_capture_rect(&geo, region)?;
            if let Some(note) = clip_note(&rect) {
                eprintln!("{note}");
            }
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started("window capture"));
            }
            let frame = self.capture_screen_rect(rect.sx, rect.sy, rect.w, rect.h)?;
            dispatch.mark();
            Ok(frame)
        })
    }

    fn send_pointer_by(&mut self, event: &PointerEvent, deadline: Deadline) -> Result<()> {
        run_x11_call_by(deadline, "pointer input", |dispatch| {
            let origin = self.window_geometry()?;
            let (ox, oy) = (origin.x, origin.y);
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started("pointer input"));
            }
            match *event {
                PointerEvent::Move { x, y } => {
                    self.warp(ox, oy, x, y)?;
                    dispatch.mark();
                }
                PointerEvent::Scroll {
                    x,
                    y,
                    dx,
                    dy,
                    ref modifiers,
                } => {
                    // Shared, frame-aware sequencing: hold the modifier across the wheel's frame instead
                    // of bursting modifier+wheel+release into one — see glass_core::run_scroll.
                    let mut sink = X11ScrollSink {
                        p: &*self,
                        dispatch,
                        ox,
                        oy,
                        x,
                        y,
                        dx,
                        dy,
                        mods: modifiers.as_slice(),
                        kcs: Vec::new(),
                        deadline,
                    };
                    glass_core::run_scroll_by(&mut sink, !modifiers.is_empty(), deadline)?;
                }
                PointerEvent::Click {
                    x,
                    y,
                    button,
                    count,
                    ref modifiers,
                } => {
                    self.warp(ox, oy, x, y)?;
                    dispatch.mark();
                    if deadline.has_passed() {
                        return Err(GlassError::caller_deadline_elapsed("pointer input"));
                    }
                    let kcs = self.press_mods(modifiers)?;
                    if !kcs.is_empty() {
                        dispatch.mark();
                    }
                    let b = button_number(button);
                    run_clicks_by(
                        count,
                        || deadline.has_passed(),
                        |down| {
                            self.button(if down { XT_BTN_PRESS } else { XT_BTN_RELEASE }, b)?;
                            dispatch.mark();
                            Ok(())
                        },
                        |button_down, failed| {
                            let mut cleanup_error = None;
                            if button_down && let Err(error) = self.button(XT_BTN_RELEASE, b) {
                                cleanup_error = Some(error);
                            } else if button_down {
                                dispatch.mark();
                            }
                            if let Err(error) = self.release_mods(&kcs)
                                && cleanup_error.is_none()
                            {
                                cleanup_error = Some(error);
                            }
                            if failed
                                && let Err(error) = self
                                    .conn
                                    .sync()
                                    .map_err(|e| GlassError::Backend(format!("sync: {e}")))
                                && cleanup_error.is_none()
                            {
                                cleanup_error = Some(error);
                            }
                            match cleanup_error {
                                Some(error) => Err(error),
                                None => Ok(()),
                            }
                        },
                    )?;
                }
                PointerEvent::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    button,
                    ref modifiers,
                    duration_ms,
                } => {
                    let gesture =
                        glass_core::DragGesture::plan((from_x, from_y), (to_x, to_y), duration_ms);
                    let mut sink = X11DragSink {
                        p: &*self,
                        dispatch,
                        ox,
                        oy,
                        b: button_number(button),
                        mods: modifiers.as_slice(),
                        kcs: Vec::new(),
                    };
                    glass_core::run_drag_by(&mut sink, &gesture, deadline)?;
                }
                PointerEvent::Gesture { .. } => {
                    return Err(crate::unsupported_multi_touch());
                }
            }
            self.commit()
        })
    }

    fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        run_x11_call_by(deadline, "key input", |dispatch| {
            match event {
                KeyEvent::Text(text) => {
                    // Per-character, self-committed typing (an XFlush per char) so a heavy client
                    // (e.g. a browser) receives a long string instead of dropping events flushed
                    // once at the end — see glass_core::run_type and X11TypeSink. The 8ms dwell
                    // paces between characters (XFlush sends but does not wait).
                    let mut sink = X11TypeSink {
                        p: &*self,
                        dispatch,
                        idx: 0,
                    };
                    run_x11_type_by(&mut sink, text, Duration::from_millis(8), deadline)?;
                }
                KeyEvent::Chord(chord) => {
                    let (mods, keysym) = glass_core::keys::parse_chord(chord)?;
                    let (keycode, needs_shift) = self.keycode_for(keysym)?;
                    let mut mods = mods;
                    if needs_shift && !mods.contains(&glass_core::keys::Modifier::Shift) {
                        mods.push(glass_core::keys::Modifier::Shift);
                    }
                    let mut sink = X11ChordSink {
                        p: &*self,
                        dispatch,
                        mods: &mods,
                        keycode,
                        kcs: Vec::new(),
                    };
                    glass_core::run_chord_by(&mut sink, deadline)?;
                }
            }
            self.commit()
        })
    }

    fn get_clipboard(&mut self) -> Result<String> {
        crate::clipboard::get(&self.display)
    }

    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        match &self.clipboard_owner {
            Some(o) if o.is_alive() => {
                o.set_text(text);
                Ok(())
            }
            _ => {
                self.clipboard_owner = Some(crate::clipboard::ClipboardOwner::spawn(
                    self.display.clone(),
                    text.to_string(),
                )?);
                Ok(())
            }
        }
    }

    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let win = self.require_window()?;
        match *op {
            WindowOp::Focus => {
                self.focus_window(win)?;
            }
            WindowOp::Resize { width, height } => {
                self.configure_active(
                    win,
                    &ConfigureWindowAux::new().width(width).height(height),
                    "resize",
                )?;
            }
            WindowOp::Move { x, y } => {
                self.configure_active(win, &ConfigureWindowAux::new().x(x).y(y), "move")?;
            }
            WindowOp::Geometry => {}
        }
        self.commit()?;
        self.window_geometry()
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        self.require_window()?; // no active app -> WindowNotFound, not an empty list
        let pids: Vec<u32> = self
            .child
            .as_ref()
            .map(|c| proc_tree_pids(c.id()))
            .unwrap_or_default();
        let active = self.window;
        let mut out = Vec::new();
        for win in self.scan_all_windows(&pids)? {
            out.push(WindowInfo {
                id: WindowId(win as u64),
                title: self.window_name(win),
                class: self.window_class(win).map(|(_instance, class)| class),
                geometry: self.geometry_of(win)?,
                active: Some(win) == active,
            });
        }
        Ok(out)
    }

    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        let pids: Vec<u32> = self
            .child
            .as_ref()
            .map(|c| proc_tree_pids(c.id()))
            .unwrap_or_default();
        let target = id.0 as Window;
        if self.scan_all_windows(&pids)?.contains(&target) {
            self.window = Some(target);
            // Move keyboard focus to the selected window so subsequent synthetic
            // keys reach it. Best-effort: a focus failure must not fail selection.
            if let Err(e) = self.focus_window(target) {
                eprintln!("glass: focus-on-select failed (keys may not reach the window): {e}");
            }
            self.geometry_of(target)
        } else {
            Err(GlassError::WindowNotFound)
        }
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        std::mem::take(&mut *self.logs.lock().expect("log buffer mutex"))
    }

    fn app_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// The app's full process subtree, not just the spawned child. For a
    /// sandboxed launch the spawned child is `bwrap` and the real app is a
    /// descendant with a different pid; the a11y reader correlates the AT-SPI
    /// connection pid against this set, so it must include descendants — the
    /// inherited `[app_pid()]` default breaks a11y for every `sandbox != off`
    /// launch. Mirrors the `proc_tree_pids` set used by window discovery.
    fn app_pids(&self) -> Vec<u32> {
        match &self.child {
            Some(c) => proc_tree_pids(c.id()),
            None => Vec::new(),
        }
    }

    fn a11y_bus_addr(&self) -> Option<String> {
        self.dbus.as_ref().map(|b| b.a11y_bus_address().to_string())
    }
}

/// Decide whether a window's fetched `WM_NAME` and `WM_CLASS` satisfy a hint.
/// Pure (no X), so it can be unit-tested exhaustively. A hint matches when the
/// title equals `WM_NAME` exactly, OR the class equals *either* part of the
/// window's `WM_CLASS` (instance or class) — an agent rarely knows which, and
/// both are stable identifiers. Title and class are OR'd: any provided field
/// that matches is enough.
fn hint_matches(name: Option<&str>, class: Option<(&str, &str)>, hint: &WindowHint) -> bool {
    if let Some(title) = &hint.title
        && name == Some(title.as_str())
    {
        return true;
    }
    if let Some(want) = &hint.class
        && let Some((instance, class)) = class
        && (instance == want.as_str() || class == want.as_str())
    {
        return true;
    }
    false
}

impl Drop for X11Platform {
    /// Reap the launched app on drop — parity with the Wayland/Windows backends, for a backend
    /// dropped without an explicit `stop_app()` (panic-unwind, or the process-exit backstop
    /// path). Not a guarantee: glass-mcp drops on a detached thread nothing joins, so a process
    /// exiting right after kills this partway (glass#415) — it stays clean where the Wayland one
    /// does not only by being short. `kill_child` uses
    /// `self.child.take()`, so this is idempotent with `stop_app`. Field order then
    /// drops `xvfb`, tearing down any private display we spawned.
    fn drop(&mut self) {
        self.kill_child(); // takes the child, and drops the clipboard owner with it
    }
}

/// Fail a launch that asked for containment this machine cannot provide.
///
/// `probe` is a thunk because it forks bubblewrap, and `sandbox:"off"` is the escape hatch
/// for machines where that is exactly what does not work. Kept out of `start_app` so both
/// answers are reachable from a test.
fn ensure_sandbox_available(
    level: glass_core::SandboxLevel,
    probe: impl FnOnce() -> glass_sandbox_linux::Availability,
) -> Result<()> {
    if level == glass_core::SandboxLevel::Off {
        return Ok(());
    }
    match probe() {
        glass_sandbox_linux::Availability::Ok => Ok(()),
        // The fix travels with the cause: only the probe knows whether bubblewrap is missing,
        // refusing, or wedged. Telling a user to install one that is installed sends them past the
        // mount holding it.
        glass_sandbox_linux::Availability::Unavailable(why) => Err(GlassError::SandboxUnavailable(
            format!("{why}. See `glass-mcp doctor`."),
        )),
    }
}

/// Search a `GetKeyboardMapping` table for `keysym`, returning the keycode that produces it
/// and whether Shift is needed — column 0 is the unshifted keysym, column 1 the shifted one.
///
/// `keysyms` is the flat table for keycodes `min..=max`, `per` entries each. Do not fold this
/// back into the request around it: against a live server every arithmetic slip here still
/// lands on *some* real key.
fn keycode_in(keysyms: &[u32], per: usize, min: u8, max: u8, keysym: u32) -> Option<(u8, bool)> {
    for kc in min..=max {
        let base = (kc as usize - min as usize) * per;
        if keysyms.get(base) == Some(&keysym) {
            return Some((kc, false));
        }
        if per > 1 && keysyms.get(base + 1) == Some(&keysym) {
            return Some((kc, true));
        }
    }
    None
}

fn button_number(button: glass_core::MouseButton) -> u8 {
    match button {
        glass_core::MouseButton::Left => 1,
        glass_core::MouseButton::Middle => 2,
        glass_core::MouseButton::Right => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        hint_matches, run_clicks_by, run_scroll_buttons_by, run_x11_call_by, run_x11_type_by,
    };
    use glass_core::{BoundDispatch, Deadline, GlassError, Result, TypeSink, Whose, WindowHint};
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct RecordingTypeSink {
        characters: Vec<char>,
    }

    impl TypeSink for RecordingTypeSink {
        fn character(&mut self, character: char) -> Result<()> {
            self.characters.push(character);
            std::thread::sleep(Duration::from_millis(10));
            Ok(())
        }
    }

    #[test]
    fn spent_input_deadline_dispatches_no_backend_events() {
        let mut recorded_events = Vec::new();
        let deadline = Deadline::at(Instant::now() - Duration::from_millis(1));

        let error = run_x11_call_by(deadline, "pointer input", |_| {
            recorded_events.push("motion");
            Ok(())
        })
        .expect_err("spent input must be rejected before dispatch");

        assert!(recorded_events.is_empty());
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn pre_dispatch_caller_deadline_error_stays_not_dispatched() {
        let error = run_x11_call_by(Deadline::UNBOUNDED, "key input", |_| {
            Err::<(), _>(GlassError::deadline_not_started("typing"))
        })
        .expect_err("typing did not reach XTEST");

        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn click_timeout_after_press_releases_and_syncs_before_returning() {
        let events = RefCell::new(Vec::new());
        let checks = Cell::new(0);

        run_clicks_by(
            1,
            || {
                checks.set(checks.get() + 1);
                checks.get() == 2
            },
            |down| {
                events
                    .borrow_mut()
                    .push(if down { "press" } else { "release" });
                Ok(())
            },
            |button_down, failed| {
                if button_down {
                    events.borrow_mut().push("release");
                }
                if failed {
                    events.borrow_mut().push("sync");
                }
                Ok(())
            },
        )
        .expect_err("the deadline expires after the press");

        assert_eq!(*events.borrow(), ["press", "release", "sync"]);
    }

    #[test]
    fn large_scroll_stops_before_the_first_notch_after_the_deadline() {
        let events = RefCell::new(Vec::new());
        let checks = Cell::new(0);

        let error = run_x11_call_by(Deadline::UNBOUNDED, "pointer input", |dispatch| {
            run_scroll_buttons_by(
                5,
                4,
                1_000,
                || {
                    checks.set(checks.get() + 1);
                    checks.get() > 1
                },
                |down, button| {
                    events.borrow_mut().push((down, button));
                    dispatch.mark();
                    Ok(())
                },
            )
        })
        .expect_err("the deadline expires before the second notch");

        assert_eq!(
            (events.borrow().as_slice(), error.bound_dispatch()),
            (
                &[(true, 5u8), (false, 5u8)][..],
                Some(BoundDispatch::MayHaveDispatched),
            )
        );
    }

    #[test]
    fn short_typing_deadline_stops_before_all_characters() {
        let requested_text = "abcd";
        let mut sink = RecordingTypeSink::default();

        let error = run_x11_type_by(
            &mut sink,
            requested_text,
            Duration::ZERO,
            Deadline::from_millis(5),
        )
        .expect_err("typing must stop when the shared deadline expires");

        assert!(sink.characters.len() < requested_text.chars().count());
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn capture_returning_after_the_deadline_is_not_success() {
        let capture_error = run_x11_call_by(Deadline::from_millis(1), "capture", |dispatch| {
            dispatch.mark();
            std::thread::sleep(Duration::from_millis(10));
            Ok(())
        })
        .expect_err("a late capture must not return success");

        assert_eq!(capture_error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            capture_error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    // proc_tree_pids / collect_descendants moved to the `glass-proc-linux` crate
    // (tested there).

    fn hint(title: Option<&str>, class: Option<&str>) -> WindowHint {
        WindowHint {
            title: title.map(Into::into),
            class: class.map(Into::into),
        }
    }

    #[test]
    fn matches_title_exactly() {
        let h = hint(Some("Calculator"), None);
        assert!(hint_matches(Some("Calculator"), None, &h));
        assert!(
            !hint_matches(Some("Calc"), None, &h),
            "title is an exact match, not substring"
        );
    }

    #[test]
    fn class_hint_matches_either_instance_or_class() {
        // xcalc's WM_CLASS is ("xcalc", "XCalc") — either should satisfy the hint.
        assert!(hint_matches(
            None,
            Some(("xcalc", "XCalc")),
            &hint(None, Some("XCalc"))
        ));
        assert!(hint_matches(
            None,
            Some(("xcalc", "XCalc")),
            &hint(None, Some("xcalc"))
        ));
        assert!(!hint_matches(
            None,
            Some(("xcalc", "XCalc")),
            &hint(None, Some("gedit"))
        ));
    }

    #[test]
    fn class_hint_does_not_match_when_window_has_no_class() {
        assert!(!hint_matches(
            Some("whatever"),
            None,
            &hint(None, Some("XCalc"))
        ));
    }

    #[test]
    fn either_title_or_class_can_match() {
        // title wrong but class right still matches (OR semantics).
        let h = hint(Some("Nope"), Some("XCalc"));
        assert!(hint_matches(
            Some("Calculator"),
            Some(("xcalc", "XCalc")),
            &h
        ));
    }

    /// Keycodes 8, 9, 10 with two columns each: unshifted then shifted.
    fn two_column_map() -> Vec<u32> {
        vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
    }

    #[test]
    fn a_keysym_in_the_unshifted_column_needs_no_shift() {
        let map = two_column_map();
        assert_eq!(
            super::keycode_in(&map, 2, 8, 10, 0xcc),
            Some((9, false)),
            "0xcc is keycode 9's unshifted keysym"
        );
    }

    #[test]
    fn a_keysym_in_the_shifted_column_reports_that_shift_is_needed() {
        let map = two_column_map();
        assert_eq!(super::keycode_in(&map, 2, 8, 10, 0xbb), Some((8, true)));
    }

    #[test]
    fn the_last_keycode_in_the_range_is_still_searched() {
        let map = two_column_map();
        assert_eq!(super::keycode_in(&map, 2, 8, 10, 0xee), Some((10, false)));
        assert_eq!(super::keycode_in(&map, 2, 8, 10, 0xff), Some((10, true)));
    }

    #[test]
    fn a_keysym_the_map_does_not_carry_is_not_found() {
        let map = two_column_map();
        assert_eq!(super::keycode_in(&map, 2, 8, 10, 0x99), None);
    }

    #[test]
    fn a_single_column_map_never_reads_into_the_next_keycode() {
        // With one keysym per keycode there is no shifted column, and index `base + 1` is
        // already the *next* key — reading it would report the wrong keycode entirely.
        let map = vec![0xaa, 0xbb, 0xcc];
        assert_eq!(super::keycode_in(&map, 1, 8, 10, 0xbb), Some((9, false)));
        assert_eq!(super::keycode_in(&map, 1, 8, 10, 0xcc), Some((10, false)));
    }

    #[test]
    fn an_unconfined_launch_never_runs_the_bubblewrap_probe() {
        // A panicking probe is the only way to assert the probe is never called.
        use glass_core::SandboxLevel;
        super::ensure_sandbox_available(SandboxLevel::Off, || {
            panic!("sandbox:off must not probe for bubblewrap")
        })
        .expect("an unconfined launch is always allowed");
    }

    #[test]
    fn a_contained_launch_is_refused_when_bubblewrap_cannot_work() {
        use glass_core::{GlassError, SandboxLevel};
        use glass_sandbox_linux::Availability;
        let err = super::ensure_sandbox_available(SandboxLevel::Default, || {
            Availability::Unavailable("no user namespaces".into())
        })
        .expect_err("a contained launch cannot proceed without containment");
        assert!(matches!(err, GlassError::SandboxUnavailable(_)), "{err:?}");
        assert!(
            err.to_string().contains("no user namespaces"),
            "the real cause must survive into the message: {err}"
        );
        super::ensure_sandbox_available(SandboxLevel::Default, || Availability::Ok)
            .expect("a working bubblewrap refuses nothing");
    }

    #[test]
    fn empty_hint_never_matches() {
        let h = hint(None, None);
        assert!(!hint_matches(Some("anything"), Some(("a", "b")), &h));
    }

    #[test]
    fn a_clip_note_is_produced_only_when_the_capture_was_clipped() {
        use crate::coords::ClippedRect;
        let clipped = ClippedRect {
            sx: 0,
            sy: 0,
            w: 320,
            h: 200,
            clipped: true,
        };
        let note = super::clip_note(&clipped).expect("a clipped capture must say so");
        assert!(note.contains("320x200"), "{note}");
        assert!(
            super::clip_note(&ClippedRect {
                clipped: false,
                ..clipped
            })
            .is_none(),
            "an unclipped capture has nothing to report"
        );
    }
}

/// Everything the backend can only do against a real display: enumerating windows, reading
/// their properties, translating the server's errors, and driving XTEST. Every test here is
/// `#[ignore]`d — that is what the module is for, so a display-free test does not belong in it.
#[cfg(test)]
mod display_tests {
    use super::*;
    use crate::testx::TestX;

    /// A pid no window on a fresh private display can be carrying.
    const OTHER_PID: u32 = 999_999;

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn interning_is_stable_per_name_and_distinct_across_names() {
        let x = TestX::start();
        let plat = x.platform();
        let pid = plat.intern(b"_NET_WM_PID").expect("intern");
        assert_ne!(pid, 0, "an interned atom is never the null atom");
        assert_eq!(pid, plat.intern(b"_NET_WM_PID").expect("intern"));
        assert_ne!(pid, plat.intern(b"_NET_CLIENT_LIST").expect("intern"));
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn there_is_no_active_window_until_one_is_selected() {
        let x = TestX::start();
        let mut plat = x.platform();
        assert!(matches!(
            plat.require_window(),
            Err(GlassError::WindowNotFound)
        ));
        let win = x.window().create();
        plat.window = Some(win);
        assert_eq!(plat.require_window().expect("now set"), win);
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_windows_name_is_read_back_and_absence_is_not_an_empty_string() {
        let x = TestX::start();
        let plat = x.platform();
        let named = x.window().named("Calculator").create();
        let anonymous = x.window().create();
        assert_eq!(plat.window_name(named).as_deref(), Some("Calculator"));
        assert_eq!(
            plat.window_name(anonymous),
            None,
            "a window with no WM_NAME has no name, rather than an empty one"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_windows_class_is_read_back_as_the_instance_class_pair() {
        let x = TestX::start();
        let plat = x.platform();
        let win = x.window().classed("xcalc", "XCalc").create();
        assert_eq!(
            plat.window_class(win),
            Some(("xcalc".to_string(), "XCalc".to_string()))
        );
        assert_eq!(plat.window_class(x.window().create()), None);
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_single_component_class_stands_for_both_halves() {
        // ICCCM wants `instance\0class\0`; some apps write only one string, and dropping the
        // pair rather than doubling it would make their windows unmatchable by class.
        let x = TestX::start();
        let plat = x.platform();
        let win = x.window().classed("solo", "").create();
        assert_eq!(
            plat.window_class(win),
            Some(("solo".to_string(), "solo".to_string()))
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn geometry_is_reported_in_root_coordinates() {
        let x = TestX::start();
        let plat = x.platform();
        let win = x.window().at(37, 53).sized(321, 211).create();
        assert_eq!(
            plat.geometry_of(win).expect("geometry"),
            WindowGeometry {
                x: 37,
                y: 53,
                width: 321,
                height: 211
            }
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_active_windows_geometry_is_its_own() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().at(11, 22).sized(133, 144).create();
        plat.window = Some(win);
        assert_eq!(
            plat.window_geometry().expect("geometry"),
            WindowGeometry {
                x: 11,
                y: 22,
                width: 133,
                height: 144
            }
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_op_on_a_destroyed_window_reports_it_gone_and_forgets_it() {
        // The stale id must not come back on the next call as a raw protocol error — the
        // caller gets one actionable WindowNotFound either way.
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().create();
        plat.window = Some(win);
        x.destroy(win);
        assert!(matches!(
            plat.window_geometry(),
            Err(GlassError::WindowNotFound)
        ));
        assert_eq!(plat.window, None, "the stale id must be forgotten");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_dead_display_is_a_backend_failure_not_a_closed_window() {
        // Only `BadWindow`/`BadDrawable` mean the window went away. Reporting a lost
        // connection as WindowNotFound would send the caller looking for an app that closed,
        // when the whole display is gone — and would drop the window id it can still use.
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().create();
        plat.window = Some(win);
        drop(x);

        let err = plat
            .window_geometry()
            .expect_err("a request on a dead display cannot succeed");
        assert!(
            matches!(err, GlassError::Backend(_)),
            "expected a backend failure, got {err:?}"
        );
        assert_eq!(
            plat.window,
            Some(win),
            "a dead display must not make glass forget which window it was driving"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_client_list_is_read_from_the_root_and_empty_when_unset() {
        let x = TestX::start();
        let plat = x.platform();
        let atom = plat.intern(b"_NET_CLIENT_LIST").expect("intern");
        assert!(
            plat.client_list_windows(atom).is_empty(),
            "a bare Xvfb has no window manager and so no client list"
        );
        let (a, b) = (x.window().create(), x.window().create());
        x.set_client_list(&[a, b]);
        assert_eq!(plat.client_list_windows(atom), vec![a, b]);
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_mapped_window_owned_by_the_app_matches() {
        let x = TestX::start();
        let plat = x.platform();
        let pid_atom = plat.intern(b"_NET_WM_PID").expect("intern");
        let win = x.window().owned_by(4242).create();
        assert!(
            plat.window_matches(win, &[4242], pid_atom, None)
                .expect("match")
        );
        assert!(
            !plat
                .window_matches(win, &[OTHER_PID], pid_atom, None)
                .expect("match"),
            "another process's window is not the app's"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_unmapped_window_never_matches() {
        // Windows exist in the tree before they are shown; treating one as the app's window
        // hands back a target with nothing on screen.
        let x = TestX::start();
        let plat = x.platform();
        let pid_atom = plat.intern(b"_NET_WM_PID").expect("intern");
        let hidden = x.window().owned_by(4242).unmapped().create();
        assert!(
            !plat
                .window_matches(hidden, &[4242], pid_atom, None)
                .expect("match")
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_window_hint_matches_a_window_outside_the_pid_set() {
        // The fallback for apps that never set _NET_WM_PID.
        let x = TestX::start();
        let plat = x.platform();
        let pid_atom = plat.intern(b"_NET_WM_PID").expect("intern");
        let win = x.window().named("Calculator").create();
        let hint = WindowHint {
            title: Some("Calculator".into()),
            class: None,
        };
        assert!(
            plat.window_matches(win, &[], pid_atom, Some(&hint))
                .expect("match")
        );
        let wrong = WindowHint {
            title: Some("Spreadsheet".into()),
            class: None,
        };
        assert!(
            !plat
                .window_matches(win, &[], pid_atom, Some(&wrong))
                .expect("match")
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn scanning_returns_every_window_the_app_owns_and_nothing_else() {
        let x = TestX::start();
        let plat = x.platform();
        let main = x.window().owned_by(4242).named("main").create();
        let dialog = x.window().owned_by(4242).named("dialog").create();
        let _stranger = x.window().owned_by(OTHER_PID).named("stranger").create();
        let mut found = plat.scan_all_windows(&[4242]).expect("scan");
        found.sort_unstable();
        let mut want = vec![main, dialog];
        want.sort_unstable();
        assert_eq!(found, want);
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_window_listed_twice_is_returned_once() {
        // `_NET_CLIENT_LIST` and the root's children overlap whenever a window is not
        // reparented, which is every window under a bare Xvfb.
        let x = TestX::start();
        let plat = x.platform();
        let win = x.window().owned_by(4242).create();
        x.set_client_list(&[win]);
        assert_eq!(plat.scan_all_windows(&[4242]).expect("scan"), vec![win]);
    }

    // --- XTEST input -----------------------------------------------------------------
    //
    // Every assertion here reads the server's own state or the events a watching window
    // received. `Ok(())` from an input call means the request was written, not that anything
    // moved — which is exactly what the mutants replacing these bodies return.

    /// The keysym for a plain lowercase `a`, and for the shifted `A` on the same key.
    const KEYSYM_A: u32 = 0x61;
    const KEYSYM_SHIFT_A: u32 = 0x41;
    const KEYSYM_SHIFT_L: u32 = 0xffe1;

    /// Push the backend's buffered requests and wait for the server to have processed them.
    /// The input primitives do not flush, and a flush would not be enough anyway: these tests
    /// read the result over a *second* connection, ordered against the first only by a reply.
    fn commit(plat: &X11Platform) {
        plat.conn.sync().expect("sync");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_warp_moves_the_pointer_to_the_windows_origin_plus_the_offset() {
        let x = TestX::start();
        let plat = x.platform();
        plat.warp(100, 50, 7, 9).expect("warp");
        commit(&plat);
        let (px, py, _) = x.pointer();
        assert_eq!((px, py), (107, 59));
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_button_press_is_held_until_it_is_released() {
        let x = TestX::start();
        let plat = x.platform();
        plat.button(XT_BTN_PRESS, 1).expect("press");
        commit(&plat);
        assert!(
            x.pointer().2.contains(KeyButMask::BUTTON1),
            "button 1 should read as down between press and release"
        );
        plat.button(XT_BTN_RELEASE, 1).expect("release");
        commit(&plat);
        assert!(!x.pointer().2.contains(KeyButMask::BUTTON1));
    }

    /// The buttons a window watching input saw pressed, in order.
    fn buttons_pressed(x: &TestX) -> Vec<u8> {
        x.drain_events(Duration::from_millis(50))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::ButtonPress(b) => Some(b.detail),
                _ => None,
            })
            .collect()
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_positive_scroll_clicks_the_positive_button_once_per_step() {
        let x = TestX::start();
        let plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.warp(0, 0, 10, 10).expect("warp into the window");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.scroll_button(4, 5, 3, Deadline::UNBOUNDED)
            .expect("scroll");
        commit(&plat);
        assert_eq!(buttons_pressed(&x), vec![4, 4, 4], "window {win}");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_negative_scroll_clicks_the_negative_button_that_many_times() {
        // The magnitude, not the signed value: a step count that stayed negative would
        // produce an empty range and scroll nothing at all.
        let x = TestX::start();
        let plat = x.platform();
        x.window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.warp(0, 0, 10, 10).expect("warp into the window");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.scroll_button(4, 5, -2, Deadline::UNBOUNDED)
            .expect("scroll");
        commit(&plat);
        assert_eq!(buttons_pressed(&x), vec![5, 5]);
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_unmapped_keysym_is_an_invalid_key_not_some_other_keycode() {
        let x = TestX::start();
        let plat = x.platform();
        let err = plat
            .keycode_for(0x00ff_fffe)
            .expect_err("a keysym no key produces has no keycode");
        assert!(matches!(err, GlassError::InvalidKey(_)), "{err:?}");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_keysym_resolves_to_a_keycode_that_really_carries_it() {
        // Checked against the server's own table rather than a hardcoded keycode, which
        // varies with the keymap the runner happens to load.
        let x = TestX::start();
        let plat = x.platform();
        let (min, _max, per, keysyms) = x.keymap();

        let (kc, shifted) = plat.keycode_for(KEYSYM_A).expect("`a` must be typeable");
        let base = (kc as usize - min as usize) * per;
        assert!(!shifted, "plain `a` needs no Shift");
        assert_eq!(keysyms.get(base), Some(&KEYSYM_A));

        let (kc_upper, shifted_upper) = plat
            .keycode_for(KEYSYM_SHIFT_A)
            .expect("`A` must be typeable");
        assert!(shifted_upper, "`A` is the shifted column of its key");
        let base_upper = (kc_upper as usize - min as usize) * per;
        assert_eq!(keysyms.get(base_upper + 1), Some(&KEYSYM_SHIFT_A));
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn every_keysym_the_server_maps_can_be_resolved() {
        // A mapping request one keycode short still resolves nearly everything; only the
        // keys at the very end of the range go missing.
        let x = TestX::start();
        let plat = x.platform();
        let (min, max, per, keysyms) = x.keymap();
        let mut checked = 0;
        for kc in min..=max {
            let base = (kc as usize - min as usize) * per;
            for col in 0..per.min(2) {
                match keysyms.get(base + col) {
                    Some(&sym) if sym != 0 => {
                        assert!(
                            plat.keycode_for(sym).is_ok(),
                            "keysym 0x{sym:x} is on keycode {kc} but did not resolve"
                        );
                        checked += 1;
                    }
                    _ => {}
                }
            }
        }
        assert!(checked > 20, "the keymap looks empty ({checked} keysyms)");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn each_modifier_resolves_to_its_own_real_keycode() {
        let x = TestX::start();
        let plat = x.platform();
        use glass_core::keys::Modifier;
        let shift = plat.modifier_keycode(Modifier::Shift).expect("shift");
        let control = plat.modifier_keycode(Modifier::Control).expect("control");
        assert_eq!(
            shift,
            plat.keycode_for(KEYSYM_SHIFT_L).expect("shift sym").0
        );
        assert_ne!(
            shift, control,
            "distinct modifiers cannot share one keycode"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn tapping_a_keycode_presses_and_releases_it_at_the_focused_window() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().watching_input().create();
        plat.focus_window(win).expect("focus");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        let (kc, _) = plat.keycode_for(KEYSYM_A).expect("keycode");
        plat.tap_keycode(kc).expect("tap");
        commit(&plat);

        let events = x.drain_events(Duration::from_millis(50));
        let presses: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyPress(k) => Some(k.detail),
                _ => None,
            })
            .collect();
        let releases: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyRelease(k) => Some(k.detail),
                _ => None,
            })
            .collect();
        assert_eq!(presses, vec![kc], "{events:?}");
        assert_eq!(releases, vec![kc], "a tap must not leave the key down");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn pressing_modifiers_holds_them_down_and_releasing_lets_them_go() {
        let x = TestX::start();
        let plat = x.platform();
        use glass_core::keys::Modifier;
        let held = plat
            .press_mods(&[Modifier::Shift, Modifier::Control])
            .expect("press");
        commit(&plat);
        assert_eq!(
            held,
            vec![
                plat.modifier_keycode(Modifier::Shift).unwrap(),
                plat.modifier_keycode(Modifier::Control).unwrap()
            ],
            "the returned keycodes are what release_mods is given"
        );
        for kc in &held {
            assert!(x.key_is_down(*kc), "keycode {kc} should be held down");
        }
        plat.release_mods(&held).expect("release");
        commit(&plat);
        for kc in &held {
            assert!(!x.key_is_down(*kc), "keycode {kc} was left down");
        }
    }

    /// The keycodes a watching window saw pressed while `f` ran, with focus already on it.
    fn keys_pressed_during(
        x: &TestX,
        plat: &mut X11Platform,
        f: impl FnOnce(&mut X11Platform),
    ) -> Vec<u8> {
        let win = x.window().watching_input().create();
        plat.focus_window(win).expect("focus");
        commit(plat);
        let _ = x.drain_events(Duration::from_millis(50));
        f(plat);
        commit(plat);
        x.drain_events(Duration::from_millis(50))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyPress(k) => Some(k.detail),
                _ => None,
            })
            .collect()
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_uppercase_letter_is_typed_with_shift_held() {
        let x = TestX::start();
        let mut plat = x.platform();
        let shift = plat
            .modifier_keycode(glass_core::keys::Modifier::Shift)
            .expect("shift");
        let pressed = keys_pressed_during(&x, &mut plat, |p| {
            p.key_with_mods(KEYSYM_SHIFT_A, false, &[]).expect("type A");
        });
        assert!(
            pressed.contains(&shift),
            "`A` lives in the shifted column, so Shift must go down first: {pressed:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_lowercase_letter_is_typed_without_shift() {
        // Holding Shift for an unshifted key turns `a` into `A` at the application.
        let x = TestX::start();
        let mut plat = x.platform();
        let shift = plat
            .modifier_keycode(glass_core::keys::Modifier::Shift)
            .expect("shift");
        let pressed = keys_pressed_during(&x, &mut plat, |p| {
            p.key_with_mods(KEYSYM_A, false, &[]).expect("type a");
        });
        assert!(
            !pressed.contains(&shift),
            "Shift must not be pressed for a plain `a`: {pressed:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn focusing_a_window_makes_the_server_route_keys_to_it() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().create();
        plat.focus_window(win).expect("focus");
        commit(&plat);
        assert_eq!(x.focused(), win);
    }

    // --- the Platform surface --------------------------------------------------------

    /// A real child to stand in for a launched app. The pid accessors, window enumeration
    /// and the close ladder all read `self.child`, and `_NET_WM_PID` matching needs a pid
    /// that is genuinely in `/proc`. `sleep` leaves on its own, so a test that panics before
    /// its teardown cannot strand it.
    fn spawn_stand_in() -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        // As production spawns an app (`command.rs`). `reap_launch` signals `child.id()` as a
        // process GROUP; on a child that is not a group leader that pgid belongs to somebody
        // else, and teardown SIGKILLs an unrelated group.
        cmd.process_group(0);
        cmd.spawn().expect("the stand-in app should spawn")
    }

    /// The buttons and their positions a watching window saw, as `(detail, x, y)`.
    fn clicks_seen(x: &TestX) -> Vec<(u8, i16, i16)> {
        x.drain_events(Duration::from_millis(80))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::ButtonPress(b) => Some((b.detail, b.event_x, b.event_y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_app_pid_and_its_tree_are_empty_until_something_is_launched() {
        let x = TestX::start();
        let mut plat = x.platform();
        assert_eq!(plat.app_pid(), None);
        assert!(plat.app_pids().is_empty());

        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        assert_eq!(plat.app_pid(), Some(pid));
        assert!(
            plat.app_pids().contains(&pid),
            "the process tree must include the launched child itself"
        );
    }

    #[test]
    #[ignore = "starts a real X server and a private AT-SPI bus; needs Xvfb and at-spi2-core"]
    fn the_a11y_bus_address_is_the_private_buss_own_and_absent_without_one() {
        // The a11y reader connects to whatever this returns. Reporting nothing for a launch
        // that did start a bus leaves the tree unreadable; reporting something for one that
        // did not points the reader at the host's bus.
        let x = TestX::start();
        let mut plat = x.platform();
        assert_eq!(plat.a11y_bus_addr(), None);

        let bus = glass_dbus_linux::PrivateBus::start().expect("a private a11y bus should start");
        let expected = bus.a11y_bus_address().to_string();
        plat.dbus = Some(bus);
        assert_eq!(plat.a11y_bus_addr().as_deref(), Some(expected.as_str()));
        assert!(
            !expected.is_empty(),
            "an address that is blank reaches nothing"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn draining_logs_hands_over_the_buffer_and_leaves_it_empty() {
        let x = TestX::start();
        let mut plat = x.platform();
        plat.logs
            .lock()
            .expect("log buffer")
            .push((Stream::Stdout, "hello".to_string()));
        assert_eq!(
            plat.drain_logs(),
            vec![(Stream::Stdout, "hello".to_string())]
        );
        assert!(
            plat.drain_logs().is_empty(),
            "a drain must not hand back what it already returned"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_click_presses_the_mapped_button_at_the_requested_point() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.window = Some(win);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_pointer(&PointerEvent::Click {
            x: 30,
            y: 40,
            button: glass_core::MouseButton::Right,
            count: 2,
            modifiers: vec![],
        })
        .expect("click");

        assert_eq!(
            clicks_seen(&x),
            vec![(3, 30, 40), (3, 30, 40)],
            "a double right-click is button 3 twice, at the point asked for"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_scroll_clicks_the_wheel_buttons_for_each_axis() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.window = Some(win);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_pointer(&PointerEvent::Scroll {
            x: 5,
            y: 5,
            dx: 0,
            dy: -2,
            modifiers: vec![],
        })
        .expect("scroll");

        let buttons: Vec<u8> = clicks_seen(&x).into_iter().map(|(b, _, _)| b).collect();
        assert_eq!(buttons, vec![4, 4], "scrolling up twice is button 4 twice");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_drag_presses_at_the_start_and_releases_at_the_end() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.window = Some(win);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_pointer(&PointerEvent::Drag {
            from_x: 10,
            from_y: 10,
            to_x: 200,
            to_y: 150,
            button: glass_core::MouseButton::Left,
            modifiers: vec![],
            duration_ms: 40,
        })
        .expect("drag");

        let events = x.drain_events(Duration::from_millis(80));
        let press = events.iter().find_map(|e| match e {
            x11rb::protocol::Event::ButtonPress(b) => Some((b.event_x, b.event_y)),
            _ => None,
        });
        let release = events.iter().find_map(|e| match e {
            x11rb::protocol::Event::ButtonRelease(b) => Some((b.event_x, b.event_y)),
            _ => None,
        });
        assert_eq!(press, Some((10, 10)), "the button goes down at the start");
        assert_eq!(release, Some((200, 150)), "and comes up at the end");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_drag_with_a_modifier_holds_it_from_before_the_press_to_after_the_release() {
        // A constrained drag (shift to snap an axis, ctrl to copy) is a different gesture
        // from the same drag unmodified.
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.window = Some(win);
        plat.focus_window(win).expect("focus");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_pointer(&PointerEvent::Drag {
            from_x: 10,
            from_y: 10,
            to_x: 80,
            to_y: 80,
            button: glass_core::MouseButton::Left,
            modifiers: vec![glass_core::keys::Modifier::Shift],
            duration_ms: 40,
        })
        .expect("drag");

        let shift = plat
            .modifier_keycode(glass_core::keys::Modifier::Shift)
            .expect("shift");
        let events = x.drain_events(Duration::from_millis(120));
        assert!(
            events.iter().any(|e| matches!(
                e,
                x11rb::protocol::Event::KeyPress(k) if k.detail == shift
            )),
            "the modifier must go down for a modified drag: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                x11rb::protocol::Event::KeyRelease(k) if k.detail == shift
            )),
            "and must not be left held afterwards: {events:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_long_drag_reaches_the_window_while_it_is_still_running() {
        // Each step commits on its own. Held to one flush at the end, a client that renders
        // per frame — a browser — sees the pointer jump rather than drag.
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.window = Some(win);
        let _ = x.drain_events(Duration::from_millis(50));

        let arrived_early = std::thread::scope(|s| {
            s.spawn(|| {
                plat.send_pointer(&PointerEvent::Drag {
                    from_x: 10,
                    from_y: 10,
                    to_x: 300,
                    to_y: 300,
                    button: glass_core::MouseButton::Left,
                    modifiers: vec![],
                    duration_ms: 600,
                })
                .expect("drag");
            });
            // A third of the way in: far enough that the press and several motions are due,
            // far enough from the end that a loaded machine cannot blur the two.
            std::thread::sleep(Duration::from_millis(200));
            !x.drain_events(Duration::ZERO).is_empty()
        });

        assert!(
            arrived_early,
            "nothing had reached the window 200ms into a 600ms drag"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_multi_touch_gesture_is_refused_by_this_backend() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().create();
        plat.window = Some(win);
        let err = plat
            .send_pointer(&PointerEvent::Gesture {
                pointers: vec![glass_core::Segment {
                    from_x: 0,
                    from_y: 0,
                    to_x: 10,
                    to_y: 10,
                }],
                duration_ms: 10,
            })
            .expect_err("X11 has no multi-touch");
        assert!(err.to_string().contains("multi_touch"), "{err}");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn typed_text_sends_one_key_per_character() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().watching_input().create();
        plat.window = Some(win);
        plat.focus_window(win).expect("focus");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_key(&KeyEvent::Text("ab".to_string()))
            .expect("type");

        let pressed: Vec<u8> = x
            .drain_events(Duration::from_millis(120))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyPress(k) => Some(k.detail),
                _ => None,
            })
            .collect();
        let a = plat.keycode_for(KEYSYM_A).expect("a").0;
        let b = plat.keycode_for(0x62).expect("b").0;
        assert_eq!(pressed, vec![a, b], "each character in order");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_chord_holds_its_modifier_across_the_key() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().watching_input().create();
        plat.window = Some(win);
        plat.focus_window(win).expect("focus");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_key(&KeyEvent::Chord("ctrl+a".to_string()))
            .expect("chord");

        let control = plat
            .modifier_keycode(glass_core::keys::Modifier::Control)
            .expect("control");
        let a = plat.keycode_for(KEYSYM_A).expect("a").0;
        let pressed: Vec<u8> = x
            .drain_events(Duration::from_millis(80))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyPress(k) => Some(k.detail),
                _ => None,
            })
            .collect();
        assert_eq!(
            pressed,
            vec![control, a],
            "the modifier goes down before the key it modifies"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_window_op_moves_and_resizes_and_reports_the_result() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().at(5, 5).sized(100, 80).create();
        plat.window = Some(win);

        let resized = plat
            .window(&WindowOp::Resize {
                width: 321,
                height: 211,
            })
            .expect("resize");
        assert_eq!((resized.width, resized.height), (321, 211));

        let moved = plat.window(&WindowOp::Move { x: 40, y: 60 }).expect("move");
        assert_eq!((moved.x, moved.y), (40, 60));
        assert_eq!(
            (moved.width, moved.height),
            (321, 211),
            "a move must not resize"
        );

        let same = plat.window(&WindowOp::Geometry).expect("geometry");
        assert_eq!(same, moved, "a bare geometry read changes nothing");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn listing_windows_reports_the_apps_windows_and_marks_the_active_one() {
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        let main = x
            .window()
            .owned_by(pid)
            .named("main")
            .classed("app", "App")
            .at(3, 4)
            .sized(150, 120)
            .create();
        let other = x.window().owned_by(pid).named("other").create();
        plat.window = Some(main);

        let listed = plat.list_windows().expect("list");
        let entry = listed
            .iter()
            .find(|w| w.id == WindowId(main as u64))
            .expect("the active window must be listed");
        assert_eq!(entry.title.as_deref(), Some("main"));
        assert_eq!(entry.class.as_deref(), Some("App"));
        assert_eq!(
            entry.geometry,
            WindowGeometry {
                x: 3,
                y: 4,
                width: 150,
                height: 120
            }
        );
        assert!(entry.active, "the active window must be flagged as active");
        let sibling = listed
            .iter()
            .find(|w| w.id == WindowId(other as u64))
            .expect("the app's other window must be listed too");
        assert!(!sibling.active, "only one window is the active one");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn listing_windows_without_an_active_one_is_an_error_not_an_empty_list() {
        let x = TestX::start();
        let mut plat = x.platform();
        assert!(matches!(
            plat.list_windows(),
            Err(GlassError::WindowNotFound)
        ));
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn selecting_a_window_makes_it_active_and_refuses_one_that_is_not_the_apps() {
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        let mine = x.window().owned_by(pid).at(9, 8).sized(70, 60).create();
        let stranger = x.window().owned_by(999_999).create();
        plat.window = Some(mine);

        let geo = plat
            .select_window(WindowId(mine as u64))
            .expect("selecting the app's own window");
        assert_eq!((geo.x, geo.y, geo.width, geo.height), (9, 8, 70, 60));
        assert_eq!(plat.window, Some(mine));

        assert!(
            matches!(
                plat.select_window(WindowId(stranger as u64)),
                Err(GlassError::WindowNotFound)
            ),
            "a window outside the launched process tree is not selectable"
        );
        assert_eq!(
            plat.window,
            Some(mine),
            "a refused selection must leave the active window where it was"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn capturing_a_named_window_reads_that_windows_own_area() {
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        // Away from the origin, and filled: a capture that ignored the window's position would
        // read the root's black from (0,0) and still be the right size.
        let win = x
            .window()
            .owned_by(pid)
            .at(120, 60)
            .sized(48, 32)
            .filled_with(0x0000_00ff)
            .create();
        plat.window = Some(win);
        x.flush();

        let frame = plat
            .capture_window(WindowId(win as u64), None)
            .expect("capture");
        assert_eq!((frame.width, frame.height), (48, 32));
        let px = &frame.pixels[..4];
        assert_eq!(
            (px[0], px[1], px[2]),
            (0x00, 0x00, 0xff),
            "should read the window's own pixels, got {px:?}"
        );

        let stranger = x.window().owned_by(999_999).create();
        assert!(
            matches!(
                plat.capture_window(WindowId(stranger as u64), None),
                Err(GlassError::WindowNotFound)
            ),
            "a window the app does not own is not capturable"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_clipboard_round_trips_through_the_backend() {
        let x = TestX::start();
        let mut plat = x.platform();
        assert_eq!(
            plat.get_clipboard().expect("get"),
            "",
            "nothing has been copied on this display yet"
        );
        plat.set_clipboard("copied text").expect("set");
        assert_eq!(plat.get_clipboard().expect("get"), "copied text");
        plat.set_clipboard("replaced").expect("set again");
        assert_eq!(plat.get_clipboard().expect("get"), "replaced");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_second_copy_updates_the_owner_it_already_has() {
        // Re-taking the selection for every copy is visible to every other client on the
        // display: a clipboard manager watching ownership sees churn that did not happen.
        let x = TestX::start();
        let mut plat = x.platform();
        plat.set_clipboard("first").expect("set");
        let owner = x.clipboard_owner();
        assert_ne!(owner, x11rb::NONE, "the first copy takes the selection");

        plat.set_clipboard("second").expect("set again");
        assert_eq!(
            x.clipboard_owner(),
            owner,
            "a live owner should be handed the new text, not replaced"
        );
        assert_eq!(plat.get_clipboard().expect("get"), "second");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_copy_after_another_client_took_the_selection_starts_a_fresh_owner() {
        // glass's owner retires when something else takes CLIPBOARD. Handing the next copy to
        // that retired owner puts the text somewhere nothing serves it, and the paste that
        // follows returns the other application's clipboard instead.
        let x = TestX::start();
        let mut plat = x.platform();
        plat.set_clipboard("ours").expect("set");
        assert_eq!(plat.get_clipboard().expect("get"), "ours");

        let thief = x.take_clipboard();
        assert_eq!(
            x.clipboard_owner(),
            thief,
            "the test client should hold it now"
        );

        // Wait for glass's owner to RETIRE, not merely for the selection to change hands: it
        // notices SelectionClear on its own 10ms loop, and copying before it does hands the
        // text to an owner that no longer serves anything.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && plat.clipboard_owner.as_ref().is_some_and(|o| o.is_alive())
        {
            std::thread::sleep(Duration::from_millis(10));
        }

        plat.set_clipboard("ours again")
            .expect("set after losing it");
        assert_eq!(plat.get_clipboard().expect("get"), "ours again");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_copy_after_the_owner_thread_died_starts_a_fresh_owner() {
        // glass#371's symptom, in the terms an agent meets it: the copy reports success and
        // the paste comes back empty. The sibling above covers the SelectionClear route; this
        // covers a thread that died mid-life.
        let x = TestX::start();
        let mut plat = x.platform();
        plat.set_clipboard("ours").expect("set");
        assert_eq!(plat.get_clipboard().expect("get"), "ours");

        let owner_win = x.clipboard_owner();
        assert_ne!(owner_win, x11rb::NONE, "the first copy takes the selection");
        x.kill_client(owner_win);

        // Wait for the owner to RETIRE, not merely for the selection to change hands: copying
        // before it does is what put the text somewhere nothing served it.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && plat.clipboard_owner.as_ref().is_some_and(|o| o.is_alive())
        {
            std::thread::sleep(Duration::from_millis(10));
        }

        plat.set_clipboard("ours again")
            .expect("set after the owner died");
        assert_eq!(plat.get_clipboard().expect("get"), "ours again");
    }

    // --- the close ladder ------------------------------------------------------------

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn only_a_window_advertising_wm_delete_window_can_be_asked_to_close() {
        let x = TestX::start();
        let plat = x.platform();
        let protocols = plat.intern(b"WM_PROTOCOLS").expect("intern");
        let delete = plat.intern(b"WM_DELETE_WINDOW").expect("intern");

        let polite = x.window().accepting_delete().create();
        let silent = x.window().create();
        assert!(plat.accepts_delete(polite, protocols, delete));
        assert!(
            !plat.accepts_delete(silent, protocols, delete),
            "a window with no WM_PROTOCOLS cannot be asked, and must not be counted as asked"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_close_request_arrives_as_the_delete_client_message() {
        let x = TestX::start();
        let plat = x.platform();
        let protocols = plat.intern(b"WM_PROTOCOLS").expect("intern");
        let delete = plat.intern(b"WM_DELETE_WINDOW").expect("intern");
        let win = x.window().accepting_delete().create();

        plat.send_delete(win, protocols, delete).expect("send");
        commit(&plat);

        let message = x
            .next_event(Duration::from_secs(2))
            .expect("the window's owner should receive the request");
        match message {
            x11rb::protocol::Event::ClientMessage(m) => {
                assert_eq!(m.window, win);
                assert_eq!(m.type_, protocols);
                assert_eq!(
                    m.data.as_data32()[0],
                    delete,
                    "the protocol atom comes first, as ICCCM 4.2.8 specifies"
                );
            }
            other => panic!("expected a ClientMessage, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn asking_the_app_to_close_reaches_every_window_that_accepts_it() {
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        let polite = x.window().owned_by(pid).accepting_delete().create();
        plat.window = Some(polite);

        let asked = plat.request_close(pid);
        assert!(asked.any(), "the app's window should have been asked");
        assert!(
            x.next_event(Duration::from_secs(2)).is_some(),
            "the request must actually reach the window"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_app_with_no_window_is_not_reported_as_asked() {
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        assert!(
            !plat.request_close(pid).any(),
            "there was no window to ask, so nothing was asked"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_bounded_ask_reaches_the_window_through_its_own_connection() {
        // The ask runs on a second connection so the caller can abandon it without leaving
        // this backend's connection stopped mid-request.
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);
        let polite = x.window().owned_by(pid).accepting_delete().create();
        plat.window = Some(polite);

        assert!(plat.request_close_bounded(pid).any());
        assert!(
            x.next_event(Duration::from_secs(2)).is_some(),
            "the bounded ask must deliver the same request"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn stopping_the_app_reaps_the_child_and_forgets_it() {
        let x = TestX::start();
        let mut plat = x.platform();
        let child = spawn_stand_in();
        let pid = child.id();
        plat.child = Some(child);

        plat.stop_app().expect("stop");
        assert_eq!(plat.app_pid(), None, "the child must be forgotten");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "pid {pid} outlived the stop that was supposed to reap it"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn dropping_the_backend_reaps_an_app_that_was_never_stopped() {
        // Parity with the other backends: a panic-unwind or the process-exit backstop must
        // not leave the launched app running.
        let x = TestX::start();
        let pid = {
            let mut plat = x.platform();
            let child = spawn_stand_in();
            let pid = child.id();
            plat.child = Some(child);
            pid
        };
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "pid {pid} outlived the backend that launched it"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_launch_whose_app_never_appears_is_a_timeout_and_leaves_nothing_running() {
        let x = TestX::start();
        let mut plat = x.platform();
        let spec = AppSpec {
            build: None,
            run: vec!["sleep".to_string(), "30".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 250,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        };
        let err = plat
            .start_app(&spec)
            .expect_err("a command that maps no window cannot be launched");
        assert!(matches!(err, GlassError::Timeout(250)), "{err:?}");
        assert_eq!(
            plat.app_pid(),
            None,
            "a launch that failed must not leave its child behind"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn discovery_waits_out_its_timeout_before_giving_up() {
        // The budget has to be spent, not just reported: giving up at once fails every app
        // slower than instant. Timed around `discover_window`, so a spawn that fails under
        // load cannot read as a deadline that was not honoured.
        let x = TestX::start();
        let mut plat = x.platform();
        plat.child = Some(spawn_stand_in());
        let spec = AppSpec {
            build: None,
            run: vec!["sleep".to_string(), "30".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 400,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        };
        let started = Instant::now();
        let err = plat
            .discover_window(&spec)
            .expect_err("the stand-in never maps a window");
        let waited = started.elapsed();
        assert!(matches!(err, GlassError::Timeout(400)), "{err:?}");
        assert!(
            waited >= Duration::from_millis(350),
            "gave up after {waited:?}, which is not the 400ms budget it was given"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_untypable_character_is_reported_at_its_own_index() {
        // The index is all the error may carry — naming the character would put typed
        // content into the unredacted audit log — so it has to advance per character.
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().create();
        plat.window = Some(win);
        let err = plat
            .send_key(&KeyEvent::Text("a€".to_string()))
            .expect_err("€ has no X11 keysym");
        assert!(err.to_string().contains("index 1"), "{err}");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_scroll_with_a_modifier_holds_it_across_the_wheel() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x
            .window()
            .at(0, 0)
            .sized(400, 400)
            .watching_input()
            .create();
        plat.window = Some(win);
        plat.focus_window(win).expect("focus");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_pointer(&PointerEvent::Scroll {
            x: 5,
            y: 5,
            dx: 0,
            dy: -1,
            modifiers: vec![glass_core::keys::Modifier::Control],
        })
        .expect("scroll");

        let control = plat
            .modifier_keycode(glass_core::keys::Modifier::Control)
            .expect("control");
        let keys: Vec<u8> = x
            .drain_events(Duration::from_millis(150))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyPress(k) => Some(k.detail),
                _ => None,
            })
            .collect();
        assert!(
            keys.contains(&control),
            "a modified scroll must hold the modifier down: {keys:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_chord_whose_key_needs_shift_gets_shift_as_well_as_its_own_modifiers() {
        let x = TestX::start();
        let mut plat = x.platform();
        let win = x.window().watching_input().create();
        plat.window = Some(win);
        plat.focus_window(win).expect("focus");
        commit(&plat);
        let _ = x.drain_events(Duration::from_millis(50));

        plat.send_key(&KeyEvent::Chord("ctrl+A".to_string()))
            .expect("chord");

        let shift = plat
            .modifier_keycode(glass_core::keys::Modifier::Shift)
            .expect("shift");
        let keys: Vec<u8> = x
            .drain_events(Duration::from_millis(100))
            .into_iter()
            .filter_map(|e| match e {
                x11rb::protocol::Event::KeyPress(k) => Some(k.detail),
                _ => None,
            })
            .collect();
        assert!(
            keys.contains(&shift),
            "`A` sits in the shifted column, so ctrl+A is ctrl+shift+a: {keys:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_launch_whose_command_does_not_exist_reports_the_spawn_failure() {
        let x = TestX::start();
        let mut plat = x.platform();
        let spec = AppSpec {
            build: None,
            run: vec!["/nonexistent/glass-test-binary".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 250,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        };
        let err = plat.start_app(&spec).expect_err("nothing to spawn");
        assert!(matches!(err, GlassError::AppNotStarted(_)), "{err:?}");
    }

    // --- capture ---------------------------------------------------------------------

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_zero_area_region_is_rejected_before_a_doomed_get_image() {
        let x = TestX::start();
        let plat = x.platform();
        let geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        let flat = Region {
            x: 0,
            y: 0,
            width: 0,
            height: 40,
        };
        // Asserting the message, not just an error: clipping rejects an empty rectangle too,
        // so "it failed" would pass even if the zero-area check had stopped running.
        let flat_err = plat
            .resolve_capture_rect(&geo, Some(&flat))
            .expect_err("a region with no width has no pixels to read")
            .to_string();
        assert!(flat_err.contains("zero area"), "{flat_err}");
        let thin = Region {
            x: 0,
            y: 0,
            width: 40,
            height: 0,
        };
        let thin_err = plat
            .resolve_capture_rect(&geo, Some(&thin))
            .expect_err("a region with no height has no pixels to read")
            .to_string();
        assert!(thin_err.contains("zero area"), "{thin_err}");
        assert!(
            plat.resolve_capture_rect(&geo, None).is_ok(),
            "a window with area must still resolve"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_capture_reads_the_pixels_that_are_actually_on_screen() {
        // Everything in the decode path — the plane mask, the depth lookup and the
        // bytes-per-pixel it yields — is only observable in the pixels that come back.
        let x = TestX::start();
        let plat = x.platform();
        x.window()
            .at(0, 0)
            .sized(64, 64)
            .filled_with(0x00ff_0000)
            .create();
        x.flush();

        let frame = plat.capture_screen_rect(0, 0, 64, 64).expect("capture");
        assert_eq!((frame.width, frame.height), (64, 64));
        let px = &frame.pixels[..4];
        assert_eq!(
            (px[0], px[1], px[2]),
            (0xff, 0x00, 0x00),
            "the red window should read back as red, got {px:?}"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn scanning_for_one_window_finds_a_match_and_reports_none_otherwise() {
        let x = TestX::start();
        let plat = x.platform();
        let pid_atom = plat.intern(b"_NET_WM_PID").expect("intern");
        let list_atom = plat.intern(b"_NET_CLIENT_LIST").expect("intern");
        let win = x.window().owned_by(4242).create();
        assert_eq!(
            plat.scan_for_window(&[4242], pid_atom, list_atom, None)
                .expect("scan"),
            Some(win)
        );
        assert_eq!(
            plat.scan_for_window(&[OTHER_PID], pid_atom, list_atom, None)
                .expect("scan"),
            None
        );
    }
}

#[cfg(test)]
mod env_display_tests {
    use super::{DisplayTarget, display_target, normalize_display};

    #[test]
    fn unset_or_blank_spawns() {
        assert_eq!(display_target(None), DisplayTarget::Spawn);
        assert_eq!(display_target(Some("   ")), DisplayTarget::Spawn);
    }

    #[test]
    fn explicit_display_attaches() {
        assert_eq!(
            display_target(Some(":0")),
            DisplayTarget::Attach(":0".into())
        );
        assert_eq!(
            display_target(Some(":42")),
            DisplayTarget::Attach(":42".into())
        );
        assert_eq!(
            display_target(Some("42")),
            DisplayTarget::Attach(":42".into())
        );
    }

    #[test]
    fn normalize_adds_leading_colon() {
        assert_eq!(normalize_display("42"), ":42");
        assert_eq!(normalize_display(":42"), ":42");
    }
}
