//! A private headless sway, and a real client on it, for the tests that need an actual compositor.
//!
//! `start_app` spawns the compositor itself, so unlike the X11 harness there is no server to
//! attach to. Tests drive the production path; this supplies what it needs — an app for sway to
//! launch, and a view of the session that is not the code under test.
//!
//! The app is this test binary re-executed (`--exact testw::fixture --ignored`), so sway `exec`s a
//! real external process as production does — and a *native Wayland* one, which nothing else here
//! covers: `glass-testapp` depends only on x11rb and arrives through Xwayland.
//!
//! The observer is a second sway IPC connection. Reading state back through the backend's own
//! connection would let a mutant that breaks a read hide behind the matching broken write.

use std::time::{Duration, Instant};

use glass_core::{AppSpec, Platform, SandboxLevel};

use crate::platform::WaylandPlatform;
use crate::swayipc::{Ipc, Window as SwayWindow};

/// What the re-executed fixture should put on screen: `title:app_id:WxH`, comma-separated.
const WINDOWS: &str = "GLASS_TESTW_WINDOWS";
/// Set to make the fixture ignore `xdg_toplevel.close` — an app with no shutdown path, which
/// teardown has to fall back to signalling.
const IGNORES_CLOSE: &str = "GLASS_TESTW_IGNORES_CLOSE";
/// Set to make the fixture an X11 client, reaching the compositor through Xwayland.
const USE_X11: &str = "GLASS_TESTW_X11";
/// Printed by the fixture once every window is mapped, so a log test has a line it can wait for.
pub(crate) const READY_LINE: &str = "testw: windows mapped";
/// Printed by the fixture the moment it reaches the compositor, before it maps anything. One per
/// launched process, so a test can count attempts without waiting for a window.
pub(crate) const CONNECTED_LINE: &str = "testw: connected";
/// Printed by the fixture when it acts on a close request, rather than being signalled.
pub(crate) const CLOSING_LINE: &str = "testw: closing";

/// How long a harness wait may spin before the test fails outright, and how long a launch gives
/// the compositor. Both bound a hang; neither paces anything, and a passing test on an idle
/// machine reaches neither.
///
/// Do not cut these to make mutants fail faster. At 5s/8s, tests began missing the budget under
/// the load of a sweep through contention alone — and cargo-mutants records a spurious failure as
/// a *kill*, so tight budgets inflate the caught count and make consecutive sweeps of identical
/// code disagree. A hang is graded a timeout, which this repo already treats as a detection.
const SETTLE_BUDGET: Duration = Duration::from_secs(20);

/// See [`SETTLE_BUDGET`]. `start_app` retries the bring-up, and spends this twice per attempt —
/// once waiting for the socket, once for a window — so a failing launch costs four times it.
const LAUNCH_BUDGET_MS: u64 = 20_000;

/// A launched session: the backend under test, plus an independent view of the same compositor.
///
/// `#[must_use]`: dropping it tears the compositor down at once, so a test that meant to hold one
/// fails for an unrelated reason.
#[must_use]
pub(crate) struct Session {
    platform: WaylandPlatform,
    ipc: Ipc,
}

/// Describes the session to launch. Nothing runs until [`Launch::start`].
#[must_use]
pub(crate) struct Launch {
    windows: Vec<String>,
    timeout_ms: u64,
    a11y: bool,
    env: Vec<(String, String)>,
}

impl Launch {
    /// One 320x240 window titled `glass-testw`.
    pub(crate) fn new() -> Launch {
        Launch {
            windows: vec!["glass-testw:glass-testw:320x240".into()],
            timeout_ms: LAUNCH_BUDGET_MS,
            a11y: false,
            env: Vec::new(),
        }
    }

    /// Replace the window list. Each entry is `title:app_id:WxH`.
    /// Replace the window list. Parsed here, so a malformed `title:app_id:WxH` fails on this line
    /// rather than inside the launched fixture.
    pub(crate) fn windows(mut self, windows: &[&str]) -> Launch {
        self.windows = windows.iter().map(|w| Spec::parse(w).to_entry()).collect();
        self
    }

    pub(crate) fn timeout_ms(mut self, ms: u64) -> Launch {
        self.timeout_ms = ms;
        self
    }

    pub(crate) fn env(mut self, k: &str, v: &str) -> Launch {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    /// Launch with accessibility, so the session brings up its own private bus.
    pub(crate) fn with_a11y(mut self) -> Launch {
        self.a11y = true;
        self
    }

    /// An app with no shutdown path: it never acts on a close request.
    pub(crate) fn ignoring_close(self) -> Launch {
        self.env(IGNORES_CLOSE, "1")
    }

    /// Reach the compositor through Xwayland. sway starts it lazily, so without an X client the
    /// session has no X11 side at all.
    pub(crate) fn through_xwayland(self) -> Launch {
        self.env(USE_X11, "1")
    }

    /// The `AppSpec` this launch describes, without starting anything — for the tests that drive
    /// `start_app` themselves.
    pub(crate) fn spec(&self) -> AppSpec {
        let exe = std::env::current_exe().expect("the test binary should have a path");
        let mut env = vec![(WINDOWS.to_string(), self.windows.join(","))];
        env.extend(self.env.iter().cloned());
        AppSpec {
            build: None,
            run: vec![
                exe.to_string_lossy().into_owned(),
                "--exact".into(),
                "testw::fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            cwd: None,
            env,
            window_hint: None,
            timeout_ms: self.timeout_ms,
            // Every session test launches unconfined. `ensure_sandbox_available` is covered
            // directly, with a probe that panics if it is called.
            sandbox: SandboxLevel::Off,
            a11y: self.a11y,
        }
    }

    /// Bring the session up and wait until the fixture has mapped every window.
    ///
    /// A precondition, not a convenience: input delivered before a surface is mapped goes
    /// nowhere, and the wait also clears the ready line from the sink so a later `wait_for_log`
    /// sees only what the test itself caused.
    pub(crate) fn start_mapped(self) -> Session {
        let mut s = self.start();
        s.wait_for_log(READY_LINE);
        s
    }

    /// Bring the session up. Panics if it does not start — a test that wants the failure calls
    /// `start_app` itself with [`Launch::spec`].
    pub(crate) fn start(self) -> Session {
        let spec = self.spec();
        let mut platform = WaylandPlatform::new().expect("a sway should be discoverable");
        if let Err(e) = platform.start_app(&spec) {
            // What the compositor and the app said on the way down. A bare `Timeout(..)` says
            // only that no window arrived, which is the one thing already known.
            let said: Vec<String> = platform.drain_logs().into_iter().map(|(_, l)| l).collect();
            panic!("the fixture app should start: {e}\nthe session said: {said:#?}");
        }
        let dir = platform
            .session_runtime_dir()
            .expect("a started session has a runtime dir")
            .to_path_buf();
        let ipc = Ipc::connect(&dir).expect("the observer should reach sway IPC");
        Session { platform, ipc }
    }
}

impl Session {
    pub(crate) fn platform(&mut self) -> &mut WaylandPlatform {
        &mut self.platform
    }

    /// The compositor's window list, read over a connection the backend does not own.
    pub(crate) fn windows(&mut self) -> Vec<SwayWindow> {
        self.ipc.windows().expect("the observer should reach sway")
    }

    /// The session's private runtime dir — what identifies its own Xwayland among any other X
    /// servers on this machine.
    pub(crate) fn runtime_dir(&self) -> std::path::PathBuf {
        self.platform
            .session_runtime_dir()
            .expect("a started session has a runtime dir")
            .to_path_buf()
    }

    /// The session's wayland socket — what the clipboard opens its own connection to.
    pub(crate) fn wayland_socket(&self) -> std::path::PathBuf {
        crate::platform::find_wayland_socket(&self.runtime_dir())
            .expect("the session has a wayland socket")
    }

    /// The title of the window sway currently reports as focused.
    pub(crate) fn focused_title(&mut self) -> Option<String> {
        self.windows()
            .into_iter()
            .find(|w| w.focused)
            .and_then(|w| w.title)
    }

    /// Spin until the app has echoed a line containing `needle`, and return every line drained
    /// along the way. Drains are destructive, so the caller gets the accumulated set rather than
    /// whatever happened to arrive in the last poll.
    pub(crate) fn wait_for_log(&mut self, needle: &str) -> Vec<String> {
        let deadline = Instant::now() + SETTLE_BUDGET;
        let mut seen: Vec<String> = Vec::new();
        loop {
            seen.extend(self.platform.drain_logs().into_iter().map(|(_, l)| l));
            if seen.iter().any(|l| l.contains(needle)) {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "the app never logged {needle:?}; it said: {seen:#?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Spin until `f` holds, failing the test rather than hanging if it never does.
    pub(crate) fn until(&mut self, what: &str, mut f: impl FnMut(&mut Session) -> bool) {
        let deadline = Instant::now() + SETTLE_BUDGET;
        while Instant::now() < deadline {
            if f(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }
}

/// The event-clock stamps the fixture echoed, in arrival order.
///
/// Each input line carries the compositor timestamp as a `t<N>` word (see the fixture's
/// `echo`); a line without one carried no event. The format is the harness's business, so the
/// parse lives here and the tests keep their own assertions about what the numbers must do.
pub(crate) fn event_times(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .filter_map(|l| {
            l.split_whitespace()
                .find_map(|w| w.strip_prefix('t')?.parse().ok())
        })
        .collect()
}

/// The fill the fixture paints window `i`, as the RGB a capture reports. A distinct colour per
/// window, so a capture test can tell them apart — and derived from the same expression the
/// fixture uses, so the two cannot drift.
pub(crate) fn window_fill_argb(i: usize) -> u32 {
    0x00_11_22 + (i as u32 + 1) * 0x33_00_00
}

/// [`window_fill_argb`] as the `(r, g, b)` a captured frame carries.
pub(crate) fn window_fill_rgb(i: usize) -> (u8, u8, u8) {
    let [_, r, g, b] = window_fill_argb(i).to_be_bytes();
    (r, g, b)
}

// ---------------------------------------------------------------------------
// The fixture app: a native Wayland client, re-executed as this test binary.
// ---------------------------------------------------------------------------

/// One window the fixture should map.
///
/// Parsed in the *launcher*, serialized over the env var, and re-parsed in the fixture. The wire
/// format is forced by the process boundary; parsing on the way in is not, and it is what puts a
/// malformed spec's panic on the line that wrote it. Left to the child, a typo is a launch that
/// maps nothing, which surfaces as a timeout attributed to the compositor two budgets later.
struct Spec {
    title: String,
    app_id: String,
    width: i32,
    height: i32,
}

impl Spec {
    /// `title:app_id:WxH`, all three required. Panics with the offending entry — these are test
    /// literals, so a bad one is a bug in the test, not an input to handle.
    fn parse(entry: &str) -> Spec {
        let bad = || panic!("window spec must be `title:app_id:WxH`, got {entry:?}");
        let parts: Vec<&str> = entry.split(':').collect();
        let [title, app_id, size] = parts.as_slice() else {
            bad()
        };
        let Some((w, h)) = size.split_once('x') else {
            bad()
        };
        let (Ok(width), Ok(height)) = (w.parse::<i32>(), h.parse::<i32>()) else {
            bad()
        };
        if width <= 0 || height <= 0 {
            bad()
        }
        Spec {
            title: (*title).to_string(),
            app_id: (*app_id).to_string(),
            width,
            height,
        }
    }

    fn to_entry(&self) -> String {
        format!(
            "{}:{}:{}x{}",
            self.title, self.app_id, self.width, self.height
        )
    }
}

fn parse_specs(s: &str) -> Vec<Spec> {
    s.split(',')
        .filter(|e| !e.is_empty())
        .map(Spec::parse)
        .collect()
}

/// The launched app. Not a test: `#[ignore]`d so it never runs as one, and reached only by
/// re-executing this binary with `--exact testw::fixture --ignored`.
#[test]
#[ignore = "the fixture app sway launches, not a test"]
fn fixture() {
    // A no-op unless the harness launched it. `#[ignore]` used to be what kept this from running
    // as a test, and the mutation gate now passes `--run-ignored all`, so the window list is what
    // separates "sway exec'd me" from "the runner reached me".
    let Ok(spec) = std::env::var(WINDOWS) else {
        return;
    };
    let specs = parse_specs(&spec);
    if std::env::var_os(USE_X11).is_some() {
        x11_app::run(&specs);
    } else {
        app::run(&specs);
    }
}

/// The same windows, as an X11 client. Connecting is what makes sway spawn the session's
/// Xwayland, so this is the only way a test reaches the recovery machinery at all.
mod x11_app {
    use super::{READY_LINE, Spec};
    use std::io::Write as _;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;

    pub(super) fn run(specs: &[Spec]) {
        let (conn, screen_num) = x11rb::connect(None).expect("the app should reach Xwayland");
        println!("{}", super::CONNECTED_LINE);
        std::io::stdout().flush().expect("flush");
        let screen = &conn.setup().roots[screen_num];
        for s in specs {
            let win = conn.generate_id().expect("id");
            conn.create_window(
                screen.root_depth,
                win,
                screen.root,
                0,
                0,
                s.width as u16,
                s.height as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                // As glass-testapp does. A window with no background and no event mask is one
                // wlroots' xwayland never gives a view — it reaches the X server and sway never
                // lists it, which reads as the lost-toplevel bug rather than as a bare fixture.
                &CreateWindowAux::new()
                    .background_pixel(screen.black_pixel)
                    .event_mask(
                        x11rb::protocol::xproto::EventMask::EXPOSURE
                            | x11rb::protocol::xproto::EventMask::STRUCTURE_NOTIFY,
                    ),
            )
            .expect("create_window");
            conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                win,
                x11rb::protocol::xproto::AtomEnum::WM_NAME,
                x11rb::protocol::xproto::AtomEnum::STRING,
                s.title.as_bytes(),
            )
            .expect("WM_NAME");
            conn.map_window(win).expect("map_window");
        }
        conn.sync().expect("sync");
        println!("{READY_LINE}");
        std::io::stdout().flush().expect("flush");
        // Stay up until the connection goes away with the compositor.
        while conn.wait_for_event().is_ok() {}
    }
}

mod app {
    use super::{READY_LINE, Spec};
    use std::io::Write as _;
    use std::os::fd::AsFd;

    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    };
    use wayland_client::{
        Connection, Dispatch, QueueHandle, delegate_noop, globals::registry_queue_init,
    };
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    struct App {
        configured: usize,
        wanted: usize,
        ignores_close: bool,
        pointer: Option<wl_pointer::WlPointer>,
        keyboard: Option<wl_keyboard::WlKeyboard>,
        redraw: bool,
    }

    // Every global the fixture binds is request-only from here on, so most events are simply not
    // interesting. `delegate_noop!` is wayland-client's own spelling of that; a hand-rolled macro
    // here expanded to the identical impls.
    //
    // The two events that ARE interesting get real impls below: `xdg_wm_base.ping` (a compositor
    // kills a client that stops answering) and `xdg_surface.configure` (a surface is not mapped
    // until its first configure is acked and a buffer committed).
    delegate_noop!(App: ignore wl_compositor::WlCompositor);
    delegate_noop!(App: ignore wl_surface::WlSurface);
    delegate_noop!(App: ignore wl_shm::WlShm);
    delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(App: ignore wl_buffer::WlBuffer);

    /// A seat only has a pointer or a keyboard once it says so. Asking for one before the
    /// capability is advertised is a protocol error, and the compositor answers by disconnecting
    /// — which looks from outside like input that never arrives and a window that captures black.
    impl Dispatch<wl_seat::WlSeat, ()> for App {
        fn event(
            state: &mut Self,
            seat: &wl_seat::WlSeat,
            event: wl_seat::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let wl_seat::Event::Capabilities { capabilities } = event else {
                return;
            };
            let Ok(caps) = capabilities.into_result() else {
                return;
            };
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }

    /// Every input event the app receives, on stdout. Under Wayland the client is the only thing
    /// that can say what input arrived — there is no `query_pointer`.
    fn echo(line: String) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }

    impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
        fn event(
            _: &mut Self,
            _: &wl_keyboard::WlKeyboard,
            event: wl_keyboard::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                wl_keyboard::Event::Key { key, state, .. } => echo(format!(
                    "input: key {key} {}",
                    state.into_result().map(|s| s as u32).unwrap_or(0)
                )),
                wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                    echo(format!("input: mods {mods_depressed}"))
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for App {
        fn event(
            _: &mut Self,
            _: &wl_pointer::WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                wl_pointer::Event::Enter {
                    surface_x,
                    surface_y,
                    ..
                } => echo(format!("input: enter {surface_x:.0} {surface_y:.0}")),
                // The clock goes out with each line: a compositor drops a pointer event whose
                // timestamp did not move, so a stuck clock loses input silently.
                wl_pointer::Event::Motion {
                    time,
                    surface_x,
                    surface_y,
                } => echo(format!(
                    "input: motion t{time} {surface_x:.0} {surface_y:.0}"
                )),
                wl_pointer::Event::Button {
                    time,
                    button,
                    state,
                    ..
                } => echo(format!(
                    "input: button t{time} {button} {}",
                    state.into_result().map(|s| s as u32).unwrap_or(0)
                )),
                wl_pointer::Event::Axis { time, axis, value } => echo(format!(
                    "input: axis t{time} {} {value:.2}",
                    axis.into_result().map(|a| a as u32).unwrap_or(0)
                )),
                _ => {}
            }
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
        fn event(
            state: &mut Self,
            _: &xdg_toplevel::XdgToplevel,
            event: xdg_toplevel::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // sway's `kill`. Exiting here gives the fixture a shutdown path, so teardown reports
            // a clean close instead of spending the close grace and signalling.
            if matches!(event, xdg_toplevel::Event::Close) && !state.ignores_close {
                // The end state is identical either way, so this line is the only way a test can
                // tell an app that was asked to close from one that was signalled.
                echo(super::CLOSING_LINE.to_string());
                std::process::exit(0);
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for App {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &wayland_client::globals::GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
        fn event(
            _: &mut Self,
            base: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                base.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for App {
        fn event(
            state: &mut Self,
            surface: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                surface.ack_configure(serial);
                state.configured += 1;
                // A configure is only applied by the commit that answers it: ack and go quiet,
                // and the surface is mapped at an agreed size with nothing to show.
                state.redraw = true;
            }
        }
    }

    /// An shm buffer of `w`x`h` solid `argb`, backed by a memfd written as an ordinary file (no
    /// mmap, so no `unsafe`).
    ///
    /// The file comes back with the buffer and is held for the process's lifetime: it is the
    /// buffer's pixels, and nothing else owns it.
    fn buffer(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<App>,
        w: i32,
        h: i32,
        argb: u32,
    ) -> (wl_buffer::WlBuffer, std::fs::File) {
        let len = (w * h * 4) as usize;
        let fd = rustix::fs::memfd_create("glass-testw", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("memfd_create");
        let mut file = std::fs::File::from(fd);
        let px = argb.to_ne_bytes();
        let row: Vec<u8> = px.repeat(w as usize);
        for _ in 0..h {
            file.write_all(&row).expect("fill the buffer");
        }
        file.flush().expect("flush");
        let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
        let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        (buf, file)
    }

    pub(super) fn run(specs: &[Spec]) {
        let conn = Connection::connect_to_env().expect("the app should reach the compositor");
        echo(super::CONNECTED_LINE.to_string());
        let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry");
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("wl_compositor");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).expect("xdg_wm_base");
        // Held for the process's lifetime; the pointer and keyboard are created from it when it
        // advertises them.
        let _seat: wl_seat::WlSeat = globals.bind(&qh, 1..=8, ()).expect("wl_seat");

        let mut app = App {
            configured: 0,
            wanted: specs.len(),
            ignores_close: std::env::var_os(super::IGNORES_CLOSE).is_some(),
            pointer: None,
            keyboard: None,
            redraw: false,
        };
        // Keep every proxy alive for the process's lifetime: dropping a wl_surface destroys the
        // window.
        let mut alive = Vec::new();
        for (i, s) in specs.iter().enumerate() {
            let surface = compositor.create_surface(&qh, ());
            let xdg = wm.get_xdg_surface(&surface, &qh, ());
            let toplevel = xdg.get_toplevel(&qh, ());
            toplevel.set_title(s.title.clone());
            toplevel.set_app_id(s.app_id.clone());
            surface.commit(); // no buffer yet: the compositor answers with the first configure
            queue.roundtrip(&mut app).expect("configure");
            // A distinct colour per window, so a capture test can tell them apart.
            let argb = super::window_fill_argb(i);
            let (buf, backing) = buffer(&shm, &qh, s.width, s.height, argb);
            surface.attach(Some(&buf), 0, 0);
            surface.damage(0, 0, s.width, s.height);
            surface.commit();
            alive.push((surface, xdg, toplevel, buf, backing));
        }
        let paint =
            |app: &mut App, alive: &[(wl_surface::WlSurface, _, _, wl_buffer::WlBuffer, _)]| {
                if !std::mem::take(&mut app.redraw) {
                    return;
                }
                for ((surface, _, _, buf, _), s) in alive.iter().zip(specs) {
                    surface.attach(Some(buf), 0, 0);
                    surface.damage(0, 0, s.width, s.height);
                    surface.commit();
                }
            };
        queue.roundtrip(&mut app).expect("map");
        paint(&mut app, &alive);
        while app.configured < app.wanted {
            queue.blocking_dispatch(&mut app).expect("dispatch");
            paint(&mut app, &alive);
        }
        queue.roundtrip(&mut app).expect("present");
        println!("{READY_LINE}");
        std::io::stdout().flush().expect("flush");
        // Answer pings and re-commit on every configure until the compositor goes away.
        loop {
            if queue.blocking_dispatch(&mut app).is_err() {
                return;
            }
            paint(&mut app, &alive);
        }
    }
}

mod harness_tests {
    use super::*;

    #[test]
    fn parses_a_window_list() {
        let specs = parse_specs("a:app-a:100x50,b:app-b:320x240");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].title, "a");
        assert_eq!(specs[0].app_id, "app-a");
        assert_eq!((specs[1].width, specs[1].height), (320, 240));
    }

    /// The wire format survives the round trip, which is what lets the launcher parse and the
    /// fixture re-parse the same entry.
    #[test]
    fn a_spec_round_trips_through_its_wire_form() {
        let entry = "title:app-id:640x480";
        assert_eq!(Spec::parse(entry).to_entry(), entry);
    }

    /// Every field required, every extent positive. A silently defaulted field launches a window
    /// of the wrong size, and the test then fails on dimensions with nothing naming the spec.
    #[test]
    fn a_malformed_window_spec_is_rejected_where_it_is_written() {
        for bad in [
            "solo",               // no app_id, no size
            "t:app",              // no size
            "t:app:640",          // no `x`
            "t:app:640x",         // no height
            "t:app:axb",          // not numeric
            "t:app:0x480",        // zero extent
            "t:app:640x-1",       // negative extent
            "t:app:640x480:oops", // a field too many
        ] {
            let caught = std::panic::catch_unwind(|| Spec::parse(bad));
            assert!(caught.is_err(), "{bad:?} should be rejected");
        }
    }

    /// Everything else in the suite assumes the fixture really maps a window the backend sees.
    #[test]
    #[ignore = "starts a real compositor or X server; see scripts/ci/install-display-stack.sh"]
    fn the_fixture_app_maps_a_window_the_backend_reports() {
        let mut s = Launch::new().start();
        let wins = s.windows();
        assert_eq!(wins.len(), 1, "one window");
        assert_eq!(wins[0].title.as_deref(), Some("glass-testw"));
        assert_eq!(
            wins[0].x11_window, None,
            "the fixture is a native Wayland client, not an Xwayland one"
        );
    }
}
