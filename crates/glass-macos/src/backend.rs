//! The macOS `Platform` backend. Plan 1 lands the struct + trait surface with the
//! window-server methods stubbed; Plan 2 fills capture + display provisioning, Plan 3
//! input, Plan 4 windows. `new()` runs the TCC preflight so a missing grant fails fast.

use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_application_services::AXUIElement;

use glass_core::frame::{Frame, Region};
use glass_core::logbuf::Stream;
use glass_core::platform::{
    AppSpec, KeyEvent, Platform, PointerEvent, WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use glass_core::{GlassError, Result};

use crate::axwindow;
use crate::coords;
use crate::permissions;
use crate::process::{self, LogSink};

/// Poll interval between discovery attempts in [`MacosPlatform::discover_window`] —
/// matches `scwindow::find_window_for_pids`'s own poll cadence
/// (`poll_until(100, ..)`), which that loop takes over here so it can also race
/// against `child.try_wait()`.
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Timeout for the fresh per-call window re-resolution [`MacosPlatform::capture_frame`],
/// [`MacosPlatform::send_pointer`], and [`MacosPlatform::send_key`] each do on every call
/// (via `scwindow::find_window_by_id` when `active_window` is set, else
/// `scwindow::find_window_for_pids`). Short (unlike `start_app`'s `spec.timeout_ms`, which
/// waits for a brand-new window to first appear): the window is already known to have
/// existed as of the last successful call, so this only needs to cover the ordinary
/// query-round-trip latency, not a real "is the app even launching" wait.
const WINDOW_RESOLVE_TIMEOUT: Duration = Duration::from_millis(2000);

/// Read-back tolerance (pixels) [`MacosPlatform::window`]'s mutating ops use to decide
/// whether a `Move`/`Resize` actually took: `Move`'s final vs. requested position, and
/// `Resize`'s did-anything-happen-at-all check (see [`resize_was_refused`]'s doc — a
/// legitimate min/max-size clamp is not a refusal). `8`px mirrors `axwindow.rs`'s own
/// `FALLBACK_TOLERANCE_PX` — generous enough to absorb point<->pixel rounding across a
/// position/size round trip without masking a real refusal.
const WINDOW_OP_TOLERANCE_PX: i32 = 8;

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
    /// The active window's `CGWindowID` — the implicit target of `capture_frame`/
    /// `send_pointer`/`send_key`, per the `Platform` contract. `start_app` sets it to the
    /// first window discovered for the launched app; `select_window` (Plan 4 Task 5) is
    /// the only other place that changes it, and is exactly the "retargeting" the
    /// `Platform` contract describes — once an agent picks a different window, capture and
    /// input follow it. `None` only before any `start_app` call (or after `stop_app`),
    /// meaning "no window chosen yet"; every per-call resolver below falls back to the
    /// original first-on-screen-by-pid lookup in that case.
    active_window: Option<u32>,
}

impl MacosPlatform {
    /// Construct the backend, failing fast if a required TCC grant is missing.
    pub fn new() -> Result<Self> {
        permissions::preflight()?;
        Ok(Self {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: None,
            child: None,
            active_window: None,
        })
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

    /// Resolve the window `capture_frame`/`send_pointer`/`send_key` should target *this*
    /// call: `scwindow::find_window_by_id(active_window, ..)` once `select_window` (or
    /// `start_app`'s initial discovery) has set an active `CGWindowID` — the retargeting the
    /// `Platform` contract requires (see the `active_window` field's doc) — falling back to
    /// the pre-Plan-4 "first on-screen window for this pid" lookup when nothing is selected
    /// yet. Always fresh (never cached): mirrors `find_window_for_pids`'s own per-call
    /// re-resolution, since the window may have moved/resized/closed since the last call.
    fn resolve_active_window(&self, pid: i32) -> Result<crate::scwindow::WindowMatch> {
        match self.active_window {
            Some(id) => crate::scwindow::find_window_by_id(id, WINDOW_RESOLVE_TIMEOUT),
            None => crate::scwindow::find_window_for_pids(&[pid], WINDOW_RESOLVE_TIMEOUT),
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

/// Map one `scwindow::AppWindow` into the `WindowInfo` [`MacosPlatform::list_windows`]
/// returns, given the backend's current `active_window`. Factored out of `list_windows` as
/// a pure step so it's unit-testable without a live `SCShareableContent` query — runtime
/// enumeration coverage is Task 6's.
fn window_info_from(w: crate::scwindow::AppWindow, active_window: Option<u32>) -> WindowInfo {
    WindowInfo {
        id: WindowId(w.window_id as u64),
        title: w.title,
        class: w.application_name,
        geometry: w.geometry,
        active: Some(w.window_id) == active_window,
    }
}

/// Read `el`'s current `AXPosition`/`AXSize` (points) and convert to the pixel
/// `WindowGeometry` [`MacosPlatform::window`] returns for every op — the shared read-back
/// step after `Focus`/`Move`/`Resize`'s mutation, and the sole step for `Geometry`. Reuses
/// `coords::pixel_geometry_from_content_rect`'s point->pixel scaling rather than
/// reimplementing it: an AX position+size pair is exactly that function's `x`/`y`/`width`/
/// `height` args (see `coords.rs`'s module doc), so this crate keeps one scaling
/// implementation, not two.
fn read_ax_geometry(el: &AXUIElement, scale: f64) -> Result<WindowGeometry> {
    let (x, y) = axwindow::ax_position(el)?;
    let (width, height) = axwindow::ax_size(el)?;
    Ok(coords::pixel_geometry_from_content_rect(x, y, width, height, scale))
}

/// True if `geom`'s position is within [`WINDOW_OP_TOLERANCE_PX`] of a `Move { x, y }`
/// target — the signal `window(op)`'s `Move` branch uses to catch a macOS window that
/// silently ignores `AXPosition` (the no-silent-no-op contract `window(op)`'s doc
/// describes). Pure so it's unit-testable without a live `AXUIElement`.
fn move_took_effect(geom: &WindowGeometry, x: i32, y: i32) -> bool {
    (geom.x - x).abs() <= WINDOW_OP_TOLERANCE_PX && (geom.y - y).abs() <= WINDOW_OP_TOLERANCE_PX
}

/// True if a `Resize { width, height }` had no visible effect at all: `after` is (within
/// tolerance) the same size as `before`, even though `width`/`height` was a genuinely
/// different size than `before`'s own. This is deliberately narrower than "does `after`
/// exactly match the request": macOS may legitimately clamp to an intermediate size (a
/// window's min/max content-size constraint), which is expected behavior, not a bug — the
/// resulting `after` geometry is still returned to the caller in that case (see
/// `window(op)`'s `Resize` doc). Only a total no-op (the size never moved, despite a real
/// change being requested) is treated as the "macOS refused the resize" failure. Pure so
/// it's unit-testable without a live `AXUIElement`.
fn resize_was_refused(before: &WindowGeometry, after: &WindowGeometry, width: u32, height: u32) -> bool {
    let requested_a_change = (width as i32 - before.width as i32).abs() > WINDOW_OP_TOLERANCE_PX
        || (height as i32 - before.height as i32).abs() > WINDOW_OP_TOLERANCE_PX;
    let nothing_moved = (after.width as i32 - before.width as i32).abs() <= WINDOW_OP_TOLERANCE_PX
        && (after.height as i32 - before.height as i32).abs() <= WINDOW_OP_TOLERANCE_PX;
    requested_a_change && nothing_moved
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
                // NOTE: this seeds the active-window model (Plan 4 design decision 2): the
                // first window discovered for the launched app becomes the implicit target
                // of capture/input, exactly as `select_window` (a later task) will retarget
                // it to a different window later. `resolve_active_window` (used by
                // `capture_frame`/`send_pointer`/`send_key` below) is what actually honors
                // this field on every call.
                self.active_window = Some(m.window_id);
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

    /// Terminate the launched child (if any) and clear the active pid/window. Idempotent —
    /// a call with nothing running is `Ok(())`.
    fn stop_app(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child);
        }
        self.app_pid = None;
        self.active_window = None;
        Ok(())
    }

    /// Capture the active window as an RGBA8 frame, re-resolving it fresh on every call
    /// (see `scwindow.rs`'s module doc — a `Retained<SCWindow>` can't be cached across
    /// calls).
    ///
    /// NOTE: targets `active_window` when set (`capture::capture_window_by_id`, exact
    /// `CGWindowID` match) — the retargeting `select_window` (a later task) drives, per the
    /// `Platform` contract's "implicit target of capture/input" — else falls back to the
    /// pre-Plan-4 first-on-screen-window-for-this-pid path (`capture::capture_window`).
    /// `resolve_active_window`'s doc covers the shared decision; capture takes its own
    /// `capture_window`/`capture_window_by_id` branch (rather than calling
    /// `resolve_active_window` itself) because capture needs the live `SCWindow` inside a
    /// single completion-handler callback to build its `SCContentFilter`, not just the
    /// `WindowMatch` snapshot `resolve_active_window` returns.
    ///
    /// **Main-thread affinity:** like `start_app`, this reaches `ffi::app_kit_init()`
    /// (via `capture::capture_window`/`capture_window_by_id`) and must run on the true main
    /// thread; see the note on `start_app`.
    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        match self.active_window {
            Some(id) => crate::capture::capture_window_by_id(id, region),
            None => crate::capture::capture_window(&[pid as i32], region),
        }
    }
    /// Map the active window into `input::send_pointer` — see `input.rs`'s module doc for
    /// the CGEvent details and its main-thread-affinity note (shared with
    /// `start_app`/`capture_frame` above).
    ///
    /// NOTE: re-resolves the window fresh via `resolve_active_window` on every call —
    /// targeting `active_window` when set (the retargeting `select_window`, a later task,
    /// drives), else falling back to the pre-Plan-4 first-on-screen-window-for-this-pid
    /// path — mirroring `capture_frame`'s per-call resolution above. The window may have
    /// moved or resized since `start_app` (or any earlier `send_pointer` call), so a
    /// `scale`/`origin_pt`/geometry cached once at `start_app` would go stale. Both the
    /// bounds check (`check_pointer_bounds`, see its doc) and the coordinate mapping use
    /// this freshly-resolved geometry/scale/origin; the CGEvent focus target is the
    /// resolved window's own owning pid (`m.pid`, from the fresh `SCShareableContent`
    /// lookup), not necessarily `self.app_pid` — today they're always the same pid (glass
    /// launches one app per session), but `m.pid` is the one actually tied to the window
    /// being clicked.
    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let m = self.resolve_active_window(pid as i32)?;
        check_pointer_bounds(event, &m.geometry)?;
        crate::input::send_pointer(event, m.pid, m.scale, m.origin_pt)
    }
    /// Map the active window into `input::send_key` — see `input.rs`'s module doc for the
    /// CGEvent keyboard details.
    ///
    /// NOTE: re-resolves the active window via `resolve_active_window` first, same as
    /// `send_pointer`, even though keyboard CGEvents target a *process* (`focus(pid)`
    /// activates the app, and the posted event then goes to whatever window that app
    /// currently has key/main — there is no per-window keyboard targeting yet; that needs
    /// AXUIElement raise/main, Plan 4 Task 4). The resolution here still matters: if
    /// `active_window` is set but that window has closed, this surfaces
    /// `GlassError::WindowNotFound` instead of silently posting keys to whatever else
    /// happens to be focused — the same no-silent-wrong-target discipline `send_pointer`'s
    /// bounds check enforces for clicks.
    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let m = self.resolve_active_window(pid as i32)?;
        crate::input::send_key(event, m.pid)
    }
    /// Resolve the active window's `AXUIElement` (fresh every call, same rationale as
    /// `resolve_active_window`: the window may have moved/resized/closed since the last
    /// call) and dispatch `op` onto it:
    ///
    /// - `Focus` activates the owning app (`input::focus`, the same
    ///   `NSRunningApplication::activate` call `send_pointer`/`send_key` already make),
    ///   then `AXRaise`s and marks the window `AXMain` — CGEvents alone can't target a
    ///   specific window (see `send_key`'s doc), this is what actually does.
    /// - `Move { x, y }` converts the target from global-screen PIXELS to Quartz POINTS
    ///   (`coords::global_pixel_to_point`) and sets `AXPosition`.
    /// - `Resize { width, height }` sets `AXSize`, then re-sets `AXPosition` to its
    ///   just-read current value, then sets `AXSize` again — the proven
    ///   `window_ops.swift` workaround for a window that won't grow past its current
    ///   on-screen bounds via a single `AXSize` set alone.
    /// - `Geometry` performs no mutation.
    ///
    /// Every branch reads back the window's position/size afterward
    /// ([`read_ax_geometry`]) and returns it in pixels — the tool boundary's unit. `Move`/
    /// `Resize` additionally check the read-back actually reflects the request
    /// ([`move_took_effect`]/[`resize_was_refused`]), returning `GlassError::Backend`
    /// naming what didn't take rather than silently reporting success on a macOS window
    /// that refused the change; `Focus`/`Geometry` have no such check since they assert
    /// nothing about position/size.
    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        permissions::preflight()?;
        let id = self.active_window.ok_or(GlassError::NoActiveSession)?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;

        let m = crate::scwindow::find_window_by_id(id, WINDOW_RESOLVE_TIMEOUT)?;
        let el = axwindow::ax_window_for_cgwindowid(pid as i32, id, m.geometry.clone(), m.scale)?;

        match *op {
            WindowOp::Focus => {
                crate::input::focus(pid as i32);
                axwindow::ax_raise(&el)?;
                axwindow::ax_set_main(&el)?;
                read_ax_geometry(&el, m.scale)
            }
            WindowOp::Move { x, y } => {
                let target_pt = coords::global_pixel_to_point((x, y), m.scale);
                axwindow::ax_set_position(&el, target_pt)?;
                let geom = read_ax_geometry(&el, m.scale)?;
                if !move_took_effect(&geom, x, y) {
                    return Err(GlassError::Backend(format!(
                        "window move to ({x},{y}) px did not take; window is at ({},{})",
                        geom.x, geom.y
                    )));
                }
                Ok(geom)
            }
            WindowOp::Resize { width, height } => {
                let target_size_pt = (width as f64 / m.scale, height as f64 / m.scale);
                axwindow::ax_set_size(&el, target_size_pt)?;
                let pos = axwindow::ax_position(&el)?;
                axwindow::ax_set_position(&el, pos)?;
                axwindow::ax_set_size(&el, target_size_pt)?;
                let geom = read_ax_geometry(&el, m.scale)?;
                if resize_was_refused(&m.geometry, &geom, width, height) {
                    return Err(GlassError::Backend(format!(
                        "window resize to {width}x{height} px was refused; window remains {}x{}",
                        geom.width, geom.height
                    )));
                }
                Ok(geom)
            }
            WindowOp::Geometry => read_ax_geometry(&el, m.scale),
        }
    }
    /// Enumerate every on-screen window owned by the launched app's pid via
    /// `scwindow::list_app_windows` (one `SCShareableContent` query, all matches — not just
    /// the active one), mapping each into a `WindowInfo` via [`window_info_from`].
    ///
    /// **Main-thread affinity:** like `start_app`/`capture_frame`, reaches
    /// `ffi::app_kit_init()` (via `scwindow::list_app_windows`) and must run on the true
    /// main thread; see the note on `start_app`.
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let windows = crate::scwindow::list_app_windows(&[pid as i32])?;
        Ok(windows.into_iter().map(|w| window_info_from(w, self.active_window)).collect())
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
            active_window: None,
        };
        assert_eq!(p.drain_logs().len(), 1);
        assert!(p.drain_logs().is_empty());
    }

    #[test]
    fn app_pid_returns_the_constructed_value() {
        let p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: Some(42),
            child: None,
            active_window: None,
        };
        assert_eq!(p.app_pid(), Some(42));
    }

    #[test]
    fn stop_app_on_a_fresh_platform_is_idempotent() {
        // No child stored: stop_app must not panic (e.g. on an unwrap of a live process
        // handle) and must succeed — this path never touches AppKit, so it's exercisable
        // off the Mac-gated suite's main-thread test.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: None,
            child: None,
            active_window: None,
        };
        assert!(p.stop_app().is_ok());
        assert!(p.stop_app().is_ok(), "a second call must also be Ok");
        assert_eq!(p.app_pid(), None);
    }

    #[test]
    fn stop_app_clears_active_window() {
        // start_app seeds active_window (Plan 4 Task 1); stop_app must clear it too, so a
        // later start_app on the same MacosPlatform never leaks a stale CGWindowID from a
        // previous session into resolve_active_window.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: Some(42),
            child: None,
            active_window: Some(7),
        };
        assert!(p.stop_app().is_ok());
        assert_eq!(p.active_window, None);
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

    #[test]
    fn window_info_from_marks_the_active_window() {
        let w = crate::scwindow::AppWindow {
            window_id: 7,
            geometry: WindowGeometry { x: 1, y: 2, width: 640, height: 480 },
            title: Some("Untitled".into()),
            application_name: Some("TestApp".into()),
        };
        let info = window_info_from(w.clone(), Some(7));
        assert_eq!(info.id, WindowId(7));
        assert_eq!(info.title, Some("Untitled".into()));
        assert_eq!(info.class, Some("TestApp".into()));
        assert_eq!(info.geometry, WindowGeometry { x: 1, y: 2, width: 640, height: 480 });
        assert!(info.active, "window_id matches active_window");

        let not_active = window_info_from(w, Some(8));
        assert!(!not_active.active, "window_id does not match a different active_window");
    }

    #[test]
    fn window_info_from_is_not_active_when_no_window_is_selected() {
        let w = crate::scwindow::AppWindow {
            window_id: 7,
            geometry: WindowGeometry { x: 0, y: 0, width: 100, height: 100 },
            title: None,
            application_name: None,
        };
        let info = window_info_from(w, None);
        assert!(!info.active);
        assert_eq!(info.title, None);
        assert_eq!(info.class, None);
    }

    #[test]
    fn move_took_effect_accepts_exact_and_within_tolerance() {
        let geom = WindowGeometry { x: 100, y: 200, width: 640, height: 480 };
        assert!(move_took_effect(&geom, 100, 200), "exact match");
        assert!(move_took_effect(&geom, 104, 196), "within tolerance");
        assert!(move_took_effect(&geom, 92, 208), "within tolerance, other direction");
    }

    #[test]
    fn move_took_effect_rejects_a_window_that_did_not_move() {
        // The refusal case: AXSetAttributeValue(AXPosition) reported success but the
        // window is still sitting wherever it started.
        let geom = WindowGeometry { x: 100, y: 200, width: 640, height: 480 };
        assert!(!move_took_effect(&geom, 500, 200), "x off by more than tolerance");
        assert!(!move_took_effect(&geom, 100, 600), "y off by more than tolerance");
    }

    #[test]
    fn resize_was_refused_when_size_never_moves() {
        let before = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        let after = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        // Requested a real size change (800x600) but the window is still exactly where it
        // started — the silent-no-op case `window(op)`'s Resize branch must catch.
        assert!(resize_was_refused(&before, &after, 800, 600));
    }

    #[test]
    fn resize_was_refused_is_false_when_nothing_was_requested() {
        // width/height happen to equal `before`'s own size (e.g. a Resize to the current
        // size) — no change was requested, so an unchanged `after` is not a refusal.
        let before = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        let after = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        assert!(!resize_was_refused(&before, &after, 640, 480));
    }

    #[test]
    fn resize_was_refused_is_false_on_a_legitimate_clamp() {
        // macOS clamped to an intermediate size short of the request (e.g. a min-size
        // constraint) rather than ignoring the resize outright — expected behavior, not a
        // refusal; `window(op)` returns this actual geometry rather than erroring.
        let before = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        let after = WindowGeometry { x: 0, y: 0, width: 700, height: 480 };
        assert!(!resize_was_refused(&before, &after, 1200, 480));
    }

    #[test]
    fn resize_was_refused_is_false_when_the_read_back_matches_the_request() {
        let before = WindowGeometry { x: 0, y: 0, width: 640, height: 480 };
        let after = WindowGeometry { x: 0, y: 0, width: 800, height: 600 };
        assert!(!resize_was_refused(&before, &after, 800, 600));
    }
}
