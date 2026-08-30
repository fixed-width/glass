use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::Deadline;
use glass_core::Platform;
use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, PointerEvent, Region, Result, Stream, WindowGeometry,
    WindowId, WindowInfo, WindowOp,
};

use crate::a11y::AndroidA11y;
use crate::a11y_service::{A11yServiceRegistry, ServiceA11y};
use crate::adb::Adb;
use crate::agent::{AgentClient, AgentRegistry};
use crate::build::run_build;
use crate::cmd::{force_stop_args, install_args, launch_args, parse_launch};
use crate::input::{AgentInjector, Injector, ShellInjector};
use crate::logs::{LogSink, LogcatStream};
use crate::parse::{
    check_am_start, check_install, describe_missing_window, parse_app_windows, parse_pid,
    parse_pids,
};
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
    #[cfg(all(test, unix))]
    window_list_parse_delay: Option<Duration>,
}

impl AndroidPlatform {
    #[cfg(all(test, unix))]
    fn with_window_list_parse_delay(mut self, delay: Duration) -> Self {
        self.window_list_parse_delay = Some(delay);
        self
    }

    /// Stop the app, under `deadline` when the caller has one to share.
    ///
    /// The logcat reap comes first and is unbounded, measured at 0-1ms — a kill and reap of a
    /// direct child, which only uninterruptible sleep can hold up.
    fn stop_app_until(&mut self, deadline: Deadline) -> Result<()> {
        if let Some(mut app) = self.app.take() {
            if let Some(mut logcat) = app.logcat.take() {
                logcat.stop();
            }
            let stopped = self.adb().run_until(
                force_stop_args(&app.package).iter().map(String::as_str),
                deadline,
            );
            glass_core::note_if_skipped("the app force-stop", &stopped);
        }
        Ok(())
    }

    /// Attach to (or boot) an emulator, and connect the on-device agent if enabled.
    pub fn from_env(
        emulators: &crate::avd::EmulatorRegistry,
        agents: &AgentRegistry,
    ) -> Result<Self> {
        let get = |k: &str| std::env::var(k).ok();
        let base = Adb::from_env();
        let target = crate::target::resolve(base, emulators, &get)?;

        // Best-effort: use the agent when enabled; on any failure, fall back to adb paths.
        let agent = if crate::agent::agent_enabled(&get) {
            match agents
                .ensure(target.adb(), &get)
                .and_then(AgentClient::connect)
            {
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
            #[cfg(all(test, unix))]
            window_list_parse_delay: None,
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

    /// This device's accessibility reader: the Compose-rich [`ServiceA11y`] when the companion
    /// APK resolves and its service installs and connects, else the basic `uiautomator`
    /// [`AndroidA11y`]. Never absent — `uiautomator` needs nothing beyond `adb`.
    ///
    /// The input agent, the other optional companion, degrades the same way in
    /// [`AndroidPlatform::from_env`].
    pub fn accessibility(
        &self,
        services: &A11yServiceRegistry,
    ) -> Box<dyn glass_core::Accessibility + Send> {
        let get = |k: &str| std::env::var(k).ok();
        let Some(apk) = crate::a11y_service::a11y_apk(&get) else {
            return Box::new(AndroidA11y::for_adb(self.resolved_adb()));
        };
        match services.ensure(&self.resolved_adb(), &apk) {
            // The package isn't known until start_app; the device service serves the ACTIVE
            // window regardless, so an empty package is correct for the MVP.
            Ok(client) => Box::new(ServiceA11y::new(client, String::new())),
            Err(e) => {
                eprintln!("glass-android: a11y service unavailable, using uiautomator: {e}");
                Box::new(AndroidA11y::for_adb(self.resolved_adb()))
            }
        }
    }

    /// Refresh active-window geometry before capture, retaining the cache if the window is absent.
    fn refresh_window_until(&mut self, deadline: Deadline) -> Result<WindowGeometry> {
        let (package, active_id) = {
            let app = self.running()?;
            (app.package.clone(), app.active_id)
        };
        let dump = self
            .adb()
            .run_until(["shell", "dumpsys", "window", "windows"], deadline)?;
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
    fn discover_window(
        &self,
        package: &str,
        timeout_ms: u64,
    ) -> Result<(WindowId, WindowGeometry)> {
        self.discover_window_by(package, timeout_ms, Deadline::UNBOUNDED)
    }

    fn discover_window_by(
        &self,
        package: &str,
        timeout_ms: u64,
        deadline: Deadline,
    ) -> Result<(WindowId, WindowGeometry)> {
        let started = Instant::now();
        let (ends, owner) = deadline.resolve(started + Duration::from_millis(timeout_ms.max(1)));
        let mut last_dump = String::new();
        loop {
            if Instant::now() >= ends {
                return Err(discovery_deadline_error(
                    owner,
                    last_dump.is_empty(),
                    &last_dump,
                    package,
                    timeout_ms,
                ));
            }
            let dump = self
                .adb()
                .run_until(
                    ["shell", "dumpsys", "window", "windows"],
                    Deadline::at(ends),
                )
                .map_err(|error| {
                    discovery_call_error(owner, error, &last_dump, package, timeout_ms)
                })?;
            if let Some(w) = parse_app_windows(&dump, package).into_iter().next() {
                return Ok((WindowId(w.id), w.frame));
            }
            last_dump = dump;
            if Instant::now() >= ends {
                return Err(match owner {
                    glass_core::Whose::Caller => {
                        GlassError::caller_deadline_elapsed("Android window discovery")
                    }
                    glass_core::Whose::Callee => {
                        window_never_appeared(&last_dump, package, timeout_ms)
                    }
                });
            }
            std::thread::sleep(
                Duration::from_millis(150).min(ends.saturating_duration_since(Instant::now())),
            );
        }
    }
}

/// What a discovery reports when its budget ran out, diagnosed from the last dump it read — not a
/// fresh one, which would describe a different moment.
fn window_never_appeared(dump: &str, package: &str, timeout_ms: u64) -> GlassError {
    GlassError::AppWindowNotVisible {
        package: package.to_string(),
        timeout_ms,
        observed: describe_missing_window(dump, package),
    }
}

fn discovery_deadline_error(
    owner: glass_core::Whose,
    no_attempt: bool,
    last_dump: &str,
    package: &str,
    timeout_ms: u64,
) -> GlassError {
    match (owner, no_attempt) {
        (glass_core::Whose::Caller, true) => {
            GlassError::deadline_not_started("Android window discovery")
        }
        (glass_core::Whose::Caller, false) => {
            GlassError::caller_deadline_elapsed("Android window discovery")
        }
        (glass_core::Whose::Callee, _) => window_never_appeared(last_dump, package, timeout_ms),
    }
}

fn discovery_call_error(
    owner: glass_core::Whose,
    error: GlassError,
    last_dump: &str,
    package: &str,
    timeout_ms: u64,
) -> GlassError {
    if owner == glass_core::Whose::Callee && error.bound_owner() == Some(glass_core::Whose::Caller)
    {
        window_never_appeared(last_dump, package, timeout_ms)
    } else {
        error
    }
}

fn pid_discovery_error_is_bounded(error: &GlassError) -> bool {
    error.bound().is_some()
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
    Ok(Region {
        x: x0 as u32,
        y: y0 as u32,
        width: w as u32,
        height: h as u32,
    })
}

fn finish_capture(
    bytes: &[u8],
    win: &WindowGeometry,
    region: Option<&Region>,
    deadline: Deadline,
) -> Result<Frame> {
    let display = decode_screencap(bytes)?;
    let window_region = visible_window_region(win, display.width, display.height)?;
    let window_frame = display.crop(&window_region)?;
    let frame = match region {
        Some(region) => window_frame.crop(region),
        None => Ok(window_frame),
    }?;
    if deadline.has_passed() {
        Err(GlassError::caller_deadline_elapsed("capture"))
    } else {
        Ok(frame)
    }
}

/// Force-stop a launch that failed after `am start` had already put the app on the device, and
/// hand its error back — `start_app` records `self.app` only on success, so nothing else reaps it.
fn reap_failed_launch(adb: &Adb, package: &str, e: GlassError) -> GlassError {
    let _ = adb.run(force_stop_args(package).iter().map(String::as_str));
    e
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

        // Each failure from here down reaps for itself — see `reap_failed_launch`.
        let (active_id, window) = match self.discover_window(&target.package, spec.timeout_ms) {
            Ok(found) => found,
            Err(e) => return Err(reap_failed_launch(&adb, &target.package, e)),
        };

        let pidof = adb
            .run(["shell", "pidof", &target.package])
            .unwrap_or_default();
        let pid = parse_pid(&pidof);
        let logcat = match pid {
            Some(pid) => match LogcatStream::spawn(&adb, pid, self.logs.clone()) {
                Ok(stream) => Some(stream),
                Err(e) => return Err(reap_failed_launch(&adb, &target.package, e)),
            },
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

    /// Force-stop measures well inside the 3s all of teardown gets (see [`AdbOp::Launch`]), so the
    /// deadline is not expected to bind. It is for the device that stopped answering, which would
    /// otherwise run out the 10s `AdbOp::Shell` budget and leave the hook nothing (glass#422).
    fn stop_app_by(&mut self, deadline: Deadline) -> Result<()> {
        self.stop_app_until(deadline)
    }

    fn capture_frame_by(&mut self, region: Option<&Region>, deadline: Deadline) -> Result<Frame> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("capture"));
        }
        let win = self.refresh_window_until(deadline)?;
        let bytes = self
            .adb()
            .run_bytes_until(["exec-out", "screencap"], deadline)?;
        finish_capture(&bytes, &win, region, deadline)
    }

    fn capture_window_by(
        &mut self,
        _id: WindowId,
        _region: Option<&Region>,
        deadline: Deadline,
    ) -> Result<Frame> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("window capture"));
        }
        Err(GlassError::Unsupported(
            "capture_window is not supported by this backend".into(),
        ))
    }

    fn send_pointer_by(&mut self, event: &PointerEvent, deadline: Deadline) -> Result<()> {
        glass_core::validate_pointer_input(event)?;
        let origin = self.running()?.window.clone();
        self.injector
            .pointer_by(self.target.adb(), &origin, event, deadline)
    }

    fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        self.running()?; // require an active session
        self.injector.key_by(self.target.adb(), event, deadline)
    }

    fn window_by(&mut self, op: &WindowOp, deadline: Deadline) -> Result<WindowGeometry> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("Android window operation"));
        }
        match op {
            WindowOp::Geometry => {
                let geometry = self.running()?.window.clone();
                if deadline.has_passed() {
                    Err(GlassError::caller_deadline_elapsed(
                        "Android window geometry",
                    ))
                } else {
                    Ok(geometry)
                }
            }
            WindowOp::Focus => {
                let (component, package) = {
                    let app = self.running()?;
                    (app.component.clone(), app.package.clone())
                };
                let out = self
                    .adb()
                    .run_until(launch_args(&component).iter().map(String::as_str), deadline)?;
                check_am_start(&out)?;
                let (active_id, window) = self
                    .discover_window_by(&package, 5_000, deadline)
                    .map_err(GlassError::after_dispatch)?;
                let app = self.app.as_mut().ok_or(GlassError::NoActiveSession)?;
                app.active_id = active_id;
                app.window = window.clone();
                if deadline.has_passed() {
                    Err(GlassError::caller_deadline_elapsed("Android window focus"))
                } else {
                    Ok(window)
                }
            }
            WindowOp::Resize { .. } | WindowOp::Move { .. } => {
                Err(crate::unsupported_window_move_resize())
            }
        }
    }

    fn list_windows_by(&mut self, deadline: Deadline) -> Result<Vec<WindowInfo>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("Android window list"));
        }
        let (package, active_id) = {
            let app = self.running()?;
            (app.package.clone(), app.active_id)
        };
        let dump = self
            .adb()
            .run_until(["shell", "dumpsys", "window", "windows"], deadline)?;
        let parsed = parse_app_windows(&dump, &package);
        #[cfg(all(test, unix))]
        if let Some(delay) = self.window_list_parse_delay {
            std::thread::sleep(delay);
        }
        let any_match = parsed.iter().any(|w| WindowId(w.id) == active_id);
        let windows = parsed
            .into_iter()
            .enumerate()
            .map(|(i, w)| WindowInfo {
                id: WindowId(w.id),
                title: Some(w.title),
                class: Some(package.clone()),
                geometry: w.frame,
                active: if any_match {
                    WindowId(w.id) == active_id
                } else {
                    i == 0
                },
            })
            .collect();
        if deadline.has_passed() {
            Err(GlassError::caller_deadline_elapsed("Android window list"))
        } else {
            Ok(windows)
        }
    }

    fn select_window_by(&mut self, id: WindowId, deadline: Deadline) -> Result<WindowGeometry> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("Android window selection"));
        }
        let package = self.running()?.package.clone();
        let dump = self
            .adb()
            .run_until(["shell", "dumpsys", "window", "windows"], deadline)?;
        let found = parse_app_windows(&dump, &package)
            .into_iter()
            .find(|w| WindowId(w.id) == id);
        match found {
            Some(w) => {
                if deadline.has_passed() {
                    return Err(GlassError::caller_deadline_elapsed(
                        "Android window selection",
                    ));
                }
                let app = self.app.as_mut().ok_or(GlassError::NoActiveSession)?;
                app.active_id = id;
                app.window = w.frame.clone();
                if deadline.has_passed() {
                    Err(GlassError::caller_deadline_elapsed(
                        "Android window selection",
                    ))
                } else {
                    Ok(w.frame)
                }
            }
            None => Err(GlassError::WindowNotFound),
        }
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.logs
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
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
        self.app_pids_by(Deadline::UNBOUNDED)
            .unwrap_or_else(|_| self.app_pid().into_iter().collect())
    }

    fn app_pids_by(&self, deadline: Deadline) -> Result<Vec<u32>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "Android accessibility process discovery",
            ));
        }
        // Best-effort live re-scan; falls back to the single known pid.
        if let Some(app) = &self.app {
            match self
                .adb()
                .run_until(["shell", "pidof", &app.package], deadline)
            {
                Ok(out) => {
                    let pids = parse_pids(&out);
                    if deadline.has_passed() {
                        return Err(GlassError::caller_deadline_elapsed(
                            "Android accessibility process discovery",
                        ));
                    }
                    if !pids.is_empty() {
                        return Ok(pids);
                    }
                }
                Err(error) if pid_discovery_error_is_bounded(&error) => return Err(error),
                Err(_) => {}
            }
        }
        if deadline.has_passed() {
            Err(GlassError::caller_deadline_elapsed(
                "Android accessibility process discovery",
            ))
        } else {
            Ok(self.app_pid().into_iter().collect())
        }
    }
}

impl Drop for AndroidPlatform {
    /// Force-stop the app on drop, for a platform dropped without an explicit `stop_app()` —
    /// parity with the X11/Wayland/Windows/macOS backends. The device outlives the process, so
    /// the app would otherwise still be there for the next run (glass#419).
    ///
    /// `stop_app` takes `self.app`, so this is a no-op after it — and `Glass::stop`,
    /// `Glass::shutdown` and a session replacement all call it. The path they do not reach is a
    /// launch that failed on the way up, which is [`reap_failed_launch`]'s.
    fn drop(&mut self) {
        let _ = self.stop_app();
    }
}

// Unix-gated as a whole: `FakeAdb` is a `/bin/sh` script, so this import breaks the Windows
// build without it.
#[cfg(test)]
#[cfg(unix)]
mod platform_tests {
    use super::*;
    use crate::adb::{Answer, FakeAdb};
    use crate::target::AttachedDevice;
    use glass_core::{MouseButton, SandboxLevel};

    /// Two on-screen windows for the app — a dialog on top of its main activity — plus windows
    /// owned by other packages, as `dumpsys window windows` really lays them out.
    const WINDOWS: &str = concat!(
        "  Window #0 Window{aaa111 u0 StatusBar}:\n",
        "    mOwnerUid=10168 showForAllUsers=true package=com.android.systemui appop=NONE\n",
        "    mFrame=[0,0][1080,80] isOnScreen=true\n",
        "  Window #1 Window{bbb222 u0 com.example.app/com.example.app.MyDialog}:\n",
        "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
        "    mFrame=[140,800][940,1600] isOnScreen=true\n",
        "  Window #2 Window{ddd444 u0 com.example.app/com.example.app.MainActivity}:\n",
        "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
        "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
    );

    /// The same dump with the dialog gone — the app's window list after it is dismissed.
    const WINDOWS_WITHOUT_THE_DIALOG: &str = concat!(
        "  Window #0 Window{aaa111 u0 StatusBar}:\n",
        "    mOwnerUid=10168 showForAllUsers=true package=com.android.systemui appop=NONE\n",
        "    mFrame=[0,0][1080,80] isOnScreen=true\n",
        "  Window #2 Window{ddd444 u0 com.example.app/com.example.app.MainActivity}:\n",
        "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
        "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
    );

    fn spec() -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["com.example.app/.MainActivity".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 2_000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        }
    }

    #[test]
    fn discovery_error_helpers_preserve_attempt_and_owner_distinctions() {
        let before =
            discovery_deadline_error(glass_core::Whose::Caller, true, "", "com.example.app", 100);
        assert_eq!(before.bound(), Some(glass_core::BoundKind::NotStarted));

        let after = discovery_deadline_error(
            glass_core::Whose::Caller,
            false,
            "empty dump",
            "com.example.app",
            100,
        );
        assert_eq!(after.bound(), Some(glass_core::BoundKind::TimedOut));

        let callee =
            discovery_deadline_error(glass_core::Whose::Callee, true, "", "com.example.app", 100);
        assert!(matches!(callee, GlassError::AppWindowNotVisible { .. }));

        let caller_error = || GlassError::caller_deadline_elapsed("dumpsys");
        let mapped = discovery_call_error(
            glass_core::Whose::Callee,
            caller_error(),
            "last dump",
            "com.example.app",
            100,
        );
        assert!(matches!(mapped, GlassError::AppWindowNotVisible { .. }));

        let retained = discovery_call_error(
            glass_core::Whose::Caller,
            caller_error(),
            "last dump",
            "com.example.app",
            100,
        );
        assert_eq!(retained.bound_owner(), Some(glass_core::Whose::Caller));

        let ordinary = discovery_call_error(
            glass_core::Whose::Callee,
            GlassError::Backend("dumpsys failed".into()),
            "last dump",
            "com.example.app",
            100,
        );
        assert!(matches!(ordinary, GlassError::Backend(_)));

        assert!(pid_discovery_error_is_bounded(&caller_error()));
        assert!(!pid_discovery_error_is_bounded(&GlassError::Backend(
            "pidof failed".into()
        )));
    }

    /// A screencap of `w`x`h` opaque pixels, as `exec-out screencap` returns one.
    fn frame_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend(std::iter::repeat_n(0xffu8, (w * h * 4) as usize));
        v
    }

    /// A platform bound to `fake`, with no on-device companion — the adb fallback paths.
    fn platform_over(fake: &FakeAdb) -> AndroidPlatform {
        AndroidPlatform {
            target: Box::new(AttachedDevice::from_adb(fake.adb().clone())),
            injector: Box::new(ShellInjector),
            agent: None,
            logs: Arc::new(Mutex::new(Vec::new())),
            app: None,
            window_list_parse_delay: None,
        }
    }

    /// A device that answers everything a healthy launch asks of it.
    fn launchable() -> FakeAdb {
        FakeAdb::new(&[
            (
                "shell am start *",
                Answer::says("Starting: Intent {...}\nStatus: ok\n"),
            ),
            ("shell dumpsys window windows", Answer::says(WINDOWS)),
            ("shell pidof *", Answer::says("4321\n")),
            ("exec-out screencap", Answer::says(frame_bytes(1080, 2400))),
            ("*", Answer::Silent),
        ])
    }

    /// A launched app on a fake device, ready for the calls that need a session.
    fn started(fake: &FakeAdb) -> AndroidPlatform {
        let mut platform = platform_over(fake);
        platform.start_app(&spec()).expect("the launch succeeds");
        platform
    }

    #[test]
    #[cfg(unix)]
    fn starting_an_app_launches_it_and_reports_its_topmost_window() {
        let fake = launchable();
        let mut platform = platform_over(&fake);

        let window = platform.start_app(&spec()).expect("the launch succeeds");

        // The dialog is topmost, so that is the window the session drives.
        assert_eq!((window.x, window.y), (140, 800));
        assert_eq!((window.width, window.height), (800, 800));
        assert!(fake.called("am start -W -n com.example.app/.MainActivity"));
        assert_eq!(platform.app_pid(), Some(4321), "logcat needs the app's pid");
        // Scoped to the app: without `--pid` this streams the whole device's log, which is
        // every other app's output presented as this session's.
        assert!(
            fake.wait_called("logcat -v threadtime --pid=4321", Duration::from_secs(5)),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_launch_that_the_device_refuses_is_not_reported_as_started() {
        let fake = FakeAdb::new(&[(
            "shell am start *",
            Answer::says("Starting: Intent {...}\nError: Activity not started\n"),
        )]);
        let mut platform = platform_over(&fake);
        assert!(platform.start_app(&spec()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn a_window_that_takes_a_moment_to_appear_is_waited_for() {
        // The activity is up before its window is laid out, so the first dump legitimately
        // shows nothing. A deadline computed backwards makes that first look the answer.
        let empty = Answer::says("");
        let windows = Answer::says(WINDOWS);
        let launch_reply = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let pid = Answer::says("4321\n");
        let silent = Answer::Silent;
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&launch_reply]),
            (
                "shell dumpsys window windows",
                vec![&empty, &empty, &windows],
            ),
            ("shell pidof *", vec![&pid]),
            ("*", vec![&silent]),
        ]);

        let mut platform = platform_over(&fake);
        let window = platform
            .start_app(&spec())
            .expect("the window arrives on the third look");
        assert_eq!((window.x, window.y), (140, 800));
    }

    #[test]
    #[cfg(unix)]
    fn a_window_that_never_appears_ends_the_launch_rather_than_the_wait_going_on() {
        let empty = Answer::says("");
        let launch_reply = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&launch_reply]),
            ("shell dumpsys window windows", vec![&empty]),
            ("*", vec![&Answer::Silent]),
        ]);

        let mut platform = platform_over(&fake);
        let mut spec = spec();
        spec.timeout_ms = 300;
        let at = Instant::now();
        assert!(platform.start_app(&spec).is_err());
        assert!(
            at.elapsed() < Duration::from_secs(20),
            "waited {:?}",
            at.elapsed()
        );
    }

    #[test]
    #[cfg(unix)]
    fn stopping_an_app_force_stops_its_package_and_ends_the_session() {
        let fake = launchable();
        let mut platform = started(&fake);

        platform
            .stop_app()
            // A failed force-stop is NOT surfaced — `stop_app` discards it — so this asserts
            // only that the call was made and the session ended, not that the app died.
            .expect("ending a session reports");

        assert!(
            fake.called("am force-stop com.example.app"),
            "{:?}",
            fake.calls()
        );
        assert!(matches!(
            platform.window(&WindowOp::Geometry),
            Err(GlassError::NoActiveSession)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn a_launch_whose_window_never_appears_does_not_leave_the_app_running() {
        let empty = Answer::says("");
        let launch_reply = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&launch_reply]),
            ("shell dumpsys window windows", vec![&empty]),
            ("*", vec![&Answer::Silent]),
        ]);

        let mut platform = platform_over(&fake);
        let mut spec = spec();
        spec.timeout_ms = 300;

        platform
            .start_app(&spec)
            .expect_err("a launch whose window never appears fails");

        // `am start` put it on the device and the failure left `self.app` unset, so the drop
        // below passes over it.
        assert!(
            fake.called("am force-stop com.example.app"),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn dropping_a_platform_that_still_holds_an_app_force_stops_it() {
        let fake = launchable();

        drop(started(&fake));

        assert!(
            fake.called("am force-stop com.example.app"),
            "{:?}",
            fake.calls()
        );
    }

    /// The composition guarantee (glass#422): a force-stop that never answers must not spend the
    /// budget the shutdown hook's own steps need. `Lingers` is a device that stopped answering.
    #[test]
    #[cfg(unix)]
    fn a_force_stop_that_never_answers_gives_up_at_the_shared_deadline() {
        // `launchable`'s rules, with a force-stop that never returns in front of them.
        let fake = FakeAdb::new(&[
            ("shell am force-stop *", Answer::Lingers),
            (
                "shell am start *",
                Answer::says("Starting: Intent {...}\nStatus: ok\n"),
            ),
            ("shell dumpsys window windows", Answer::says(WINDOWS)),
            ("shell pidof *", Answer::says("4321\n")),
            ("exec-out screencap", Answer::says(frame_bytes(1080, 2400))),
            ("*", Answer::Silent),
        ]);
        let mut platform = started(&fake);

        let started_at = Instant::now();
        platform
            .stop_app_by(Deadline::at(Instant::now() + Duration::from_millis(300)))
            .expect("teardown is best-effort and reports success either way");
        let waited = started_at.elapsed();

        assert!(
            waited < Duration::from_secs(2),
            "waited {waited:?} on a force-stop that never answers — the AdbOp budget (10s) was \
             used instead of the deadline, and the hook behind this gets nothing"
        );
        assert!(
            fake.called("am force-stop com.example.app"),
            "it still has to have asked: {:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_capture_is_cropped_to_the_window_rather_than_the_display() {
        let fake = launchable();
        let mut platform = started(&fake);

        let frame = platform.capture_frame(None).expect("the device captures");

        // The dialog's frame, not the 1080x2400 display behind it.
        assert_eq!((frame.width, frame.height), (800, 800));
    }

    #[test]
    fn decode_and_crop_cannot_return_success_after_the_caller_deadline() {
        let win = WindowGeometry {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let region = Region {
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        };

        let error = finish_capture(
            &frame_bytes(2, 2),
            &win,
            Some(&region),
            Deadline::at(Instant::now()),
        )
        .expect_err("a frame completed after its caller left");

        assert_eq!(
            error.bound(),
            Some(glass_core::BoundKind::TimedOut),
            "{error}"
        );
        assert_eq!(
            error.bound_owner(),
            Some(glass_core::Whose::Caller),
            "{error}"
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched),
            "{error}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn android_screencap_uses_adb_run_bytes_until() {
        let launch_reply = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let windows = Answer::says(WINDOWS);
        let pid = Answer::says("4321\n");
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&launch_reply]),
            ("shell dumpsys window windows", vec![&windows, &windows]),
            ("shell pidof *", vec![&pid]),
            ("exec-out screencap", vec![&Answer::Lingers]),
            ("*", vec![&Answer::Silent]),
        ]);
        let mut platform = started(&fake);

        let at = Instant::now();
        let err = platform
            .capture_frame_by(None, Deadline::at(Instant::now() + Duration::from_secs(2)))
            .expect_err("a live caller deadline must bound screencap");

        assert!(
            at.elapsed() < Duration::from_secs(5),
            "waited {:?}: {err}",
            at.elapsed()
        );
        assert_eq!(err.bound(), Some(glass_core::BoundKind::TimedOut));
        assert!(
            fake.called("exec-out screencap"),
            "screencap did not reach the deadline-aware runner: {:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn android_refresh_uses_the_capture_caller_deadline() {
        let launch_reply = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let windows = Answer::says(WINDOWS);
        let pid = Answer::says("4321\n");
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&launch_reply]),
            (
                "shell dumpsys window windows",
                vec![&windows, &Answer::Lingers],
            ),
            ("shell pidof *", vec![&pid]),
            (
                "exec-out screencap",
                vec![&Answer::says(frame_bytes(1080, 2400))],
            ),
            ("*", vec![&Answer::Silent]),
        ]);
        let mut platform = started(&fake);

        let at = Instant::now();
        let err = platform
            .capture_frame_by(
                None,
                Deadline::at(Instant::now() + Duration::from_millis(300)),
            )
            .expect_err("a live caller deadline must bound refresh");

        assert!(
            at.elapsed() < Duration::from_secs(2),
            "waited {:?}: {err}",
            at.elapsed()
        );
        assert_eq!(err.bound(), Some(glass_core::BoundKind::TimedOut));
        assert!(fake.called("shell dumpsys window windows"));
        assert!(
            !fake.called("exec-out screencap"),
            "refresh spent the deadline but screencap still started: {:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_window_is_re_read_before_a_capture_so_a_move_is_not_missed() {
        // The cached geometry is from launch; a rotation or a layout change moves the window
        // under it, and cropping to a stale rect captures the wrong pixels.
        let windows = Answer::says(WINDOWS);
        let moved = Answer::says(concat!(
            "  Window #1 Window{bbb222 u0 com.example.app/com.example.app.MyDialog}:
",
            "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE
",
            "    mFrame=[0,0][600,400] isOnScreen=true
",
        ));
        let started_ok = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let pid = Answer::says("4321\n");
        let shot = Answer::says(frame_bytes(1080, 2400));
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&started_ok]),
            ("shell dumpsys window windows", vec![&windows, &moved]),
            ("shell pidof *", vec![&pid]),
            ("exec-out screencap", vec![&shot]),
            ("*", vec![&Answer::Silent]),
        ]);

        let mut platform = platform_over(&fake);
        platform.start_app(&spec()).expect("the launch succeeds");
        let frame = platform.capture_frame(None).expect("the device captures");
        assert_eq!(
            (frame.width, frame.height),
            (600, 400),
            "the capture used the window's current frame"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pointer_and_key_events_reach_the_device() {
        let fake = launchable();
        let mut platform = started(&fake);

        platform
            .send_pointer(&PointerEvent::Click {
                x: 10,
                y: 20,
                button: MouseButton::Left,
                count: 1,
                modifiers: vec![],
            })
            .expect("a tap is sent");
        platform
            .send_key(&KeyEvent::Text("hi".into()))
            .expect("text is sent");

        // Window-relative (10,20) inside a window at (140,800) is absolute (150,820).
        assert!(fake.called("input tap 150 820"), "{:?}", fake.calls());
        assert!(fake.called("input text"), "{:?}", fake.calls());
    }

    #[test]
    #[cfg(unix)]
    fn input_without_a_session_is_refused_rather_than_sent_nowhere() {
        let fake = launchable();
        let mut platform = platform_over(&fake);
        assert!(platform.send_key(&KeyEvent::Text("hi".into())).is_err());
        assert!(!fake.called("input text"), "{:?}", fake.calls());
    }

    #[test]
    #[cfg(unix)]
    fn focusing_relaunches_the_activity_and_re_reads_the_window() {
        let fake = launchable();
        let mut platform = started(&fake);

        let window = platform.window(&WindowOp::Focus).expect("focus succeeds");

        assert_eq!((window.x, window.y), (140, 800));
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.contains("am start"))
                .count(),
            2,
            "focus re-issues the launch intent"
        );
    }

    #[test]
    #[cfg(unix)]
    fn moving_or_resizing_a_window_is_refused_on_this_platform() {
        let fake = launchable();
        let mut platform = started(&fake);
        assert!(matches!(
            platform.window(&WindowOp::Resize {
                width: 100,
                height: 100
            }),
            Err(GlassError::Unsupported(_))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn listing_windows_marks_the_selected_one_active() {
        let fake = launchable();
        let mut platform = started(&fake);

        let windows = platform.list_windows().expect("the app has windows");

        assert_eq!(windows.len(), 2, "{windows:?}");
        assert_eq!(windows[0].geometry.x, 140, "topmost first");
        assert!(windows[0].active, "the session drives the topmost window");
        assert!(!windows[1].active);
        assert_eq!(windows[0].class.as_deref(), Some("com.example.app"));
    }

    #[test]
    #[cfg(unix)]
    fn large_window_list_parse_finishing_after_deadline_is_not_late_success() {
        let launch_reply = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let initial_windows = Answer::says(WINDOWS);
        let large_windows = Answer::says(WINDOWS.repeat(512));
        let pid = Answer::says("4321\n");
        let silent = Answer::Silent;
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&launch_reply]),
            (
                "shell dumpsys window windows",
                vec![&initial_windows, &large_windows],
            ),
            ("shell pidof *", vec![&pid]),
            ("*", vec![&silent]),
        ]);
        let mut platform = started(&fake).with_window_list_parse_delay(Duration::from_millis(30));

        let error = platform
            .list_windows_by(Deadline::from_millis(10))
            .expect_err("a completed parse must not return success after its caller deadline");

        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_list_whose_selected_window_is_gone_falls_back_to_the_topmost() {
        // The dialog the session was driving has been dismissed. Marking nothing active would
        // leave the caller with a list it cannot act on.
        let windows = Answer::says(WINDOWS);
        let without = Answer::says(WINDOWS_WITHOUT_THE_DIALOG);
        let started_ok = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let pid = Answer::says("4321\n");
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&started_ok]),
            ("shell dumpsys window windows", vec![&windows, &without]),
            ("shell pidof *", vec![&pid]),
            ("*", vec![&Answer::Silent]),
        ]);

        let mut platform = platform_over(&fake);
        platform.start_app(&spec()).expect("the launch succeeds");
        let listed = platform.list_windows().expect("the app still has a window");

        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(
            listed[0].active,
            "with the selection gone, the topmost window is the active one"
        );
    }

    #[test]
    #[cfg(unix)]
    fn selecting_a_window_switches_to_it_and_an_unknown_id_is_refused() {
        let fake = launchable();
        let mut platform = started(&fake);

        let main = platform
            .list_windows()
            .expect("windows")
            .into_iter()
            .find(|w| w.geometry.x == 0)
            .expect("the main activity is listed");

        let geometry = platform.select_window(main.id).expect("it is one of ours");
        assert_eq!((geometry.width, geometry.height), (1080, 2400));
        // The selection sticks: the session now drives the main activity.
        assert_eq!(platform.window(&WindowOp::Geometry).unwrap().x, 0);

        assert!(matches!(
            platform.select_window(WindowId(0xdead_beef)),
            Err(GlassError::WindowNotFound)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn logs_are_handed_over_once_and_then_gone() {
        let fake = launchable();
        let mut platform = started(&fake);
        platform
            .logs
            .lock()
            .unwrap()
            .push((Stream::Stdout, "hello".to_string()));

        assert_eq!(
            platform.drain_logs(),
            [(Stream::Stdout, "hello".to_string())]
        );
        assert!(
            platform.drain_logs().is_empty(),
            "a drained line must not be handed out twice"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_clipboard_says_it_needs_the_companion_rather_than_answering_emptily() {
        // An empty string would read as an empty clipboard, and a silent `set` as one that
        // worked — neither is true without the agent.
        let fake = launchable();
        let mut platform = started(&fake);

        let read = platform
            .get_clipboard()
            .expect_err("no agent, no clipboard");
        assert!(matches!(read, GlassError::Unsupported(_)), "{read}");
        assert!(read.to_string().contains("agent"), "{read}");

        let written = platform
            .set_clipboard("x")
            .expect_err("no agent, no clipboard");
        assert!(matches!(written, GlassError::Unsupported(_)), "{written}");
    }

    #[test]
    #[cfg(unix)]
    fn accessibility_pid_discovery_kills_a_wedged_pidof_at_the_shared_deadline() {
        let started_ok = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let windows = Answer::says(WINDOWS);
        let pid = Answer::says("4321\n");
        let wedged = Answer::Lingers;
        let silent = Answer::Silent;
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&started_ok]),
            ("shell dumpsys window windows", vec![&windows]),
            ("shell pidof *", vec![&pid, &wedged]),
            ("*", vec![&silent]),
        ]);
        let platform = started(&fake);
        let deadline = Deadline::from_millis(25);

        let began = std::time::Instant::now();
        let error = platform
            .app_pids_by(deadline)
            .expect_err("a wedged live pid scan must stop at the semantic caller deadline");

        assert!(
            began.elapsed() < Duration::from_secs(1),
            "pidof waited {:?}, which is closer to adb's 10s shell budget than the caller deadline",
            began.elapsed()
        );
        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert!(
            error.to_string().contains("adb:shell")
                && error.to_string().contains("adb kill-server"),
            "the bounded runner's operation and remedy were replaced: {error}"
        );
        assert!(
            fake.deadlines().contains(&deadline),
            "the exact semantic deadline must reach the adb process runner"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_app_pids_are_re_read_from_the_device_and_fall_back_to_the_known_one() {
        let fake = launchable();
        let platform = started(&fake);
        // `pidof` lists every process of the package; the launch recorded only the first.
        assert_eq!(platform.app_pid(), Some(4321));

        let started_ok = Answer::says("Starting: Intent {...}\nStatus: ok\n");
        let windows = Answer::says(WINDOWS);
        let one = Answer::says("4321\n");
        let many = Answer::says("4321 4322 4323\n");
        let none = Answer::says("\n");
        let failed = Answer::fails("device offline");
        let fake = FakeAdb::scripted(&[
            ("shell am start *", vec![&started_ok]),
            ("shell dumpsys window windows", vec![&windows]),
            ("shell pidof *", vec![&one, &many, &none, &failed]),
            ("*", vec![&Answer::Silent]),
        ]);
        let mut platform = platform_over(&fake);
        platform.start_app(&spec()).expect("the launch succeeds");

        assert_eq!(platform.app_pids(), [4321, 4322, 4323], "a live re-scan");
        assert_eq!(
            platform.app_pids(),
            [4321],
            "a scan that finds nothing falls back to the pid the launch recorded"
        );
        assert_eq!(
            platform.app_pids_by(Deadline::UNBOUNDED).unwrap(),
            [4321],
            "an ordinary pidof failure falls back to the pid the launch recorded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// glass#338's failure, now readable in the error rather than only in a whole-run device log.
    #[test]
    fn a_discovery_that_ran_out_of_time_says_what_was_on_screen_instead() {
        let dump = concat!(
            "  Window #0 Window{aaa111 u0 com.google.android.settings.intelligence/.SearchActivity}:\n",
            "    package=com.google.android.settings.intelligence appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
            "  Window #1 Window{bbb222 u0 com.android.settings/.Settings}:\n",
            "    package=com.android.settings appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=false\n",
        );
        let msg = window_never_appeared(dump, "com.android.settings", 15_000).to_string();
        assert!(msg.contains("com.android.settings"), "{msg}");
        assert!(msg.contains("15000 ms"), "{msg}");
        assert!(msg.contains("not on screen"), "{msg}");
        assert!(
            msg.contains("com.google.android.settings.intelligence"),
            "{msg}"
        );
    }

    #[test]
    fn visible_window_region_full_onscreen_is_identity() {
        let win = WindowGeometry {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        };
        let r = visible_window_region(&win, 1080, 2400).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 1080, 2400));
    }

    #[test]
    fn visible_window_region_clamps_negative_origin() {
        let win = WindowGeometry {
            x: -10,
            y: -20,
            width: 1080,
            height: 2400,
        };
        let r = visible_window_region(&win, 1080, 2400).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 1070, 2380));
    }

    #[test]
    fn visible_window_region_clamps_overhang() {
        let win = WindowGeometry {
            x: 1000,
            y: 0,
            width: 200,
            height: 100,
        };
        let r = visible_window_region(&win, 1080, 720).unwrap();
        assert_eq!((r.x, r.width), (1000, 80));
        assert_eq!((r.y, r.height), (0, 100));
    }

    #[test]
    fn visible_window_region_errors_when_fully_offscreen() {
        let win = WindowGeometry {
            x: 2000,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(visible_window_region(&win, 1080, 720).is_err());
    }
}
