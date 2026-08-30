use std::path::PathBuf;
use std::time::Duration;

use crate::error::{GlassError, Result};
use crate::frame::{Frame, Region};
use crate::keys::Modifier;
use crate::logbuf::Stream;

/// The wall-clock budget the whole of teardown gets when the *process* is going away — the
/// server's stdin closed or a termination signal arrived. `glass-mcp`'s shutdown path spends it
/// on `Glass::shutdown` and then exits regardless, so anything a [`Platform::stop_app`] impl was
/// still waiting on is abandoned mid-flight: on a Unix backend that orphans the app to init.
///
/// Every backend's teardown must therefore fit inside this. The four that wait on a local child
/// assert their grace constants against it at compile time; the two that tear down over a link to
/// a device have no constants to sum and spend [`Platform::stop_app_by`]'s deadline instead.
pub const TEARDOWN_BUDGET: Duration = Duration::from_secs(3);

/// How much of [`TEARDOWN_BUDGET`] is held back from the deadline the steps are given.
///
/// Killing a step that hit its deadline costs up to [`crate::bounded::KILL_REAP`] *after* that
/// deadline, so without headroom the case this exists for — one step wedges, is cut off, the rest
/// still run — is itself what overruns the budget.
pub const TEARDOWN_REAP_HEADROOM: Duration = Duration::from_millis(750);

/// How much of [`TEARDOWN_BUDGET`] is reserved for the host's shutdown hook, so stopping the
/// sessions cannot spend all of it.
///
/// A shared deadline bounds a sequence; it does not divide one. Without a reserve, a device that
/// stops answering during `stop_app` leaves the hook nothing — and `run_bounded_until` does not
/// start a command with no time left, so its steps are skipped rather than slowed. On Android the
/// hook is what hands the device back (`glass_android::hand_the_device_back`).
///
/// Five adb calls, the slowest measured at 52ms on an emulator with every core saturated
/// (glass#422).
pub const TEARDOWN_HOOK_RESERVE: Duration = Duration::from_millis(750);

// The sessions get what is left — 1.5s, against the ~270ms the slowest measured backend
// teardown needs.
const _: () = assert!(
    TEARDOWN_REAP_HEADROOM.as_millis() + TEARDOWN_HOOK_RESERVE.as_millis()
        < TEARDOWN_BUDGET.as_millis(),
    "the reap headroom and the hook's reserve must leave the sessions something to stop with"
);
const _: () = assert!(
    crate::bounded::KILL_REAP.as_millis() < TEARDOWN_REAP_HEADROOM.as_millis(),
    "the headroom must cover a killed step's reap, or the kill itself overruns the budget"
);

/// How aggressively to contain the process tree a backend launches. Platform-agnostic
/// *policy*; the Linux mechanism (bubblewrap) lives in the `glass-sandbox-linux` crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SandboxLevel {
    /// Filesystem/process containment, network allowed. The secure default.
    #[default]
    Default,
    /// `Default` plus no network (`unshare net`).
    Strict,
    /// No containment — explicit opt-out.
    Off,
}

impl std::str::FromStr for SandboxLevel {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(SandboxLevel::Default),
            "strict" => Ok(SandboxLevel::Strict),
            "off" => Ok(SandboxLevel::Off),
            other => Err(format!(
                "unknown sandbox level '{other}'; expected default|strict|off"
            )),
        }
    }
}

impl SandboxLevel {
    /// Containment strength as a total order: `Off` < `Default` < `Strict`. Used to enforce a policy
    /// floor (the operator-set minimum). This is deliberately NOT the enum's declaration order, so
    /// deriving `Ord` would give the wrong answer — hence an explicit rank.
    pub fn strength(self) -> u8 {
        match self {
            SandboxLevel::Off => 0,
            SandboxLevel::Default => 1,
            SandboxLevel::Strict => 2,
        }
    }
}

impl std::fmt::Display for SandboxLevel {
    /// The lowercase name, round-tripping with `FromStr` (used in the policy-floor error message
    /// and the `doctor` floor line).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SandboxLevel::Off => "off",
            SandboxLevel::Default => "default",
            SandboxLevel::Strict => "strict",
        })
    }
}

/// Window geometry in screen coordinates, as reported by a backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// One pointer's straight path within a multi-touch gesture, in **window-relative** px.
/// `from == to` is a finger held stationary for the gesture's duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
}

/// Max simultaneous pointers a `Gesture` may carry (Android tops out around this).
pub const MAX_GESTURE_POINTERS: usize = 10;

/// Maximum consecutive clicks accepted by public tools and defensive backend entry points.
pub const MAX_CLICK_COUNT: u32 = 10;

/// Maximum absolute wheel-notch count accepted on either scroll axis.
pub const MAX_SCROLL_NOTCHES: u32 = 100;

/// A pointer action in **window-relative** coordinates (0,0 = top-left).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    Move {
        x: i32,
        y: i32,
    },
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
        count: u32,
        modifiers: Vec<Modifier>,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        button: MouseButton,
        modifiers: Vec<Modifier>,
        duration_ms: u64,
    },
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        modifiers: Vec<Modifier>,
    },
    /// N simultaneous straight pointer segments, all down at t=0 and up at t=duration_ms.
    /// Android-only (needs the on-device agent); other paths return `Unsupported`.
    Gesture {
        pointers: Vec<Segment>,
        duration_ms: u64,
    },
}

/// Validate a click work factor before any backend allocation, loop, or native dispatch.
pub fn validate_click_count(count: u32) -> crate::Result<()> {
    if (1..=MAX_CLICK_COUNT).contains(&count) {
        Ok(())
    } else {
        Err(
            GlassError::InvalidPointerInput("click count must be between 1 and 10")
                .before_dispatch(),
        )
    }
}

/// Validate and return a scroll delta's unsigned magnitude without negating `i32::MIN`.
pub fn validate_scroll_delta(delta: i32) -> crate::Result<u32> {
    let magnitude = delta.unsigned_abs();
    if magnitude <= MAX_SCROLL_NOTCHES {
        Ok(magnitude)
    } else {
        Err(
            GlassError::InvalidPointerInput("scroll dx and dy must each be between -100 and 100")
                .before_dispatch(),
        )
    }
}

/// Defensively bound scalar pointer work at core and backend entry points.
pub fn validate_pointer_input(event: &PointerEvent) -> crate::Result<()> {
    match event {
        PointerEvent::Click { count, .. } => validate_click_count(*count),
        PointerEvent::Scroll { dx, dy, .. } => {
            validate_scroll_delta(*dx)?;
            validate_scroll_delta(*dy)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A keyboard action: either literal text to type or a chord like `ctrl+s`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Text(String),
    Chord(String),
}

/// A window-management request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowOp {
    Focus,
    Resize { width: u32, height: u32 },
    Move { x: i32, y: i32 },
    Geometry,
}

/// Opaque, backend-assigned window identity. May change across calls (a closed
/// and reopened window gets a new id); callers should re-list rather than cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

/// A top-level window belonging to the launched app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: Option<String>,
    pub class: Option<String>,
    pub geometry: WindowGeometry,
    pub active: bool,
}

/// Hint to disambiguate the target window when more than one appears.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowHint {
    pub title: Option<String>,
    pub class: Option<String>,
}

/// The private a11y bus details a sandboxed/launched app needs, kept together so a caller can't
/// pass the bus address without also binding its socket dir into the sandbox. `addr` is the private
/// **session** bus address (for `DBUS_SESSION_BUS_ADDRESS`); `dir` is the per-launch runtime dir
/// holding the session + at-spi sockets (bound into the sandbox).
#[derive(Clone, Copy, Debug)]
pub struct A11yBind<'a> {
    pub addr: &'a str,
    pub dir: &'a std::path::Path,
}

/// Everything a backend needs to build, launch, and locate an app's window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSpec {
    /// Optional shell command run (in `cwd`) before launching.
    pub build: Option<String>,
    /// Program + args to launch. `run[0]` is the thing to launch and `run[1..]` are its
    /// arguments.
    ///
    /// A backend must either pass `run[1..]` to the launched process or fail: dropping them
    /// launches an app the caller did not ask for and reports success, which is the silent
    /// fallback this crate's contract forbids. Not every platform can carry them — one that
    /// launches by activity or document rather than by command line has no argument vector to
    /// fill — and those backends return an error naming what they could not use.
    ///
    /// What `run[0]` names is the backend's own business (an executable, a bundle, an installed
    /// app's identifier); each backend's `start_app` documents the shapes it accepts.
    pub run: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub window_hint: Option<WindowHint>,
    /// Max time to wait for the window to appear.
    pub timeout_ms: u64,
    /// How aggressively to contain the launched process tree.
    pub sandbox: SandboxLevel,
    /// Spawn a private, isolated AT-SPI bus for this launch so the app publishes an
    /// accessibility tree glass can read. On by default at the tool boundary (the a11y
    /// path is the low-token default); when false, no a11y processes are spawned and the
    /// a11y tools return a "relaunch with a11y:true" error. Only affects Linux backends;
    /// others read accessibility ambiently.
    pub a11y: bool,
}

/// The OS/display-server seam. Backends (e.g. `glass-x11`) implement this; no
/// glass-core code depends on a concrete backend. Must stay object-safe.
pub trait Platform {
    /// Run the optional build step, spawn the app, locate its window, and
    /// return the window's geometry.
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry>;

    /// Terminate the running app (idempotent), giving up by `deadline`.
    ///
    /// **Cap every wait of your own by it.** Teardown shares [`TEARDOWN_BUDGET`], so a step that
    /// overruns spends what the steps behind it needed, and they are skipped rather than slowed
    /// (glass#422, glass#427).
    ///
    /// A backend whose teardown is a signal ladder over local processes may ignore it, having
    /// asserted its own constants against [`TEARDOWN_BUDGET`] already. One that tears down over a
    /// link to a device has a tool invocation per step, no sum to assert, and nothing but this.
    ///
    /// Required rather than defaulted, so a new device-linked backend cannot inherit an unbounded
    /// teardown in silence — which is how both device backends came to need glass#422/#427.
    fn stop_app_by(&mut self, deadline: crate::Deadline) -> Result<()>;

    /// [`Platform::stop_app_by`] with no caller deadline; backend-owned ceilings still apply.
    fn stop_app(&mut self) -> Result<()> {
        self.stop_app_by(crate::Deadline::UNBOUNDED)
    }

    /// Capture an optional window-relative region as RGBA under the caller's deadline.
    fn capture_frame_by(
        &mut self,
        region: Option<&Region>,
        deadline: crate::Deadline,
    ) -> Result<Frame>;

    /// [`Platform::capture_frame_by`] with no caller deadline; backend-owned ceilings still apply.
    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        self.capture_frame_by(region, crate::Deadline::UNBOUNDED)
    }

    /// Capture an optional region of `id` under the caller's deadline without changing the active
    /// window; a missing id returns `WindowNotFound`.
    fn capture_window_by(
        &mut self,
        id: WindowId,
        region: Option<&Region>,
        deadline: crate::Deadline,
    ) -> Result<Frame>;

    /// [`Platform::capture_window_by`] with no caller deadline; backend-owned ceilings still apply.
    fn capture_window(&mut self, id: WindowId, region: Option<&Region>) -> Result<Frame> {
        self.capture_window_by(id, region, crate::Deadline::UNBOUNDED)
    }

    /// Inject a pointer event (coordinates are window-relative), bounded by a caller's shared
    /// deadline.
    fn send_pointer_by(&mut self, event: &PointerEvent, deadline: crate::Deadline) -> Result<()>;

    /// [`Platform::send_pointer_by`] with no caller deadline; backend-owned ceilings still apply.
    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        self.send_pointer_by(event, crate::Deadline::UNBOUNDED)
    }

    /// Inject a keyboard event, bounded by a caller's shared deadline.
    fn send_key_by(&mut self, event: &KeyEvent, deadline: crate::Deadline) -> Result<()>;

    /// [`Platform::send_key_by`] with no caller deadline; backend-owned ceilings still apply.
    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.send_key_by(event, crate::Deadline::UNBOUNDED)
    }

    /// Read the clipboard as UTF-8 text ("" if it holds no text).
    fn get_clipboard(&mut self) -> Result<String> {
        Err(GlassError::Unsupported(
            "clipboard is not supported by this backend".into(),
        ))
    }
    /// Write UTF-8 text to the clipboard. On X11/Wayland this installs a
    /// session-lived serving owner; it is torn down on `stop_app`.
    fn set_clipboard(&mut self, _text: &str) -> Result<()> {
        Err(GlassError::Unsupported(
            "clipboard is not supported by this backend".into(),
        ))
    }

    /// Perform a window operation under the caller's deadline and return its geometry; backends
    /// must bound blocking work.
    fn window_by(&mut self, op: &WindowOp, deadline: crate::Deadline) -> Result<WindowGeometry>;

    /// [`Platform::window_by`] with no caller deadline; backend-owned ceilings still apply.
    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        self.window_by(op, crate::Deadline::UNBOUNDED)
    }

    /// All top-level windows belonging to the launched app, re-scanned live under the caller's
    /// shared deadline.
    fn list_windows_by(&mut self, deadline: crate::Deadline) -> Result<Vec<WindowInfo>>;

    /// [`Platform::list_windows_by`] with no caller deadline; backend-owned ceilings still apply.
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        self.list_windows_by(crate::Deadline::UNBOUNDED)
    }

    /// Activate `id` under the caller's deadline and return its geometry, or `WindowNotFound`.
    fn select_window_by(
        &mut self,
        id: WindowId,
        deadline: crate::Deadline,
    ) -> Result<WindowGeometry>;

    /// [`Platform::select_window_by`] with no caller deadline; backend-owned ceilings still apply.
    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        self.select_window_by(id, crate::Deadline::UNBOUNDED)
    }

    /// Hand back any log lines captured since the last call.
    fn drain_logs(&mut self) -> Vec<(Stream, String)>;

    /// The launched app's OS pid, if the backend knows it — used to correlate
    /// the accessibility tree to this app. Defaults to `None`; display backends
    /// override when they hold the child handle.
    fn app_pid(&self) -> Option<u32> {
        None
    }

    /// The launched app's process ids — the root plus any descendant/child processes the
    /// backend can enumerate (used to correlate the accessibility tree to a window that a
    /// multi-process app owns from a descendant process). Defaults to the single `app_pid`
    /// as a one-element vec (empty if unknown); a backend with a process-tree view overrides.
    fn app_pids(&self) -> Vec<u32> {
        self.app_pid().into_iter().collect()
    }

    /// Resolve [`Platform::app_pids`] under the shared deadline; blocking backends must override
    /// and bound discovery.
    fn app_pids_by(&self, deadline: crate::Deadline) -> Result<Vec<u32>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "accessibility process discovery",
            ));
        }
        let pids = self.app_pids();
        if deadline.has_passed() {
            Err(GlassError::caller_deadline_elapsed(
                "accessibility process discovery",
            ))
        } else {
            Ok(pids)
        }
    }

    /// The session's private AT-SPI bus address, if this backend spawned one.
    /// Default `None` (no private bus / non-Linux backends).
    fn a11y_bus_addr(&self) -> Option<String> {
        None
    }

    /// Raw native handle of the active (adopted) window — a Windows `HWND` as `i64` — for the
    /// accessibility reader to bind to directly (`AxContext::window_handle`). Default `None`;
    /// backends that address a11y by bus (Linux) leave it unset.
    fn active_window_handle(&self) -> Option<i64> {
        None
    }

    /// Whether this backend reports a checkable element's a11y frame as the whole containing
    /// *row*, with the actual control at the row's **trailing** edge — as iOS/idb does for a
    /// `UISwitch` in a grouped table cell. When true, `click_element` aims a row-shaped
    /// checkable's actuation at the trailing control (a swipe, since a tap doesn't actuate a
    /// UISwitch) instead of the geometric center, which would otherwise land on the inert label
    /// and no-op. Default `false`: every other backend reports a switch's frame as the tight
    /// control (its center is the right target), so the trailing-aim would misfire on a wide
    /// *labeled* checkbox whose indicator is at the leading edge — hence this is opt-in per
    /// backend rather than a geometry heuristic.
    fn a11y_toggle_control_at_trailing_edge(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod sandbox_level_tests {
    use super::SandboxLevel;
    use std::str::FromStr;

    #[test]
    fn parses_known_levels_case_insensitively() {
        assert_eq!(
            SandboxLevel::from_str("default").unwrap(),
            SandboxLevel::Default
        );
        assert_eq!(
            SandboxLevel::from_str("strict").unwrap(),
            SandboxLevel::Strict
        );
        assert_eq!(SandboxLevel::from_str("off").unwrap(), SandboxLevel::Off);
        assert_eq!(SandboxLevel::from_str("OFF").unwrap(), SandboxLevel::Off);
    }

    #[test]
    fn rejects_unknown_level_with_helpful_message() {
        let err = SandboxLevel::from_str("loose").unwrap_err();
        assert!(err.contains("loose"), "{err}");
        assert!(err.contains("default|strict|off"), "{err}");
    }

    #[test]
    fn default_is_default_variant() {
        assert_eq!(SandboxLevel::default(), SandboxLevel::Default);
    }

    #[test]
    fn strength_orders_off_below_default_below_strict() {
        assert!(SandboxLevel::Off.strength() < SandboxLevel::Default.strength());
        assert!(SandboxLevel::Default.strength() < SandboxLevel::Strict.strength());
    }

    #[test]
    fn display_round_trips_with_from_str() {
        for lvl in [
            SandboxLevel::Off,
            SandboxLevel::Default,
            SandboxLevel::Strict,
        ] {
            assert_eq!(lvl.to_string().parse::<SandboxLevel>().unwrap(), lvl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_is_object_safe() {
        // Compiles only if `Platform` is object-safe.
        fn _accepts(_p: &mut dyn Platform) {}
    }

    #[test]
    fn app_spec_is_constructible() {
        let spec = AppSpec {
            build: Some("cargo build".into()),
            run: vec!["./app".into()],
            cwd: None,
            env: vec![("RUST_LOG".into(), "debug".into())],
            window_hint: Some(WindowHint {
                title: Some("Demo".into()),
                class: None,
            }),
            timeout_ms: 5000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };
        assert_eq!(spec.run[0], "./app");
    }

    fn assert_invalid_pointer_work(event: PointerEvent) {
        let error = validate_pointer_input(&event).expect_err("unsafe input work must be rejected");
        assert!(matches!(error.cause(), GlassError::InvalidPointerInput(_)));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn click_count_validation_rejects_zero_and_oversized_work() {
        for count in [0, 11, u32::MAX] {
            assert_invalid_pointer_work(PointerEvent::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count,
                modifiers: vec![],
            });
        }
    }

    #[test]
    fn scroll_validation_rejects_overflowing_and_oversized_magnitudes() {
        for delta in [-101, 101, i32::MIN, i32::MAX] {
            assert_invalid_pointer_work(PointerEvent::Scroll {
                x: 0,
                y: 0,
                dx: delta,
                dy: 0,
                modifiers: vec![],
            });
            assert_invalid_pointer_work(PointerEvent::Scroll {
                x: 0,
                y: 0,
                dx: 0,
                dy: delta,
                modifiers: vec![],
            });
        }
    }

    #[test]
    fn bounded_click_and_scroll_work_is_accepted() {
        for count in [1, 10] {
            validate_pointer_input(&PointerEvent::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count,
                modifiers: vec![],
            })
            .unwrap();
        }
        for delta in [-100, 0, 100] {
            validate_pointer_input(&PointerEvent::Scroll {
                x: 0,
                y: 0,
                dx: delta,
                dy: delta,
                modifiers: vec![],
            })
            .unwrap();
        }
    }

    /// A bare-minimum `Platform` that overrides nothing — every optional method
    /// falls through to its default (erroring) implementation.
    struct MinimalPlatform;
    impl Platform for MinimalPlatform {
        fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
            Ok(WindowGeometry::default())
        }
        fn stop_app_by(&mut self, _deadline: crate::Deadline) -> Result<()> {
            Ok(())
        }
        fn capture_frame_by(
            &mut self,
            _region: Option<&Region>,
            _deadline: crate::Deadline,
        ) -> Result<Frame> {
            Err(GlassError::CaptureFailed("minimal".into()))
        }
        fn capture_window_by(
            &mut self,
            _id: WindowId,
            _region: Option<&Region>,
            deadline: crate::Deadline,
        ) -> Result<Frame> {
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started("window capture"));
            }
            Err(GlassError::Unsupported(
                "capture_window is not supported by this backend".into(),
            ))
        }
        fn send_pointer_by(
            &mut self,
            _event: &PointerEvent,
            _deadline: crate::Deadline,
        ) -> Result<()> {
            Ok(())
        }
        fn send_key_by(&mut self, _event: &KeyEvent, _deadline: crate::Deadline) -> Result<()> {
            Ok(())
        }
        fn window_by(
            &mut self,
            _op: &WindowOp,
            _deadline: crate::Deadline,
        ) -> Result<WindowGeometry> {
            Ok(WindowGeometry::default())
        }
        fn list_windows_by(&mut self, _deadline: crate::Deadline) -> Result<Vec<WindowInfo>> {
            Ok(vec![])
        }
        fn select_window_by(
            &mut self,
            _id: WindowId,
            _deadline: crate::Deadline,
        ) -> Result<WindowGeometry> {
            Err(GlassError::WindowNotFound)
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            vec![]
        }
    }

    /// Knows its pid and overrides nothing else, so `app_pids` falls through to the
    /// default that derives from `app_pid`.
    struct PidPlatform(u32);
    impl Platform for PidPlatform {
        fn app_pid(&self) -> Option<u32> {
            Some(self.0)
        }
        fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
            unimplemented!()
        }
        fn stop_app_by(&mut self, _deadline: crate::Deadline) -> Result<()> {
            unimplemented!()
        }
        fn capture_frame_by(
            &mut self,
            _region: Option<&Region>,
            _deadline: crate::Deadline,
        ) -> Result<Frame> {
            unimplemented!()
        }
        fn capture_window_by(
            &mut self,
            _id: WindowId,
            _region: Option<&Region>,
            deadline: crate::Deadline,
        ) -> Result<Frame> {
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started("window capture"));
            }
            Err(GlassError::Unsupported(
                "capture_window is not supported by this backend".into(),
            ))
        }
        fn send_pointer_by(
            &mut self,
            _event: &PointerEvent,
            _deadline: crate::Deadline,
        ) -> Result<()> {
            unimplemented!()
        }
        fn send_key_by(&mut self, _event: &KeyEvent, _deadline: crate::Deadline) -> Result<()> {
            unimplemented!()
        }
        fn window_by(
            &mut self,
            _op: &WindowOp,
            _deadline: crate::Deadline,
        ) -> Result<WindowGeometry> {
            unimplemented!()
        }
        fn list_windows_by(&mut self, _deadline: crate::Deadline) -> Result<Vec<WindowInfo>> {
            unimplemented!()
        }
        fn select_window_by(
            &mut self,
            _id: WindowId,
            _deadline: crate::Deadline,
        ) -> Result<WindowGeometry> {
            unimplemented!()
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            unimplemented!()
        }
    }

    #[test]
    fn default_capture_window_is_unsupported() {
        // The legacy wrapper reaches the backend's explicit unsupported decision, rather than
        // silently capturing the active window.
        let mut p = MinimalPlatform;
        let err = p.capture_window(WindowId(1), None).unwrap_err();
        assert!(matches!(err, GlassError::Unsupported(_)), "{err}");
    }

    #[test]
    fn default_app_pid_is_unknown() {
        assert_eq!(MinimalPlatform.app_pid(), None);
    }

    /// Legacy wrappers must still reach the required deadline-bearing backend methods.
    #[test]
    fn legacy_platform_methods_reach_required_methods_with_no_bound() {
        #[derive(Default)]
        struct CountingPlatform {
            stop: Option<crate::Deadline>,
            capture_frame: Option<crate::Deadline>,
            capture_window: Option<crate::Deadline>,
            pointer: Option<crate::Deadline>,
            key: Option<crate::Deadline>,
            window: Option<crate::Deadline>,
            list_windows: Option<crate::Deadline>,
            select_window: Option<crate::Deadline>,
        }
        impl Platform for CountingPlatform {
            fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
                Ok(WindowGeometry::default())
            }
            fn stop_app_by(&mut self, deadline: crate::Deadline) -> Result<()> {
                self.stop = Some(deadline);
                Ok(())
            }
            fn capture_frame_by(
                &mut self,
                _region: Option<&Region>,
                deadline: crate::Deadline,
            ) -> Result<Frame> {
                self.capture_frame = Some(deadline);
                Err(GlassError::CaptureFailed("counted".into()))
            }
            fn capture_window_by(
                &mut self,
                _id: WindowId,
                _region: Option<&Region>,
                deadline: crate::Deadline,
            ) -> Result<Frame> {
                if deadline.has_passed() {
                    return Err(GlassError::deadline_not_started("window capture"));
                }
                self.capture_window = Some(deadline);
                Err(GlassError::Unsupported("counted".into()))
            }
            fn send_pointer_by(
                &mut self,
                _event: &PointerEvent,
                deadline: crate::Deadline,
            ) -> Result<()> {
                self.pointer = Some(deadline);
                Ok(())
            }
            fn send_key_by(&mut self, _event: &KeyEvent, deadline: crate::Deadline) -> Result<()> {
                self.key = Some(deadline);
                Ok(())
            }
            fn window_by(
                &mut self,
                _op: &WindowOp,
                deadline: crate::Deadline,
            ) -> Result<WindowGeometry> {
                self.window = Some(deadline);
                Ok(WindowGeometry::default())
            }
            fn list_windows_by(&mut self, deadline: crate::Deadline) -> Result<Vec<WindowInfo>> {
                self.list_windows = Some(deadline);
                Ok(vec![])
            }
            fn select_window_by(
                &mut self,
                _id: WindowId,
                deadline: crate::Deadline,
            ) -> Result<WindowGeometry> {
                self.select_window = Some(deadline);
                Err(GlassError::WindowNotFound)
            }
            fn drain_logs(&mut self) -> Vec<(Stream, String)> {
                vec![]
            }
        }

        let mut p = CountingPlatform::default();
        p.stop_app().expect("the default delegates");
        let _ = p.capture_frame(None);
        let _ = p.capture_window(WindowId(1), None);
        p.send_pointer(&PointerEvent::Move { x: 1, y: 1 })
            .expect("the default delegates");
        p.send_key(&KeyEvent::Chord("enter".into()))
            .expect("the default delegates");
        p.window(&WindowOp::Geometry)
            .expect("the default delegates");
        p.list_windows().expect("the default delegates");
        let _ = p.select_window(WindowId(1));

        assert_eq!(p.stop, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.capture_frame, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.capture_window, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.pointer, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.key, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.window, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.list_windows, Some(crate::Deadline::UNBOUNDED));
        assert_eq!(p.select_window, Some(crate::Deadline::UNBOUNDED));
    }

    #[test]
    fn default_app_pids_is_the_one_pid_the_backend_knows() {
        assert_eq!(PidPlatform(4242).app_pids(), vec![4242]);
        assert_eq!(
            PidPlatform(4242)
                .app_pids_by(crate::Deadline::UNBOUNDED)
                .unwrap(),
            vec![4242]
        );
    }

    #[test]
    fn default_app_pids_is_empty_when_the_pid_is_unknown() {
        assert!(MinimalPlatform.app_pids().is_empty());
        assert!(
            MinimalPlatform
                .app_pids_by(crate::Deadline::UNBOUNDED)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn default_a11y_bus_addr_is_unset() {
        // Not `Some("")`: an empty address would be passed to a bus connection as if real.
        assert_eq!(MinimalPlatform.a11y_bus_addr(), None);
    }

    #[test]
    fn default_active_window_handle_is_unset() {
        assert_eq!(MinimalPlatform.active_window_handle(), None);
    }

    #[test]
    fn default_backend_aims_a_checkable_at_its_centre() {
        // Opt-in per backend: the trailing-edge aim misfires on a wide labeled checkbox.
        assert!(!MinimalPlatform.a11y_toggle_control_at_trailing_edge());
    }
}
