//! `IosPlatform`: the `glass_core::Platform` implementation that drives a single
//! foreground app on an iOS Simulator over `xcrun simctl`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region, Result, Stream,
    WindowGeometry, WindowId, WindowInfo, WindowOp,
};

use crate::capture::screenshot;
use crate::idb::client::IdbClient;
use crate::idb::companion::IdbCompanion;
use crate::injector::IdbInjector;
use crate::logs::LogStream;
use crate::target::{SimTarget, SimulatorRegistry};

/// The single foreground app this backend drives.
struct RunningApp {
    bundle_id: String,
    geometry: WindowGeometry,
    logs: LogStream,
}

/// Drives a native app on an iOS Simulator over `xcrun simctl`. A single foreground app at a
/// time, reported as one fullscreen window; there is no window management to speak of.
///
/// Input (tap/type/swipe/scroll) and the accessibility tree run over an owned
/// `idb_companion`: the [`IdbCompanion`] process is spawned for the resolved simulator and
/// killed when this platform drops; [`IdbClient`] carries HID input to it, and
/// [`IdbInjector`] builds those HID events at the discovered point→pixel [`scale`].
pub struct IosPlatform {
    target: SimTarget,
    app: Option<RunningApp>,
    /// Owns the `idb_companion` process bound to this simulator; killed on `Drop`.
    companion: IdbCompanion,
    /// gRPC client that injects HID input into the simulator.
    client: IdbClient,
    /// Builds HID events from glass input at the current point→pixel `scale`.
    injector: IdbInjector,
    /// Device point→pixel scale, discovered in `start_app` from the launch screenshot's
    /// pixel width versus the accessibility root's logical-point width. Provisionally
    /// `1.0` until then; input is gated by `running()` so none is issued before a real
    /// scale is known.
    scale: f64,
}

/// The Simulator reports exactly one fullscreen window per running app.
const IOS_WINDOW_ID: WindowId = WindowId(1);

/// Does `s` name an on-disk `.app` bundle path, as opposed to a bare bundle id of an app
/// already installed on the simulator?
fn looks_like_app_path(s: &str) -> bool {
    s.ends_with(".app") || s.contains('/')
}

/// Split `spec.run` into `(optional install path, bundle id)`. `run[0]` naming a path (it
/// ends in `.app` or contains a `/`) is installed via `simctl install`, with its bundle id
/// read from the bundle's `Info.plist`; otherwise `run[0]` is used directly as the bundle id
/// of an app already installed on the simulator.
pub(crate) fn bundle_id_from_run(run: &[String]) -> Result<(Option<String>, String)> {
    let first = run
        .first()
        .ok_or_else(|| GlassError::Backend("cannot start app: run command is empty".into()))?;
    if looks_like_app_path(first) {
        let plist = format!("{first}/Info.plist");
        let out = Command::new("plutil")
            .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-", &plist])
            .output()
            .map_err(|e| GlassError::Backend(format!("plutil: {e}")))?;
        if !out.status.success() {
            return Err(GlassError::Backend(format!(
                "could not read CFBundleIdentifier from {plist}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let bundle_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((Some(first.clone()), bundle_id))
    } else {
        Ok((None, first.clone()))
    }
}

impl IosPlatform {
    /// Resolve (attaching to or booting) a simulator per the `GLASS_IOS_*` env vars, then
    /// spawn its `idb_companion` and open the input/accessibility client.
    pub fn from_env(reg: &SimulatorRegistry) -> Result<Self> {
        let target = SimTarget::from_env(reg)?;
        let companion = IdbCompanion::spawn(target.udid())?;
        let client = IdbClient::connect(companion.socket())?;
        // Provisional; `start_app` refines it once a screenshot and the accessibility root
        // frame are available. Input before then is gated by `running()`.
        let scale = 1.0;
        Ok(Self {
            target,
            app: None,
            companion,
            client,
            injector: IdbInjector::new(scale),
            scale,
        })
    }

    fn running(&self) -> Result<&RunningApp> {
        self.app.as_ref().ok_or(GlassError::NoActiveSession)
    }

    /// Discover the device's point→pixel scale by dividing the capture's pixel width by
    /// the accessibility root's logical-point width. `describe` can briefly lag a launch,
    /// so this retries a few times before giving up. It never falls back to a placeholder:
    /// an undetermined scale would place every tap at the wrong point, so the failure
    /// surfaces as an error and the caller leaves the session unstarted.
    fn discover_scale(&self, frame_px_width: u32) -> Result<f64> {
        const ATTEMPTS: usize = 3;
        const RETRY_DELAY: Duration = Duration::from_millis(200);
        for attempt in 0..ATTEMPTS {
            if let Some(scale) = self
                .client
                .accessibility_info(None, true)
                .ok()
                .and_then(|json| crate::axmap::root_point_width(&json))
                .filter(|pt_w| *pt_w > 0.0)
                .map(|pt_w| f64::from(frame_px_width) / pt_w)
            {
                return Ok(scale);
            }
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(RETRY_DELAY);
            }
        }
        Err(GlassError::Backend(
            "could not determine the iOS display scale from the accessibility tree; \
             the app may not have finished rendering"
                .into(),
        ))
    }

    /// The Unix socket the backing `idb_companion` serves on. The accessibility reader
    /// connects a second client to this same socket.
    pub fn socket_path(&self) -> &std::path::Path {
        self.companion.socket()
    }

    /// Build an accessibility reader over a second client to the same companion socket.
    /// The session holds the platform and the accessibility reader as separate boxed
    /// trait objects, so the reader owns its own client rather than borrowing this one.
    pub fn accessibility(&self) -> Result<crate::a11y::IosA11y> {
        let client = IdbClient::connect(self.companion.socket())?;
        Ok(crate::a11y::IosA11y::new(client, self.scale))
    }
}

impl Platform for IosPlatform {
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        if let Some(build) = &spec.build {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(build);
            if let Some(dir) = &spec.cwd {
                cmd.current_dir(dir);
            }
            cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
            let status = cmd
                .status()
                .map_err(|e| GlassError::Backend(format!("build step: {e}")))?;
            if !status.success() {
                return Err(GlassError::Backend(format!(
                    "build command failed with status {status}"
                )));
            }
        }

        let udid = self.target.udid();
        let (install, bundle_id) = bundle_id_from_run(&spec.run)?;
        if let Some(path) = install {
            self.target.simctl().run(&["install", udid, &path])?;
        }
        // `SIMCTL_CHILD_<KEY>` is Apple's convention for passing environment variables through
        // `simctl launch` to the launched process, so `spec.env` is set that way rather than on
        // this (glass's own) process.
        let mut launch = Command::new(self.target.simctl().program());
        launch.args(self.target.simctl().full_args(&[
            "launch",
            "--terminate-running-process",
            udid,
            &bundle_id,
        ]));
        for (k, v) in &spec.env {
            launch.env(format!("SIMCTL_CHILD_{k}"), v);
        }
        let out = launch
            .output()
            .map_err(|e| GlassError::Backend(format!("simctl launch: {e}")))?;
        if !out.status.success() {
            return Err(GlassError::Backend(format!(
                "simctl launch failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        // Capture once, purely to learn the device's pixel dimensions for the geometry we
        // report. These are device *pixels* (the screenshot's raw resolution), not UIKit
        // points — the two differ by the device's point-to-pixel scale factor, which will
        // matter once pointer input is added.
        let frame = screenshot(self.target.simctl(), udid)?;
        // Learn the point→pixel scale before reporting the app as running, so no tap is
        // ever issued at an unverified scale. `describe` reports point frames while the
        // screenshot is in pixels; their ratio is the scale. On failure the app is left
        // unregistered (no active session) rather than driven at a wrong scale.
        let scale = self.discover_scale(frame.width)?;
        self.scale = scale;
        self.injector = IdbInjector::new(scale);
        let geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        };
        self.app = Some(RunningApp {
            bundle_id,
            geometry: geometry.clone(),
            logs: LogStream::spawn(udid),
        });
        Ok(geometry)
    }

    fn stop_app(&mut self) -> Result<()> {
        if let Some(app) = self.app.take() {
            let _ = self
                .target
                .simctl()
                .run(&["terminate", self.target.udid(), &app.bundle_id]);
        }
        Ok(())
    }

    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        self.running()?;
        let frame = screenshot(self.target.simctl(), self.target.udid())?;
        match region {
            None => Ok(frame),
            Some(r) => frame.crop(r),
        }
    }

    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        self.running()?;
        let events = self.injector.pointer_events(event)?;
        if events.is_empty() {
            // A `Move` has no touch equivalent, so there is nothing to inject.
            return Ok(());
        }
        self.client.hid(events)
    }

    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.running()?;
        self.client.hid(self.injector.key_events(event)?)
    }

    fn get_clipboard(&mut self) -> Result<String> {
        self.running()?;
        self.target.simctl().run(&["pbpaste", self.target.udid()])
    }

    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        self.running()?;
        // `simctl pbcopy` reads the clipboard text from stdin, so it needs a piped child
        // rather than the `Simctl::run` one-shot-output helper.
        let mut child = Command::new(self.target.simctl().program())
            .args(
                self.target
                    .simctl()
                    .full_args(&["pbcopy", self.target.udid()]),
            )
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GlassError::Backend(format!("pbcopy: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| GlassError::Backend("pbcopy: failed to open stdin".into()))?;
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            // Reap the child before returning so it doesn't outlive us as a zombie; its exit
            // status doesn't matter here since we're already reporting the write failure.
            let _ = child.wait();
            return Err(GlassError::Backend(format!("pbcopy write: {e}")));
        }
        // Close our end so `pbcopy` sees EOF and exits; holding it open past this point
        // would deadlock the `wait_with_output()` below.
        drop(stdin);
        let out = child
            .wait_with_output()
            .map_err(|e| GlassError::Backend(format!("pbcopy wait: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(GlassError::Backend(format!(
                "pbcopy failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }

    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let geometry = self.running()?.geometry.clone();
        match op {
            WindowOp::Geometry | WindowOp::Focus => Ok(geometry),
            WindowOp::Resize { .. } | WindowOp::Move { .. } => Err(GlassError::Unsupported(
                "window resize/move (iOS Simulator apps are fullscreen)".into(),
            )),
        }
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let app = self.running()?;
        Ok(vec![WindowInfo {
            id: IOS_WINDOW_ID,
            title: Some(app.bundle_id.clone()),
            class: None,
            geometry: app.geometry.clone(),
            active: true,
        }])
    }

    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        if id == IOS_WINDOW_ID {
            Ok(self.running()?.geometry.clone())
        } else {
            Err(GlassError::WindowNotFound)
        }
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.app
            .as_ref()
            .map(|a| a.logs.drain())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bundle_id_passthrough() {
        // A reverse-DNS bundle id (no path) is used directly, nothing to install.
        let (install, bid) = bundle_id_from_run(&["tech.fixedwidth.demo".into()]).unwrap();
        assert_eq!(install, None);
        assert_eq!(bid, "tech.fixedwidth.demo");
    }

    #[test]
    fn run_empty_is_error() {
        assert!(bundle_id_from_run(&[]).is_err());
    }

    #[test]
    fn looks_like_app_path_recognizes_bundle_paths() {
        assert!(looks_like_app_path("/x/App.app"));
        assert!(looks_like_app_path("MyApp.app"));
        assert!(looks_like_app_path("./rel/App.app"));
    }

    #[test]
    fn looks_like_app_path_rejects_bundle_ids() {
        assert!(!looks_like_app_path("tech.fixedwidth.demo"));
    }
}

/// State-machine tests: `IosPlatform` built directly (bypassing `from_env`) with a fake
/// `RunningApp` (or none) and stub companion/client, so none of these touch a real
/// simulator or `idb_companion` — `xcrun` is never invoked and no RPC is made, only the
/// pure in-memory branching (session guards, the `Move`-is-a-noop short-circuit).
#[cfg(test)]
mod state_machine_tests {
    use super::*;
    use glass_core::MouseButton;

    fn geometry() -> WindowGeometry {
        WindowGeometry {
            x: 0,
            y: 0,
            width: 390,
            height: 844,
        }
    }

    fn running_platform() -> IosPlatform {
        IosPlatform {
            target: SimTarget::for_test(),
            app: Some(RunningApp {
                bundle_id: "tech.fixedwidth.demo".into(),
                geometry: geometry(),
                logs: LogStream::spawn("fake"),
            }),
            companion: IdbCompanion::for_test(),
            client: IdbClient::for_test(),
            injector: IdbInjector::new(1.0),
            scale: 1.0,
        }
    }

    fn idle_platform() -> IosPlatform {
        IosPlatform {
            target: SimTarget::for_test(),
            app: None,
            companion: IdbCompanion::for_test(),
            client: IdbClient::for_test(),
            injector: IdbInjector::new(1.0),
            scale: 1.0,
        }
    }

    fn tap() -> PointerEvent {
        PointerEvent::Click {
            x: 1,
            y: 1,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        }
    }

    #[test]
    fn send_pointer_move_is_a_noop_when_running() {
        // A `Move` maps to no touch events, so it short-circuits to `Ok` without needing
        // the companion — no RPC is issued.
        let mut p = running_platform();
        assert!(p.send_pointer(&PointerEvent::Move { x: 1, y: 1 }).is_ok());
    }

    #[test]
    fn send_pointer_with_no_active_session_errors() {
        // The `running()` guard fires before the injector/client, so even a real tap is
        // rejected with no active app.
        let mut p = idle_platform();
        assert!(matches!(
            p.send_pointer(&tap()).unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    #[test]
    fn send_key_with_no_active_session_errors() {
        let mut p = idle_platform();
        assert!(matches!(
            p.send_key(&KeyEvent::Text("hi".into())).unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    #[test]
    fn window_geometry_returns_stored_geometry() {
        let mut p = running_platform();
        assert_eq!(p.window(&WindowOp::Geometry).unwrap(), geometry());
    }

    #[test]
    fn window_focus_returns_stored_geometry() {
        let mut p = running_platform();
        assert_eq!(p.window(&WindowOp::Focus).unwrap(), geometry());
    }

    #[test]
    fn window_resize_is_unsupported() {
        let mut p = running_platform();
        let err = p
            .window(&WindowOp::Resize {
                width: 100,
                height: 100,
            })
            .unwrap_err();
        assert!(matches!(err, GlassError::Unsupported(_)), "{err:?}");
    }

    #[test]
    fn window_move_is_unsupported() {
        let mut p = running_platform();
        let err = p.window(&WindowOp::Move { x: 1, y: 1 }).unwrap_err();
        assert!(matches!(err, GlassError::Unsupported(_)), "{err:?}");
    }

    #[test]
    fn window_with_no_active_session_errors() {
        let mut p = idle_platform();
        assert!(matches!(
            p.window(&WindowOp::Geometry).unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    #[test]
    fn list_windows_returns_one_window() {
        let mut p = running_platform();
        let windows = p.list_windows().unwrap();
        assert_eq!(windows.len(), 1);
        let w = &windows[0];
        assert_eq!(w.id, IOS_WINDOW_ID);
        assert!(w.active);
        assert_eq!(w.title.as_deref(), Some("tech.fixedwidth.demo"));
        assert_eq!(w.geometry, geometry());
    }

    #[test]
    fn select_window_matching_id_returns_geometry() {
        let mut p = running_platform();
        assert_eq!(p.select_window(IOS_WINDOW_ID).unwrap(), geometry());
    }

    #[test]
    fn select_window_other_id_is_not_found() {
        let mut p = running_platform();
        assert!(matches!(
            p.select_window(WindowId(2)).unwrap_err(),
            GlassError::WindowNotFound
        ));
    }

    #[test]
    fn capture_frame_with_no_active_session_errors() {
        let mut p = idle_platform();
        assert!(matches!(
            p.capture_frame(None).unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    #[test]
    fn get_clipboard_with_no_active_session_errors() {
        let mut p = idle_platform();
        assert!(matches!(
            p.get_clipboard().unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    #[test]
    fn set_clipboard_with_no_active_session_errors() {
        let mut p = idle_platform();
        assert!(matches!(
            p.set_clipboard("x").unwrap_err(),
            GlassError::NoActiveSession
        ));
    }
}
