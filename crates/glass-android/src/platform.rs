use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, PointerEvent, Region, Result, Stream, WindowGeometry,
    WindowId, WindowInfo, WindowOp,
};
use glass_core::Platform;

use crate::adb::Adb;
use crate::build::run_build;
use crate::cmd::{force_stop_args, install_args, launch_args, parse_launch};
use crate::logs::{LogSink, LogcatStream};
use crate::parse::{check_am_start, check_install, parse_pid, parse_pids, parse_window_frame};
use crate::screencap::decode_screencap;
use crate::target::{AdbTarget, AttachedDevice};

/// The single foreground app this backend drives.
struct RunningApp {
    package: String,
    component: String,
    pid: Option<u32>,
    window: WindowGeometry,
    logcat: Option<LogcatStream>,
}

/// Drives a native Android app in an AVD over `adb`.
pub struct AndroidPlatform {
    target: Box<dyn AdbTarget + Send>,
    logs: LogSink,
    app: Option<RunningApp>,
}

impl AndroidPlatform {
    /// Attach to a running emulator (serial from `GLASS_ANDROID_SERIAL`, else the sole device).
    pub fn from_env() -> Result<Self> {
        let base = Adb::from_env();
        let serial = std::env::var("GLASS_ANDROID_SERIAL").ok();
        let target = AttachedDevice::resolve(base, serial.as_deref())?;
        Ok(Self { target: Box::new(target), logs: Arc::new(Mutex::new(Vec::new())), app: None })
    }

    fn adb(&self) -> &Adb {
        self.target.adb()
    }

    fn running(&self) -> Result<&RunningApp> {
        self.app.as_ref().ok_or(GlassError::NoActiveSession)
    }

    /// Poll `dumpsys window windows` until the app's frame appears or `timeout_ms` elapses.
    fn discover_window(&self, package: &str, timeout_ms: u64) -> Result<WindowGeometry> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let dump = self.adb().run(["shell", "dumpsys", "window", "windows"])?;
            if let Ok(geo) = parse_window_frame(&dump, package) {
                return Ok(geo);
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Timeout(timeout_ms));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn window_id(&self) -> WindowId {
        // One foreground window; a stable-per-session id is sufficient.
        WindowId(1)
    }
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

        let window = self.discover_window(&target.package, spec.timeout_ms)?;

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
        let win = self.running()?.window.clone();
        let bytes = self.adb().run_bytes(["exec-out", "screencap"])?;
        let display = decode_screencap(&bytes)?;
        let window_region = Region {
            x: win.x.max(0) as u32,
            y: win.y.max(0) as u32,
            width: win.width,
            height: win.height,
        };
        let window_frame = display.crop(&window_region)?;
        match region {
            Some(r) => window_frame.crop(r),
            None => Ok(window_frame),
        }
    }

    fn send_pointer(&mut self, _event: &PointerEvent) -> Result<()> {
        Err(GlassError::Unsupported(
            "pointer input on the android backend (arrives in P2)".into(),
        ))
    }

    fn send_key(&mut self, _event: &KeyEvent) -> Result<()> {
        Err(GlassError::Unsupported(
            "keyboard input on the android backend (arrives in P2)".into(),
        ))
    }

    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let app = self.running()?;
        match op {
            WindowOp::Geometry => Ok(app.window.clone()),
            WindowOp::Focus => {
                let component = app.component.clone();
                let out = self.adb().run(launch_args(&component).iter().map(String::as_str))?;
                check_am_start(&out)?;
                Ok(self.running()?.window.clone())
            }
            WindowOp::Resize { .. } | WindowOp::Move { .. } => Err(GlassError::Unsupported(
                "window resize/move (Android apps are full-screen)".into(),
            )),
        }
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let app = self.running()?;
        Ok(vec![WindowInfo {
            id: self.window_id(),
            title: Some(app.component.clone()),
            class: Some(app.package.clone()),
            geometry: app.window.clone(),
            active: true,
        }])
    }

    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        if id == self.window_id() {
            Ok(self.running()?.window.clone())
        } else {
            Err(GlassError::WindowNotFound)
        }
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.logs.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
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
