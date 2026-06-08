//! The Windows `Platform` backend for glass (WGC capture + SendInput).
//!
//! v1 drives the interactive desktop. The OS-touching modules and the
//! `WindowsPlatform` impl are gated per-item with `#[cfg(windows)]` (not a
//! crate-level gate) so the pure [`dpi`] coordinate math still compiles and is
//! unit-tested on the Linux dev box. Off Windows the crate exposes only `dpi`.

pub mod dpi; // pure coordinate math — cross-platform, unit-tested on the Linux dev box
pub mod doctor; // pure check-mapping cross-platform; Windows fact-gathering is cfg(windows)
pub mod jobpids; // pure JOBOBJECT_BASIC_PROCESS_ID_LIST byte parser — Miri'd on the host
pub mod jobcfg; // pure SandboxLevel -> job-limit descriptor — unit-tested on the Linux dev box
pub mod containment; // Windows containment provider seam (pure config is host-tested)
pub mod pixels; // pure BGRA->RGBA swizzle — cross-platform, unit-tested on the Linux dev box
pub mod vkmap; // pure named-keysym->VK map — cross-platform, unit-tested on the Linux dev box

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod util;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use backend::WindowsPlatform;

#[cfg(windows)]
mod backend {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use glass_core::frame::{Frame, Region};
    use glass_core::logbuf::Stream;
    use glass_core::platform::{
        AppSpec, KeyEvent, Platform, PointerEvent, WindowGeometry, WindowHint, WindowId, WindowInfo,
        WindowOp,
    };
    use glass_core::{GlassError, Result};

    use crate::containment::{Launched, LogSink};
    use crate::display::{DisplayProvider, ExistingDesktop};
    use crate::windows::{
        app_window_infos, find_app_window, focus_window, geometry_of, move_window, resize_window,
    };

    /// The Windows `Platform` backend (v1: drives the interactive desktop).
    pub struct WindowsPlatform {
        /// How the target app's display is provisioned. v1 = `ExistingDesktop`; a
        /// headless `VirtualDisplay` provider is a deferred follow-on plan.
        display: Box<dyn DisplayProvider + Send>,
        /// The launched, contained app (provider-wrapped: today only `Unconfined`,
        /// a CREATE_SUSPENDED root in a `KILL_ON_JOB_CLOSE` Job).
        /// `None` until `start_app`; dropped/killed on `stop_app` and `Drop`.
        app: Option<Launched>,
        /// Lines drained by `drain_logs`, pushed by the per-stream reader threads.
        logs: LogSink,
        /// The active window, stored as a raw `HWND` (`isize`) so the backend stays
        /// `Send` (a raw `*mut c_void` is not). Reconstruct with
        /// [`crate::util::raw_to_hwnd`] at the point of use. `None` until window
        /// discovery (here) or select (Task 6) sets it.
        active_hwnd: Option<isize>,
    }

    impl WindowsPlatform {
        pub fn new() -> Result<Self> {
            Ok(Self {
                display: Box::new(ExistingDesktop),
                app: None,
                logs: Arc::new(Mutex::new(Vec::new())),
                active_hwnd: None,
            })
        }

        /// Poll the desktop's top-level windows until the discovery ladder
        /// ([`find_app_window`]) yields a window — the app's *process set* (the Job PID
        /// list ∪ the Toolhelp walk — Electron/Java hand windows to children) first, then
        /// a title/class-hint fallback — or `timeout_ms` elapses. Sets `active_hwnd` and
        /// returns the window's DWM frame geometry.
        fn discover_window(
            &mut self,
            hint: Option<&WindowHint>,
            timeout_ms: u64,
        ) -> Result<WindowGeometry> {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                // Fail fast if the app died on launch (crash/instant exit), rather than
                // busy-polling the full timeout and reporting a misleading Timeout.
                if let Some(app) = self.app.as_mut() {
                    if let Ok(Some(status)) = app.try_wait() {
                        return Err(GlassError::AppExited(status.code()));
                    }
                }
                // Recompute the union each iteration: a launcher-handoff child may only
                // appear (and own the window) several polls in.
                let pids = self.app_pids();
                if let Some(w) = find_app_window(&pids, hint) {
                    // A window passed the filter but has no DWM frame bounds yet (a transient
                    // splash destroyed mid-startup): don't fail — keep polling for the real one.
                    if let Some(r) = crate::util::extended_frame_bounds(w.hwnd()) {
                        self.active_hwnd = Some(w.raw);
                        return Ok(crate::windows::rect_to_geometry(r));
                    }
                }
                if Instant::now() >= deadline {
                    return Err(GlassError::Timeout(timeout_ms));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Platform for WindowsPlatform {
        fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
            // Resolve the containment provider before doing any work. `off` → Unconfined
            // (today's direct spawn); `default`/`strict` require an in-OS provider and
            // fail closed while Sandboxie availability is stubbed false (a later task).
            let containment = crate::containment::resolve_containment(spec)?;
            containment.run_build(spec)?;
            // Validate a usable display before launching — reject a degenerate 0x0
            // (headless / Session-0) where no window can ever appear.
            let disp = self.display.ensure()?;
            if disp.width == 0 || disp.height == 0 {
                return Err(GlassError::Backend(
                    "no usable interactive display (0x0 virtual screen) — headless/Session-0 \
                     context? a VirtualDisplay provider is a follow-on plan"
                        .into(),
                ));
            }
            // Launch via the provider: it spawns suspended-in-Job, wires log readers
            // before resuming, then resumes.
            let app = containment.launch(spec, self.logs.clone())?;
            self.app = Some(app);
            match self.discover_window(spec.window_hint.as_ref(), spec.timeout_ms) {
                Ok(geo) => Ok(geo),
                Err(e) => {
                    // Window never appeared (or geometry failed): don't orphan the tree.
                    if let Some(app) = self.app.take() {
                        app.kill();
                    }
                    self.active_hwnd = None;
                    Err(e)
                }
            }
        }

        fn stop_app(&mut self) -> Result<()> {
            if let Some(app) = self.app.take() {
                app.kill();
            }
            self.active_hwnd = None;
            Ok(())
        }

        fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
            let raw = self.active_hwnd.ok_or_else(|| {
                GlassError::CaptureFailed(
                    "no active window; start an app or select a window first".into(),
                )
            })?;
            crate::capture::capture_window(crate::util::raw_to_hwnd(raw), region)
        }

        fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
            let raw = self.active_hwnd.ok_or(GlassError::WindowNotFound)?;
            crate::input::send_pointer(raw, event)
        }

        fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
            let raw = self.active_hwnd.ok_or(GlassError::WindowNotFound)?;
            crate::input::send_key(raw, event)
        }

        fn get_clipboard(&mut self) -> Result<String> {
            crate::clipboard::get()
        }

        fn set_clipboard(&mut self, text: &str) -> Result<()> {
            crate::clipboard::set(text)
        }

        fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
            let raw = self.active_hwnd.ok_or(GlassError::WindowNotFound)?;
            let hwnd = crate::util::raw_to_hwnd(raw);
            match *op {
                WindowOp::Focus => focus_window(hwnd)?,
                WindowOp::Move { x, y } => move_window(hwnd, x, y)?,
                WindowOp::Resize { width, height } => resize_window(hwnd, width, height)?,
                WindowOp::Geometry => {}
            }
            // Re-read the resulting geometry (the op may have moved/resized the frame).
            geometry_of(hwnd)
        }

        fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
            // No active app -> WindowNotFound, never an empty list (mirrors x11).
            if self.app.is_none() {
                return Err(GlassError::WindowNotFound);
            }
            let pids = self.app_pids();
            let mut out = Vec::new();
            for w in app_window_infos(&pids) {
                out.push(WindowInfo {
                    id: WindowId(w.raw as u64),
                    title: (!w.title.is_empty()).then(|| w.title.clone()),
                    class: (!w.class.is_empty()).then(|| w.class.clone()),
                    geometry: geometry_of(w.hwnd())?,
                    active: Some(w.raw) == self.active_hwnd,
                });
            }
            Ok(out)
        }

        fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
            if self.app.is_none() {
                return Err(GlassError::WindowNotFound);
            }
            let pids = self.app_pids();
            let raw = id.0 as isize;
            // Validate against the current app-window set (stronger than a bare IsWindow,
            // and matches x11): only switch to a window the app actually owns right now.
            if app_window_infos(&pids).iter().any(|w| w.raw == raw) {
                self.active_hwnd = Some(raw);
                geometry_of(crate::util::raw_to_hwnd(raw))
            } else {
                Err(GlassError::WindowNotFound)
            }
        }

        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            std::mem::take(&mut *self.logs.lock().unwrap())
        }

        fn app_pid(&self) -> Option<u32> {
            self.app.as_ref().map(|a| a.root_pid())
        }

        /// The launched app's process set. Fully resolved per provider by
        /// [`Launched::pids`] (Job list ∪ descendant walk ∪ root for Unconfined; `/listpids` ∪
        /// descendant walk for Sandboxie) — this just delegates. Empty if no app is launched.
        fn app_pids(&self) -> Vec<u32> {
            self.app.as_ref().map(|a| a.pids()).unwrap_or_default()
        }
    }

    impl Drop for WindowsPlatform {
        fn drop(&mut self) {
            if let Some(app) = self.app.take() {
                app.kill();
            }
        }
    }
}
