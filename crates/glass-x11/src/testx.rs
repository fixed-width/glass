//! A private X server and real client windows, for the tests that need actual X protocol.
//!
//! The backend's methods talk to a display, so most of them have no seam to fake at: there is
//! no subprocess to stand in for and no trait to implement. An `Xvfb` starts in ~45ms, though,
//! so the tests drive a real server and put real windows in front of the code instead.
//!
//! The windows belong to a *second* connection. A window the backend created itself would be
//! matched by ids it already holds, which is not what enumeration has to do.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::platform::X11Platform;
use crate::xvfb::Xvfb;

/// A private headless display, plus a client on it that is not the code under test.
pub(crate) struct TestX {
    xvfb: Xvfb,
    conn: RustConnection,
    root: Window,
    root_visual: Visualid,
}

impl TestX {
    /// A display at the size production defaults to.
    pub(crate) fn start() -> TestX {
        TestX::sized("1280x800x24")
    }

    /// A display at `screen` (`WxHxDepth`), for the capture tests, which are about what
    /// happens at the display's edges.
    pub(crate) fn sized(screen: &str) -> TestX {
        let xvfb = Xvfb::start(screen).expect("a private Xvfb should start");
        let (conn, screen_num) =
            x11rb::connect(Some(&xvfb.display)).expect("the test client should connect");
        let setup = &conn.setup().roots[screen_num];
        let (root, root_visual) = (setup.root, setup.root_visual);
        TestX {
            xvfb,
            conn,
            root,
            root_visual,
        }
    }

    pub(crate) fn display(&self) -> &str {
        &self.xvfb.display
    }

    pub(crate) fn root(&self) -> Window {
        self.root
    }

    /// A backend attached to this display.
    pub(crate) fn platform(&self) -> X11Platform {
        X11Platform::connect(Some(self.display())).expect("the backend should connect")
    }

    pub(crate) fn intern(&self, name: &[u8]) -> Atom {
        self.conn
            .intern_atom(false, name)
            .expect("intern")
            .reply()
            .expect("intern reply")
            .atom
    }

    /// Start describing a window. Nothing is created until [`TestWindow::create`].
    pub(crate) fn window(&self) -> TestWindow<'_> {
        TestWindow {
            x: self,
            name: None,
            class: None,
            pid: None,
            rect: (0, 0, 200, 100),
            accepts_delete: false,
            map: true,
            events: EventMask::NO_EVENT,
            background: None,
        }
    }

    /// Where the pointer is, in root coordinates, and which buttons are down — how a test
    /// sees the effect of XTEST motion and button events.
    pub(crate) fn pointer(&self) -> (i16, i16, KeyButMask) {
        let p = self
            .conn
            .query_pointer(self.root)
            .expect("query_pointer")
            .reply()
            .expect("query_pointer reply");
        (p.root_x, p.root_y, p.mask)
    }

    /// Whether `keycode` is currently held down, read from the server's own keyboard state.
    pub(crate) fn key_is_down(&self, keycode: u8) -> bool {
        let keys = self
            .conn
            .query_keymap()
            .expect("query_keymap")
            .reply()
            .expect("query_keymap reply")
            .keys;
        keys[keycode as usize / 8] & (1 << (keycode % 8)) != 0
    }

    /// The window the server currently sends key events to.
    pub(crate) fn focused(&self) -> Window {
        self.conn
            .get_input_focus()
            .expect("get_input_focus")
            .reply()
            .expect("get_input_focus reply")
            .focus
    }

    /// The whole keyboard mapping, as `keycode_for` reads it: `(min, max, per, keysyms)`.
    pub(crate) fn keymap(&self) -> (u8, u8, usize, Vec<u32>) {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let m = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .expect("get_keyboard_mapping")
            .reply()
            .expect("keyboard mapping reply");
        (min, max, m.keysyms_per_keycode as usize, m.keysyms)
    }

    /// Drain and return every event queued for this client.
    pub(crate) fn drain_events(&self, settle: Duration) -> Vec<Event> {
        std::thread::sleep(settle);
        let mut out = Vec::new();
        while let Some(e) = self.conn.poll_for_event().expect("poll_for_event") {
            out.push(e);
        }
        out
    }

    /// Publish `wins` as the root's `_NET_CLIENT_LIST`, the list a window manager maintains.
    /// Xvfb runs without one, so a test that is about that path has to write it.
    pub(crate) fn set_client_list(&self, wins: &[Window]) {
        let atom = self.intern(b"_NET_CLIENT_LIST");
        self.conn
            .change_property32(PropMode::REPLACE, self.root, atom, AtomEnum::WINDOW, wins)
            .expect("set _NET_CLIENT_LIST");
        self.flush();
    }

    pub(crate) fn destroy(&self, win: Window) {
        self.conn.destroy_window(win).expect("destroy_window");
        self.flush();
    }

    /// Block until the server has processed everything sent so far, so a subsequent read on
    /// the backend's own connection cannot race the writes above it.
    pub(crate) fn flush(&self) {
        self.conn.sync().expect("sync");
    }

    /// The next event this client receives, or `None` once `within` elapses. Used to prove a
    /// message the backend claims to have sent actually arrived.
    pub(crate) fn next_event(&self, within: Duration) -> Option<Event> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match self.conn.poll_for_event().expect("poll_for_event") {
                Some(e) => return Some(e),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        None
    }
}

/// A window to create on the test display. Only the properties a test is about need naming.
pub(crate) struct TestWindow<'a> {
    x: &'a TestX,
    name: Option<String>,
    class: Option<(String, String)>,
    pid: Option<u32>,
    rect: (i16, i16, u16, u16),
    accepts_delete: bool,
    map: bool,
    events: EventMask,
    background: Option<u32>,
}

impl TestWindow<'_> {
    /// Selects the input events XTEST synthesises, so a test can read back what the backend
    /// actually sent rather than only that the call returned `Ok`.
    pub(crate) fn watching_input(mut self) -> Self {
        self.events = EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE;
        self
    }

    /// Fills the window with `pixel`, giving a capture something other than the root's black
    /// to read.
    pub(crate) fn filled_with(mut self, pixel: u32) -> Self {
        self.background = Some(pixel);
        self
    }

    /// Sets `WM_NAME`, what `window_name` reads.
    pub(crate) fn named(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// Sets `WM_CLASS` as the NUL-separated `instance\0class\0` pair ICCCM specifies.
    pub(crate) fn classed(mut self, instance: &str, class: &str) -> Self {
        self.class = Some((instance.to_string(), class.to_string()));
        self
    }

    /// Sets `_NET_WM_PID`, the property window enumeration filters the app's windows by.
    pub(crate) fn owned_by(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub(crate) fn at(mut self, x: i16, y: i16) -> Self {
        self.rect.0 = x;
        self.rect.1 = y;
        self
    }

    pub(crate) fn sized(mut self, w: u16, h: u16) -> Self {
        self.rect.2 = w;
        self.rect.3 = h;
        self
    }

    /// Advertises `WM_DELETE_WINDOW` in `WM_PROTOCOLS`, making the window one the close
    /// request can be sent to.
    pub(crate) fn accepting_delete(mut self) -> Self {
        self.accepts_delete = true;
        self
    }

    /// Leaves the window unmapped — present in the tree but not on screen.
    pub(crate) fn unmapped(mut self) -> Self {
        self.map = false;
        self
    }

    pub(crate) fn create(self) -> Window {
        let conn = &self.x.conn;
        let win = conn.generate_id().expect("generate_id");
        let (x, y, w, h) = self.rect;
        let mut aux = CreateWindowAux::new().event_mask(self.events);
        if let Some(pixel) = self.background {
            aux = aux.background_pixel(pixel);
        }
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            self.x.root,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            self.x.root_visual,
            &aux,
        )
        .expect("create_window");

        if let Some(name) = &self.name {
            conn.change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                name.as_bytes(),
            )
            .expect("WM_NAME");
        }
        if let Some((instance, class)) = &self.class {
            let mut value = Vec::new();
            value.extend_from_slice(instance.as_bytes());
            value.push(0);
            value.extend_from_slice(class.as_bytes());
            value.push(0);
            conn.change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                &value,
            )
            .expect("WM_CLASS");
        }
        if let Some(pid) = self.pid {
            let atom = self.x.intern(b"_NET_WM_PID");
            conn.change_property32(PropMode::REPLACE, win, atom, AtomEnum::CARDINAL, &[pid])
                .expect("_NET_WM_PID");
        }
        if self.accepts_delete {
            let protocols = self.x.intern(b"WM_PROTOCOLS");
            let delete = self.x.intern(b"WM_DELETE_WINDOW");
            conn.change_property32(PropMode::REPLACE, win, protocols, AtomEnum::ATOM, &[delete])
                .expect("WM_PROTOCOLS");
        }
        if self.map {
            conn.map_window(win).expect("map_window");
        }
        self.x.flush();
        win
    }
}
