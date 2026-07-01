//! The macOS `Platform` backend. Plan 1 lands the struct + trait surface with the
//! window-server methods stubbed; Plan 2 fills capture + display provisioning, Plan 3
//! input, Plan 4 windows. `new()` runs the TCC preflight so a missing grant fails fast.

use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::frame::{Frame, Region};
use glass_core::logbuf::Stream;
use glass_core::platform::{
    AppSpec, KeyEvent, Platform, PointerEvent, WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use glass_core::{GlassError, Result};

use crate::coords;
use crate::permissions;
use crate::process::{self, LogSink};

/// Poll interval between discovery attempts in [`MacosPlatform::discover_window`] —
/// matches `scwindow::find_window_for_pids`'s own poll cadence
/// (`poll_until(100, ..)`), which that loop takes over here so it can also race
/// against `child.try_wait()`.
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Timeout for the fresh window re-resolution [`MacosPlatform::send_pointer`] does on every
/// call via `scwindow::find_window_for_pids`. Short (unlike `start_app`'s `spec.timeout_ms`,
/// which waits for a brand-new window to first appear): the window is already known to have
/// existed as of the last successful call, so this only needs to cover the ordinary
/// query-round-trip latency, not a real "is the app even launching" wait.
const POINTER_RESOLVE_TIMEOUT: Duration = Duration::from_millis(2000);

/// macOS backend. v1 renders the target app onto a `CGVirtualDisplay` (Plan 2) and
/// drives it with ScreenCaptureKit + CGEvent + AXUIElement.
pub struct MacosPlatform {
    /// Logs drained by `drain_logs`, filled by `process::spawn`'s per-stream reader
    /// threads once `start_app` exists (Plan 2). `Arc<Mutex<_>>` because those threads
    /// push into it concurrently with `drain_logs` reading it here. Empty until
    /// `start_app` launches a child.
    logs: LogSink,
    /// The launched app's root pid; `None` until `start_app`.
    app_pid: Option<u32>,
    /// The launched child process, kept so `stop_app`/`Drop` can `process::terminate`
    /// it. `None` until `start_app` and after `stop_app` (idempotent).
    child: Option<Child>,
}

impl MacosPlatform {
    /// Construct the backend, failing fast if a required TCC grant is missing.
    pub fn new() -> Result<Self> {
        permissions::preflight()?;
        Ok(Self { logs: Arc::new(Mutex::new(Vec::new())), app_pid: None, child: None })
    }

    /// Run the optional `spec.build` shell step in `spec.cwd`, mirroring the X11/Windows
    /// backends' `sh -c`/`cmd /C` build step. A nonzero exit maps to
    /// `GlassError::AppNotStarted` — no launch is attempted if the build failed.
    fn run_build(spec: &AppSpec) -> Result<()> {
        let Some(build) = &spec.build else {
            return Ok(());
        };
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(build);
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        let status = cmd
            .status()
            .map_err(|e| GlassError::AppNotStarted(format!("build command: {e}")))?;
        if !status.success() {
            return Err(GlassError::AppNotStarted(format!(
                "build command failed with status {status}"
            )));
        }
        Ok(())
    }

    /// Poll for `child`'s window, alternating a single [`crate::scwindow::query_once`]
    /// discovery attempt with `child.try_wait()` so a crashed launch fails fast with
    /// [`GlassError::AppExited`] instead of riding out the whole `timeout_ms` budget
    /// waiting for a window that will never appear — mirrors
    /// `glass-x11/src/platform.rs`'s `discover_window`. Can't delegate this to
    /// `scwindow::find_window_for_pids`: that helper owns its *entire* poll loop
    /// internally, with no child handle to race against.
    ///
    /// Returns the whole [`crate::scwindow::WindowMatch`] (not just its `geometry`), even
    /// though `start_app` only reads `geometry` from it today — `send_pointer` does its own
    /// independent, fresh `scwindow::find_window_for_pids` resolution per call rather than
    /// reusing anything cached here (see its doc), so this return type is just the natural
    /// shape of a `query_once` result, not evidence of caching elsewhere.
    fn discover_window(
        child: &mut Child,
        pid: u32,
        timeout_ms: u64,
    ) -> Result<crate::scwindow::WindowMatch> {
        crate::ffi::app_kit_init();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            if let Some(m) = crate::scwindow::query_once(&[pid as i32])? {
                return Ok(m);
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(GlassError::AppExited(status.code()));
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Timeout(timeout_ms));
            }
            std::thread::sleep(DISCOVERY_POLL_INTERVAL);
        }
    }
}

/// Bounds-check every window-relative coordinate `event` carries against `geom` via
/// `coords::check_in_bounds`, failing with `GlassError::CoordOutOfBounds` before
/// `input::send_pointer` ever maps a coordinate to a global point — the "no
/// silently-wrong click" invariant. `glass_core::session` already runs an equivalent check
/// (`Session::check_bounds`) above every backend, but that's not a substitute here: this
/// crate's mac-gated integration tests (Task 6) call `MacosPlatform::send_pointer`
/// directly, bypassing the session layer entirely, so the backend must not depend on a
/// caller it can't guarantee sits in front of it. Mirrors `Session::check_bounds`'s own
/// per-variant coverage (both endpoints of a `Drag`, every pointer of a `Gesture`) even
/// though `Gesture` itself is `Unsupported` on macOS — bounds-checking still runs first so
/// an out-of-bounds `Gesture` reports `CoordOutOfBounds`, not `Unsupported`.
fn check_pointer_bounds(event: &PointerEvent, geom: &WindowGeometry) -> Result<()> {
    let check = |x: i32, y: i32| coords::check_in_bounds(x, y, geom.width, geom.height);
    match *event {
        PointerEvent::Move { x, y } => check(x, y),
        PointerEvent::Click { x, y, .. } => check(x, y),
        PointerEvent::Scroll { x, y, .. } => check(x, y),
        PointerEvent::Drag { from_x, from_y, to_x, to_y, .. } => {
            check(from_x, from_y)?;
            check(to_x, to_y)
        }
        PointerEvent::Gesture { ref pointers, .. } => {
            for p in pointers {
                check(p.from_x, p.from_y)?;
                check(p.to_x, p.to_y)?;
            }
            Ok(())
        }
    }
}

impl Platform for MacosPlatform {
    /// Run the optional build step, spawn the app, then confirm a window appears for its
    /// pid within `spec.timeout_ms` via ScreenCaptureKit's `SCShareableContent`
    /// enumeration — alternated with `child.try_wait()` so a crashed launch fails fast
    /// with `GlassError::AppExited` instead of riding out the whole timeout (see
    /// `discover_window`).
    ///
    /// **Main-thread affinity:** `discover_window` calls `ffi::app_kit_init()`, which
    /// requires the true main thread (`MainThreadMarker` panics off it). In Plan 2 this is
    /// exercised only by Task 6's `harness=false` main-thread test; wiring it under
    /// glass-mcp's worker-thread dispatcher (main-thread marshaling) is deferred to Plan 5.
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        Self::run_build(spec)?;
        let mut child = process::spawn(spec, self.logs.clone())?;
        let pid = child.id();
        match Self::discover_window(&mut child, pid, spec.timeout_ms) {
            Ok(m) => {
                self.child = Some(child);
                self.app_pid = Some(pid);
                // Scale/origin/geometry are NOT cached here: `send_pointer` re-resolves the
                // window fresh on every call instead (see its doc) since it may move/resize
                // after this initial discovery. Only the initial geometry is returned to the
                // caller, matching every other backend's `start_app` contract.
                Ok(m.geometry)
            }
            Err(e) => {
                // The window never appeared (or discovery otherwise failed): don't leak
                // the spawned child.
                process::terminate(&mut child);
                Err(e)
            }
        }
    }

    /// Terminate the launched child (if any) and clear the active pid. Idempotent — a
    /// call with nothing running is `Ok(())`.
    fn stop_app(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child);
        }
        self.app_pid = None;
        Ok(())
    }

    /// Capture the active app's window as an RGBA8 frame, re-resolving it by pid on
    /// every call (see `scwindow.rs`'s module doc — a `Retained<SCWindow>` can't be
    /// cached across calls).
    ///
    /// **Main-thread affinity:** like `start_app`, this reaches `ffi::app_kit_init()`
    /// (via `capture::capture_window`) and must run on the true main thread; see the
    /// note on `start_app`.
    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        crate::capture::capture_window(&[pid as i32], region)
    }
    /// Map the active session's pid into `input::send_pointer` — see `input.rs`'s module
    /// doc for the CGEvent details and its main-thread-affinity note (shared with
    /// `start_app`/`capture_frame` above).
    ///
    /// NOTE: re-resolves the window fresh via `scwindow::find_window_for_pids` on every
    /// call, mirroring `capture_frame`'s per-call resolution above — the window may have
    /// moved or resized since `start_app` (or any earlier `send_pointer` call), so a
    /// `scale`/`origin_pt`/geometry cached once at `start_app` would go stale. Both the
    /// bounds check (`check_pointer_bounds`, see its doc) and the coordinate mapping use
    /// this freshly-resolved geometry/scale/origin.
    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let m = crate::scwindow::find_window_for_pids(&[pid as i32], POINTER_RESOLVE_TIMEOUT)?;
        check_pointer_bounds(event, &m.geometry)?;
        crate::input::send_pointer(event, pid as i32, m.scale, m.origin_pt)
    }
    /// Map the active session's pid into `input::send_key` — see `input.rs`'s module doc
    /// for the CGEvent keyboard details.
    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        crate::input::send_key(event, pid as i32)
    }
    fn window(&mut self, _op: &WindowOp) -> Result<WindowGeometry> {
        unimplemented!("Plan 4: AXUIElement window ops")
    }
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        unimplemented!("Plan 4: CGWindowList/SCShareableContent by pid")
    }
    fn select_window(&mut self, _id: WindowId) -> Result<WindowGeometry> {
        unimplemented!("Plan 4: raise + focus + activate")
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        std::mem::take(&mut *self.logs.lock().expect("log buffer mutex"))
    }
    fn app_pid(&self) -> Option<u32> {
        self.app_pid
    }
}

impl Drop for MacosPlatform {
    /// Reap a still-running launched app on drop — parity with the X11/Wayland/Windows
    /// backends, so a backend dropped without an explicit `stop_app()` (panic-unwind, or
    /// the process-exit backstop path) does not orphan its child. `process::terminate`
    /// is idempotent, and `child.take()` in `stop_app` means this is a no-op if
    /// `stop_app` already ran.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_logs_takes_then_empties() {
        // Build without preflight (which would require grants) by constructing the struct
        // directly — `new()` is exercised in the Mac-gated suite.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(vec![(Stream::Stdout, "hi".into())])),
            app_pid: Some(42),
            child: None,
        };
        assert_eq!(p.drain_logs().len(), 1);
        assert!(p.drain_logs().is_empty());
    }

    #[test]
    fn app_pid_returns_the_constructed_value() {
        let p = MacosPlatform { logs: Arc::new(Mutex::new(Vec::new())), app_pid: Some(42), child: None };
        assert_eq!(p.app_pid(), Some(42));
    }

    #[test]
    fn stop_app_on_a_fresh_platform_is_idempotent() {
        // No child stored: stop_app must not panic (e.g. on an unwrap of a live process
        // handle) and must succeed — this path never touches AppKit, so it's exercisable
        // off the Mac-gated suite's main-thread test.
        let mut p = MacosPlatform { logs: Arc::new(Mutex::new(Vec::new())), app_pid: None, child: None };
        assert!(p.stop_app().is_ok());
        assert!(p.stop_app().is_ok(), "a second call must also be Ok");
        assert_eq!(p.app_pid(), None);
    }

    #[test]
    fn new_agrees_with_preflight() {
        // The central invariant: new() must error iff preflight() errors. Guards against a
        // future edit that swallows the missing-grant propagation. On an ungranted CI runner
        // both are Err; on a granted box both are Ok.
        assert_eq!(crate::permissions::preflight().is_err(), MacosPlatform::new().is_err());
    }

    #[test]
    fn check_pointer_bounds_accepts_inside_rejects_outside() {
        let geom = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        assert!(check_pointer_bounds(&PointerEvent::Move { x: 0, y: 0 }, &geom).is_ok());
        assert!(check_pointer_bounds(&PointerEvent::Move { x: 639, y: 479 }, &geom).is_ok());
        assert!(matches!(
            check_pointer_bounds(&PointerEvent::Move { x: 640, y: 0 }, &geom),
            Err(GlassError::CoordOutOfBounds { .. })
        ));
        assert!(matches!(
            check_pointer_bounds(&PointerEvent::Move { x: -1, y: 0 }, &geom),
            Err(GlassError::CoordOutOfBounds { .. })
        ));
    }

    #[test]
    fn check_pointer_bounds_checks_both_drag_endpoints() {
        use glass_core::platform::MouseButton;
        let geom = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        // In-bounds `from`, out-of-bounds `to`: must still reject.
        let ev = PointerEvent::Drag {
            from_x: 0,
            from_y: 0,
            to_x: 700,
            to_y: 0,
            button: MouseButton::Left,
            modifiers: vec![],
            duration_ms: 100,
        };
        assert!(matches!(check_pointer_bounds(&ev, &geom), Err(GlassError::CoordOutOfBounds { .. })));
    }
}
