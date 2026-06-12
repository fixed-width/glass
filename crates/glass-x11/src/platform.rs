use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region, Result, Stream,
    WindowGeometry, WindowHint, WindowId, WindowInfo, WindowOp,
};
use glass_proc_linux::proc_tree_pids;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::ErrorKind;
use x11rb::rust_connection::RustConnection;

const XT_MOTION: u8 = 6; // MotionNotify
const XT_BTN_PRESS: u8 = 4; // ButtonPress
const XT_BTN_RELEASE: u8 = 5; // ButtonRelease
const XT_KEY_PRESS: u8 = 2; // KeyPress
const XT_KEY_RELEASE: u8 = 3; // KeyRelease

use crate::command::build_command;

type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// The Linux/X11 backend. Connects to an X display, launches and locates the
/// target app's top-level window, and drives it via X requests + XTEST.
pub struct X11Platform {
    conn: RustConnection,
    #[expect(dead_code, reason = "captured from the X setup for completeness; not currently read")]
    screen_num: usize,
    root: Window,
    display: String,
    child: Option<Child>,
    window: Option<Window>,
    logs: LogSink,
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
fn normalize_display(d: &str) -> String {
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
        let abs = self.conn.translate_coordinates(win, self.root, 0, 0)?.reply()?;
        Ok(WindowGeometry {
            x: abs.dst_x as i32,
            y: abs.dst_y as i32,
            width: geo.width as u32,
            height: geo.height as u32,
        })
    }

    /// Intern an atom by name (small helper for the multi-window scans).
    fn intern(&self, name: &[u8]) -> Result<Atom> {
        Ok(self
            .conn
            .intern_atom(false, name)
            .map_err(|e| GlassError::Backend(format!("intern {}: {e}", String::from_utf8_lossy(name))))?
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
        for win in self.client_list_windows(client_list_atom).into_iter().chain(root_children) {
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
        let dbus_addr = self.dbus.as_ref().map(|b| b.session_bus_address());
        let a11y_dir = self.dbus.as_ref().map(|b| b.runtime_dir().to_path_buf());
        let mut cmd = build_command(spec, &self.display, dbus_addr, a11y_dir.as_deref());
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| GlassError::AppNotStarted(format!("spawn {:?}: {e}", spec.run)))?;
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, Stream::Stdout, self.logs.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, Stream::Stderr, self.logs.clone());
        }
        self.child = Some(child);
        Ok(())
    }

    /// Kill and reap the launched child (if any) and forget its window. Used by
    /// `stop_app` and by `start_app`'s failure path so a launch that never finds
    /// a window does not orphan the process.
    fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
        }
        self.window = None;
        // Drop the private a11y bus, reaping its dbus-daemon / at-spi children. Also
        // covers `start_app`'s failure path (which calls `kill_child`), so a launch
        // that never finds a window doesn't leave the bus running until Drop.
        self.dbus = None;
        if let Some(owner) = self.clipboard_owner.take() {
            owner.stop();
        }
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
        let pid_atom = self
            .conn
            .intern_atom(false, b"_NET_WM_PID")
            .map_err(|e| GlassError::Backend(format!("intern _NET_WM_PID: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("intern reply: {e}")))?
            .atom;
        let client_list_atom = self
            .conn
            .intern_atom(false, b"_NET_CLIENT_LIST")
            .map_err(|e| GlassError::Backend(format!("intern _NET_CLIENT_LIST: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("intern reply: {e}")))?
            .atom;

        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.max(1));
        loop {
            // Re-collect the pid set each iteration: a sandboxed launch's bwrap
            // child (the real app) appears in /proc shortly after bwrap starts.
            let pids: Vec<u32> = root_pid
                .map(proc_tree_pids)
                .unwrap_or_default();
            if let Some(win) =
                self.scan_for_window(&pids, pid_atom, client_list_atom, spec.window_hint.as_ref())?
            {
                self.window = Some(win);
                return Ok(win);
            }
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(GlassError::AppExited(status.code()));
                }
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
        for win in self.client_list_windows(client_list_atom).into_iter().chain(root_children) {
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
        if !pids.is_empty() {
            if let Some(reply) = self
                .conn
                .get_property(false, win, pid_atom, AtomEnum::CARDINAL, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                if let Some(win_pid) = reply.value32().and_then(|mut v| v.next()) {
                    if pids.contains(&win_pid) {
                        return Ok(true);
                    }
                }
            }
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

    fn scroll_button(&self, pos_btn: u8, neg_btn: u8, delta: i32) -> Result<()> {
        let (btn, times) = if delta >= 0 { (pos_btn, delta) } else { (neg_btn, -delta) };
        for _ in 0..times {
            self.button(XT_BTN_PRESS, btn)?;
            self.button(XT_BTN_RELEASE, btn)?;
        }
        Ok(())
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
        let per = mapping.keysyms_per_keycode as usize;
        for kc in min..=max {
            let base = (kc as usize - min as usize) * per;
            if mapping.keysyms.get(base) == Some(&keysym) {
                return Ok((kc, false));
            }
            if per > 1 && mapping.keysyms.get(base + 1) == Some(&keysym) {
                return Ok((kc, true));
            }
        }
        Err(GlassError::InvalidKey(format!("no keycode for keysym 0x{keysym:x}")))
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
            .xtest_fake_input(XT_KEY_PRESS, keycode, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| GlassError::Backend(format!("xtest key press: {e}")))?;
        self.conn
            .xtest_fake_input(XT_KEY_RELEASE, keycode, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
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

    fn key_with_mods(&self, keysym: u32, extra_shift: bool, mods: &[glass_core::keys::Modifier]) -> Result<()> {
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
    fn focus_window(&self, win: Window) -> Result<()> {
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
            .map_err(|e| GlassError::Backend(format!("raise: {e}")))?;
        self.conn
            .set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .map_err(|e| GlassError::Backend(format!("set_input_focus: {e}")))?;
        self.conn
            .flush()
            .map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
        Ok(())
    }
}

fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, stream: Stream, sink: LogSink) {
    std::thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            match line {
                Ok(text) => sink.lock().expect("log sink mutex").push((stream, text)),
                Err(_) => break,
            }
        }
    });
}

// The process-tree walk (`/proc`-based) that maps the spawned child to the
// real app's descendants now lives in the shared `glass-proc-linux` crate
// (`proc_tree_pids`), used by both the X11 and Wayland backends.

impl Platform for X11Platform {
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        if spec.sandbox != glass_core::SandboxLevel::Off {
            if let glass_sandbox_linux::Availability::Unavailable(why) = glass_sandbox_linux::availability() {
                return Err(GlassError::SandboxUnavailable(format!(
                    "{why}. Install bubblewrap / enable unprivileged user namespaces, or pass \
                     sandbox:\"off\" (GLASS_SANDBOX=off) to run unconfined. See `glass-mcp doctor`."
                )));
            }
        }
        glass_sandbox_linux::run_build(spec)?;
        // Opt-in private, isolated a11y bus (its own XDG_RUNTIME_DIR — never touches the
        // host /run/user/UID/at-spi/) so the launched app publishes an AT-SPI tree. Only
        // when the caller asked for it (`a11y: true`). The caller explicitly opted into
        // a11y, so a bus that can't start now fails the launch with the real cause rather
        // than silently degrading — nothing is leaked on this early return (`PrivateBus::start`
        // reaps its own partial children on failure, and no app child / per-launch resource
        // has been spawned yet at this `?`). For sandboxed launches, `spawn` binds the private
        // bus dir into the bwrap run so the confined app can reach the advertised
        // unix:path= sockets.
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
        match self.discover_window(spec).and_then(|_| self.window_geometry()) {
            Ok(geo) => {
                // Give the launched window keyboard focus so synthetic keys reach
                // it (no WM in the headless Xvfb assigns focus). Best-effort: a
                // focus failure must not fail an otherwise-successful launch.
                if let Some(win) = self.window {
                    if let Err(e) = self.focus_window(win) {
                        eprintln!(
                            "glass: focus-on-launch failed (keys may not reach the window): {e}"
                        );
                    }
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

    fn stop_app(&mut self) -> Result<()> {
        self.kill_child();
        Ok(())
    }

    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        let win = self.require_window()?;
        let geo = self.window_geometry()?;
        let (cx, cy, w, h) = match region {
            Some(r) => (r.x, r.y, r.width, r.height),
            None => (0, 0, geo.width, geo.height),
        };
        if w == 0 || h == 0 {
            return Err(GlassError::CaptureFailed("window has zero area".into()));
        }
        let image = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, win, cx as i16, cy as i16, w as u16, h as u16, !0u32)
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

    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        let origin = self.window_geometry()?;
        let (ox, oy) = (origin.x, origin.y);
        match *event {
            PointerEvent::Move { x, y } => self.warp(ox, oy, x, y)?,
            PointerEvent::Scroll { x, y, dx, dy, ref modifiers } => {
                self.warp(ox, oy, x, y)?;
                let kcs = self.press_mods(modifiers)?;
                // 4=up,5=down,6=left,7=right; click |delta| times.
                self.scroll_button(5, 4, dy)?;
                self.scroll_button(7, 6, dx)?;
                self.release_mods(&kcs)?;
            }
            PointerEvent::Click { x, y, button, count, ref modifiers } => {
                self.warp(ox, oy, x, y)?;
                let kcs = self.press_mods(modifiers)?;
                let b = button_number(button);
                for _ in 0..count.max(1) {
                    self.button(XT_BTN_PRESS, b)?;
                    self.button(XT_BTN_RELEASE, b)?;
                }
                self.release_mods(&kcs)?;
            }
            PointerEvent::Drag { from_x, from_y, to_x, to_y, button, ref modifiers, duration_ms } => {
                let b = button_number(button);
                let (waypoints, step) = glass_core::drag_schedule((from_x, from_y), (to_x, to_y), duration_ms);
                self.warp(ox, oy, waypoints[0].0, waypoints[0].1)?;
                let kcs = self.press_mods(modifiers)?;
                self.button(XT_BTN_PRESS, b)?;
                self.conn.flush().map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
                for &(px, py) in &waypoints[1..] {
                    std::thread::sleep(step);
                    self.warp(ox, oy, px, py)?;
                    self.conn.flush().map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
                }
                self.button(XT_BTN_RELEASE, b)?;
                self.release_mods(&kcs)?;
            }
        }
        self.conn.flush().map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
        Ok(())
    }

    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        match event {
            KeyEvent::Text(text) => {
                for c in text.chars() {
                    let keysym = glass_core::keys::keysym_for_char(c)
                        .ok_or_else(|| GlassError::InvalidKey(format!("untypable char {c:?}")))?;
                    self.key_with_mods(keysym, false, &[])?;
                }
            }
            KeyEvent::Chord(chord) => {
                let (mods, keysym) = glass_core::keys::parse_chord(chord)?;
                self.key_with_mods(keysym, false, &mods)?;
            }
        }
        self.conn.flush().map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
        Ok(())
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
        self.conn.flush().map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
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
    if let Some(title) = &hint.title {
        if name == Some(title.as_str()) {
            return true;
        }
    }
    if let Some(want) = &hint.class {
        if let Some((instance, class)) = class {
            if instance == want.as_str() || class == want.as_str() {
                return true;
            }
        }
    }
    false
}

impl Drop for X11Platform {
    /// Reap the launched app on drop — parity with the Wayland/Windows backends, so
    /// a backend dropped without an explicit `stop_app()` (panic-unwind, or the
    /// process-exit backstop path) does not orphan its app. `kill_child` uses
    /// `self.child.take()`, so this is idempotent with `stop_app`. Field order then
    /// drops `xvfb`, tearing down any private display we spawned.
    fn drop(&mut self) {
        self.kill_child(); // also stops clipboard_owner
        // Redundant safety: kill_child already calls take(), but be explicit
        // in case clipboard_owner was set after the last kill_child call.
        if let Some(owner) = self.clipboard_owner.take() {
            owner.stop();
        }
    }
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
    use super::hint_matches;
    use glass_core::WindowHint;

    // proc_tree_pids / collect_descendants moved to the `glass-proc-linux` crate
    // (tested there).

    fn hint(title: Option<&str>, class: Option<&str>) -> WindowHint {
        WindowHint { title: title.map(Into::into), class: class.map(Into::into) }
    }

    #[test]
    fn matches_title_exactly() {
        let h = hint(Some("Calculator"), None);
        assert!(hint_matches(Some("Calculator"), None, &h));
        assert!(!hint_matches(Some("Calc"), None, &h), "title is an exact match, not substring");
    }

    #[test]
    fn class_hint_matches_either_instance_or_class() {
        // xcalc's WM_CLASS is ("xcalc", "XCalc") — either should satisfy the hint.
        assert!(hint_matches(None, Some(("xcalc", "XCalc")), &hint(None, Some("XCalc"))));
        assert!(hint_matches(None, Some(("xcalc", "XCalc")), &hint(None, Some("xcalc"))));
        assert!(!hint_matches(None, Some(("xcalc", "XCalc")), &hint(None, Some("gedit"))));
    }

    #[test]
    fn class_hint_does_not_match_when_window_has_no_class() {
        assert!(!hint_matches(Some("whatever"), None, &hint(None, Some("XCalc"))));
    }

    #[test]
    fn either_title_or_class_can_match() {
        // title wrong but class right still matches (OR semantics).
        let h = hint(Some("Nope"), Some("XCalc"));
        assert!(hint_matches(Some("Calculator"), Some(("xcalc", "XCalc")), &h));
    }

    #[test]
    fn empty_hint_never_matches() {
        let h = hint(None, None);
        assert!(!hint_matches(Some("anything"), Some(("a", "b")), &h));
    }
}

#[cfg(test)]
mod env_display_tests {
    use super::{display_target, normalize_display, DisplayTarget};

    #[test]
    fn unset_or_blank_spawns() {
        assert_eq!(display_target(None), DisplayTarget::Spawn);
        assert_eq!(display_target(Some("   ")), DisplayTarget::Spawn);
    }

    #[test]
    fn explicit_display_attaches() {
        assert_eq!(display_target(Some(":0")), DisplayTarget::Attach(":0".into()));
        assert_eq!(display_target(Some(":42")), DisplayTarget::Attach(":42".into()));
        assert_eq!(display_target(Some("42")), DisplayTarget::Attach(":42".into()));
    }

    #[test]
    fn normalize_adds_leading_colon() {
        assert_eq!(normalize_display("42"), ":42");
        assert_eq!(normalize_display(":42"), ":42");
    }
}
