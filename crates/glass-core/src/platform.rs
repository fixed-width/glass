use std::path::PathBuf;

use crate::error::{GlassError, Result};
use crate::frame::{Frame, Region};
use crate::keys::Modifier;
use crate::logbuf::Stream;

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
    /// Program + args to launch. `run[0]` is the executable.
    pub run: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub window_hint: Option<WindowHint>,
    /// Max time to wait for the window to appear.
    pub timeout_ms: u64,
    /// How aggressively to contain the launched process tree.
    pub sandbox: SandboxLevel,
    /// Spawn a private, isolated AT-SPI bus for this launch so the app publishes an
    /// accessibility tree glass can read. Opt-in: when false, no a11y processes are
    /// spawned and the a11y tools return a "relaunch with a11y:true" error.
    pub a11y: bool,
}

/// The OS/display-server seam. Backends (e.g. `glass-x11`) implement this; no
/// glass-core code depends on a concrete backend. Must stay object-safe.
pub trait Platform {
    /// Run the optional build step, spawn the app, locate its window, and
    /// return the window's geometry.
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry>;

    /// Terminate the running app (idempotent).
    fn stop_app(&mut self) -> Result<()>;

    /// Capture the current window contents as an RGBA frame. `region` (if set,
    /// window-relative) captures only that sub-rectangle; `None` captures the
    /// whole window.
    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame>;

    /// Capture a specific window's region from the compositor/root WITHOUT changing
    /// the active window (unlike `select_window`). `region` (if set, relative to
    /// `id`'s own geometry) captures only that sub-rectangle; `None` captures the
    /// whole window. `WindowNotFound` if `id` is not currently one of the app's
    /// windows. Default: unsupported.
    fn capture_window(&mut self, _id: WindowId, _region: Option<&Region>) -> Result<Frame> {
        Err(GlassError::Unsupported(
            "capture_window is not supported by this backend".into(),
        ))
    }

    /// Inject a pointer event (coordinates are window-relative).
    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()>;

    /// Inject a keyboard event.
    fn send_key(&mut self, event: &KeyEvent) -> Result<()>;

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

    /// Perform a window operation, returning the resulting geometry.
    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry>;

    /// All top-level windows belonging to the launched app, re-scanned live.
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>>;

    /// Make `id` the active window (the implicit target of capture/input/window
    /// ops); returns the now-active window's geometry. `WindowNotFound` if `id`
    /// is not currently one of the app's windows.
    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry>;

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

    /// A bare-minimum `Platform` that overrides nothing — every optional method
    /// falls through to its default (erroring) implementation.
    struct MinimalPlatform;
    impl Platform for MinimalPlatform {
        fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
            Ok(WindowGeometry::default())
        }
        fn stop_app(&mut self) -> Result<()> {
            Ok(())
        }
        fn capture_frame(&mut self, _region: Option<&Region>) -> Result<Frame> {
            Err(GlassError::CaptureFailed("minimal".into()))
        }
        fn send_pointer(&mut self, _event: &PointerEvent) -> Result<()> {
            Ok(())
        }
        fn send_key(&mut self, _event: &KeyEvent) -> Result<()> {
            Ok(())
        }
        fn window(&mut self, _op: &WindowOp) -> Result<WindowGeometry> {
            Ok(WindowGeometry::default())
        }
        fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
            Ok(vec![])
        }
        fn select_window(&mut self, _id: WindowId) -> Result<WindowGeometry> {
            Err(GlassError::WindowNotFound)
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            vec![]
        }
    }

    #[test]
    fn default_capture_window_is_unsupported() {
        // A backend with no `capture_window` override (the common case today)
        // reports `Unsupported`, not a silently-wrong capture of the active window.
        let mut p = MinimalPlatform;
        let err = p.capture_window(WindowId(1), None).unwrap_err();
        assert!(matches!(err, GlassError::Unsupported(_)), "{err}");
    }
}
