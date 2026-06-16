use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, PointerEvent, Region, Result, Stream, WindowGeometry,
    WindowId, WindowInfo, WindowOp,
};
use glass_core::Platform;

use crate::adb::Adb;
use crate::agent::{AgentClient, AgentRegistry};
use crate::build::run_build;
use crate::input::{AgentInjector, Injector, ShellInjector};
use crate::cmd::{force_stop_args, install_args, launch_args, parse_launch};
use crate::logs::{LogSink, LogcatStream};
use crate::parse::{check_am_start, check_install, parse_app_windows, parse_pid, parse_pids};
use crate::screencap::decode_screencap;
use crate::target::AdbTarget;

const CLIPBOARD_NEEDS_AGENT: &str =
    "clipboard (needs the on-device agent; set GLASS_ANDROID_AGENT_JAR)";

/// The single foreground app this backend drives.
struct RunningApp {
    package: String,
    component: String,
    pid: Option<u32>,
    active_id: WindowId,
    window: WindowGeometry,
    logcat: Option<LogcatStream>,
}

/// Drives a native Android app in an AVD over `adb`.
pub struct AndroidPlatform {
    target: Box<dyn AdbTarget + Send>,
    injector: Box<dyn Injector + Send>,
    agent: Option<Arc<AgentClient>>,
    logs: LogSink,
    app: Option<RunningApp>,
}

impl AndroidPlatform {
    /// Attach to (or boot) an emulator, and connect the on-device agent if enabled.
    pub fn from_env(
        emulators: &crate::avd::EmulatorRegistry,
        agents: &AgentRegistry,
    ) -> Result<Self> {
        let base = Adb::from_env();
        let target = crate::target::resolve(base, emulators)?;

        // Best-effort: use the agent when enabled; on any failure, fall back to adb paths.
        let get = |k: &str| std::env::var(k).ok();
        let agent = if crate::agent::agent_enabled(&get) {
            match agents.ensure(target.adb()).and_then(AgentClient::connect) {
                Ok(client) => Some(Arc::new(client)),
                Err(e) => {
                    eprintln!("glass-android: agent unavailable, using adb fallback: {e}");
                    None
                }
            }
        } else {
            None
        };

        let injector: Box<dyn Injector + Send> = match &agent {
            Some(a) => Box::new(AgentInjector { agent: a.clone() }),
            None => Box::new(ShellInjector),
        };

        Ok(Self {
            target: Box::new(target),
            injector,
            agent,
            logs: Arc::new(Mutex::new(Vec::new())),
            app: None,
        })
    }

    fn adb(&self) -> &Adb {
        self.target.adb()
    }

    fn running(&self) -> Result<&RunningApp> {
        self.app.as_ref().ok_or(GlassError::NoActiveSession)
    }

    /// The resolved, serial-bound adb client — so the a11y reader drives the *same* device
    /// the platform resolved (possibly a freshly booted AVD `choose_serial` can't disambiguate).
    pub fn resolved_adb(&self) -> Adb {
        self.target.adb().clone()
    }

    /// Re-read the active window's current on-screen frame before capturing — a rotation or
    /// layout change can move/resize it since it was cached. Best-effort: keeps the cached
    /// geometry if the window isn't currently listed (mirrors `app_pids`' live re-scan).
    fn refresh_window(&mut self) -> Result<WindowGeometry> {
        let (package, active_id) = {
            let app = self.running()?;
            (app.package.clone(), app.active_id)
        };
        let dump = self.adb().run(["shell", "dumpsys", "window", "windows"])?;
        let parsed = parse_app_windows(&dump, &package);
        let fresh = parsed
            .iter()
            .find(|w| WindowId(w.id) == active_id)
            .or_else(|| parsed.first())
            .map(|w| w.frame.clone());
        match fresh {
            Some(frame) => {
                if let Some(app) = self.app.as_mut() {
                    app.window = frame.clone();
                }
                Ok(frame)
            }
            None => Ok(self.running()?.window.clone()),
        }
    }

    /// Poll `dumpsys window windows` until the app has an on-screen window, returning the
    /// topmost one's id + frame (the default active window).
    fn discover_window(&self, package: &str, timeout_ms: u64) -> Result<(WindowId, WindowGeometry)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let dump = self.adb().run(["shell", "dumpsys", "window", "windows"])?;
            if let Some(w) = parse_app_windows(&dump, package).into_iter().next() {
                return Ok((WindowId(w.id), w.frame));
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Timeout(timeout_ms));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

/// Intersect the window rect with the captured display, so a window that extends past a
/// screen edge (or whose cached geometry is stale-larger after a rotation) yields its
/// *visible* portion instead of failing `crop`. Errors only when nothing is on-screen.
fn visible_window_region(win: &WindowGeometry, disp_w: u32, disp_h: u32) -> Result<Region> {
    let x0 = win.x.max(0) as i64;
    let y0 = win.y.max(0) as i64;
    let x1 = (win.x as i64 + win.width as i64).min(disp_w as i64);
    let y1 = (win.y as i64 + win.height as i64).min(disp_h as i64);
    let (w, h) = (x1 - x0, y1 - y0);
    if w <= 0 || h <= 0 {
        return Err(GlassError::CaptureFailed(format!(
            "window {}x{} at ({},{}) is entirely off the {disp_w}x{disp_h} screen",
            win.width, win.height, win.x, win.y
        )));
    }
    Ok(Region { x: x0 as u32, y: y0 as u32, width: w as u32, height: h as u32 })
}

impl Platform for AndroidPlatform {
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        run_build(spec, &self.logs)?;
        let target = parse_launch(&spec.run)?;
        let adb = self.adb().clone();

        if let Some(apk) = &target.apk {
            let installed = adb.run(install_args(apk).iter().map(String::as_str))?;
            check_install(&installed)?;
        }

        let started = adb.run(launch_args(&target.component).iter().map(String::as_str))?;
        check_am_start(&started)?;

        let (active_id, window) = self.discover_window(&target.package, spec.timeout_ms)?;

        let pidof = adb.run(["shell", "pidof", &target.package]).unwrap_or_default();
        let pid = parse_pid(&pidof);
        let logcat = match pid {
            Some(pid) => Some(LogcatStream::spawn(&adb, pid, self.logs.clone())?),
            None => None,
        };

        self.app = Some(RunningApp {
            package: target.package,
            component: target.component,
            pid,
            active_id,
            window: window.clone(),
            logcat,
        });
        Ok(window)
    }

    fn stop_app(&mut self) -> Result<()> {
        if let Some(mut app) = self.app.take() {
            if let Some(mut logcat) = app.logcat.take() {
                logcat.stop();
            }
            let _ = self.adb().run(force_stop_args(&app.package).iter().map(String::as_str));
        }
        Ok(())
    }

    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        let win = self.refresh_window()?;
        let bytes = self.adb().run_bytes(["exec-out", "screencap"])?;
        let display = decode_screencap(&bytes)?;
        let window_region = visible_window_region(&win, display.width, display.height)?;
        let window_frame = display.crop(&window_region)?;
        match region {
            Some(r) => window_frame.crop(r),
            None => Ok(window_frame),
        }
    }

    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        let origin = self.running()?.window.clone();
        self.injector.pointer(self.target.adb(), &origin, event)
    }

    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.running()?; // require an active session
        self.injector.key(self.target.adb(), event)
    }

    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        match op {
            WindowOp::Geometry => Ok(self.running()?.window.clone()),
            WindowOp::Focus => {
                let (component, package) = {
                    let app = self.running()?;
                    (app.component.clone(), app.package.clone())
                };
                let out = self.adb().run(launch_args(&component).iter().map(String::as_str))?;
                check_am_start(&out)?;
                let (active_id, window) = self.discover_window(&package, 5_000)?;
                let app = self.app.as_mut().ok_or(GlassError::NoActiveSession)?;
                app.active_id = active_id;
                app.window = window.clone();
                Ok(window)
            }
            WindowOp::Resize { .. } | WindowOp::Move { .. } => Err(GlassError::Unsupported(
                "window resize/move (Android apps are full-screen)".into(),
            )),
        }
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let (package, active_id) = {
            let app = self.running()?;
            (app.package.clone(), app.active_id)
        };
        let dump = self.adb().run(["shell", "dumpsys", "window", "windows"])?;
        let parsed = parse_app_windows(&dump, &package);
        let any_match = parsed.iter().any(|w| WindowId(w.id) == active_id);
        Ok(parsed
            .into_iter()
            .enumerate()
            .map(|(i, w)| WindowInfo {
                id: WindowId(w.id),
                title: Some(w.title),
                class: Some(package.clone()),
                geometry: w.frame,
                active: if any_match { WindowId(w.id) == active_id } else { i == 0 },
            })
            .collect())
    }

    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        let package = self.running()?.package.clone();
        let dump = self.adb().run(["shell", "dumpsys", "window", "windows"])?;
        let found = parse_app_windows(&dump, &package)
            .into_iter()
            .find(|w| WindowId(w.id) == id);
        match found {
            Some(w) => {
                let app = self.app.as_mut().ok_or(GlassError::NoActiveSession)?;
                app.active_id = id;
                app.window = w.frame.clone();
                Ok(w.frame)
            }
            None => Err(GlassError::WindowNotFound),
        }
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.logs.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
    }

    fn get_clipboard(&mut self) -> Result<String> {
        match &self.agent {
            Some(a) => a.clipboard_get(),
            None => Err(GlassError::Unsupported(CLIPBOARD_NEEDS_AGENT.into())),
        }
    }

    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        match &self.agent {
            Some(a) => a.clipboard_set(text),
            None => Err(GlassError::Unsupported(CLIPBOARD_NEEDS_AGENT.into())),
        }
    }

    fn app_pid(&self) -> Option<u32> {
        self.app.as_ref().and_then(|a| a.pid)
    }

    fn app_pids(&self) -> Vec<u32> {
        // Best-effort live re-scan; falls back to the single known pid.
        if let Some(app) = &self.app {
            let out = self.adb().run(["shell", "pidof", &app.package]).unwrap_or_default();
            let pids = parse_pids(&out);
            if !pids.is_empty() {
                return pids;
            }
        }
        self.app_pid().into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_region_full_onscreen_is_identity() {
        let win = WindowGeometry { x: 0, y: 0, width: 1080, height: 2400 };
        let r = visible_window_region(&win, 1080, 2400).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 1080, 2400));
    }

    #[test]
    fn visible_window_region_clamps_negative_origin() {
        let win = WindowGeometry { x: -10, y: -20, width: 1080, height: 2400 };
        let r = visible_window_region(&win, 1080, 2400).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 1070, 2380));
    }

    #[test]
    fn visible_window_region_clamps_overhang() {
        let win = WindowGeometry { x: 1000, y: 0, width: 200, height: 100 };
        let r = visible_window_region(&win, 1080, 720).unwrap();
        assert_eq!((r.x, r.width), (1000, 80));
        assert_eq!((r.y, r.height), (0, 100));
    }

    #[test]
    fn visible_window_region_errors_when_fully_offscreen() {
        let win = WindowGeometry { x: 2000, y: 0, width: 100, height: 100 };
        assert!(visible_window_region(&win, 1080, 720).is_err());
    }
}
