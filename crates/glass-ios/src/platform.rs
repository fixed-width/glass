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
    /// Builds HID events from glass input at the app's real, discovered point→pixel scale.
    /// Built in `start_app` once that scale is known — and only when a `driver` is present
    /// to discover it — so it never exists at a provisional scale. `None` in observe-only
    /// mode (no companion): input is unsupported then, so no injector is built.
    injector: Option<IdbInjector>,
}

/// The `idb_companion`-backed input/accessibility driver: the spawned companion process and
/// a gRPC client that carries HID input to it. Optional on the platform — absent when
/// `idb_companion` isn't installed (or failed to start), in which case the backend degrades
/// to observe-only.
struct IdbDriver {
    /// Owns the `idb_companion` process bound to this simulator; killed on `Drop`.
    companion: IdbCompanion,
    /// gRPC client that injects HID input into the simulator.
    client: IdbClient,
}

impl IdbDriver {
    /// Spawn the companion for `udid` and connect an input client to its socket.
    fn start(udid: &str) -> Result<IdbDriver> {
        let companion = IdbCompanion::spawn(udid)?;
        let client = IdbClient::connect(companion.socket())?;
        Ok(IdbDriver { companion, client })
    }

    #[cfg(test)]
    fn for_test() -> IdbDriver {
        IdbDriver {
            companion: IdbCompanion::for_test(),
            client: IdbClient::for_test(),
        }
    }
}

/// Drives a native app on an iOS Simulator over `xcrun simctl`. A single foreground app at a
/// time, reported as one fullscreen window; there is no window management to speak of.
///
/// Capture, logs, clipboard, and window queries run over `xcrun simctl` alone. Input
/// (tap/type/swipe/scroll) and the accessibility tree additionally need an `idb_companion`
/// [`IdbDriver`]; when the companion isn't available the backend degrades to observe-only:
/// capture/logs/clipboard keep working, while input and the accessibility tree report a
/// clear [`GlassError::Unsupported`]. The failure that disabled the driver is kept in
/// `driver_error` and surfaced in that message.
pub struct IosPlatform {
    target: SimTarget,
    app: Option<RunningApp>,
    /// The input/accessibility driver, or `None` when `idb_companion` is unavailable
    /// (observe-only mode).
    driver: Option<IdbDriver>,
    /// Why the driver is absent, kept to explain the `Unsupported` error on input/a11y use.
    /// `Some` exactly when `driver` is `None` after a real start attempt.
    driver_error: Option<String>,
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
    /// try to start its `idb_companion` input/accessibility driver.
    ///
    /// A driver start failure does NOT abort: the backend degrades to observe-only
    /// (capture/logs/clipboard keep working) and the cause is kept so input and the
    /// accessibility tree report a clear `Unsupported` when attempted. The degradation is
    /// non-silent — it is logged once here, surfaced on use, and the doctor warns.
    pub fn from_env(reg: &SimulatorRegistry) -> Result<Self> {
        let target = SimTarget::from_env(reg)?;
        let (driver, driver_error) = match IdbDriver::start(target.udid()) {
            Ok(driver) => (Some(driver), None),
            Err(e) => {
                let cause = e.to_string();
                eprintln!(
                    "glass-ios: idb_companion unavailable — running observe-only \
                     (capture/logs/clipboard); input and the accessibility tree are \
                     disabled: {cause}"
                );
                (None, Some(cause))
            }
        };
        Ok(Self {
            target,
            app: None,
            driver,
            driver_error,
        })
    }

    fn running(&self) -> Result<&RunningApp> {
        self.app.as_ref().ok_or(GlassError::NoActiveSession)
    }

    /// The input/accessibility driver, or a clear `Unsupported` when the companion is
    /// absent (observe-only mode). The message carries the recorded cause so a caller sees
    /// *why* input is disabled (not installed vs. installed-but-crashed).
    fn driver(&self) -> Result<&IdbDriver> {
        self.driver.as_ref().ok_or_else(|| {
            GlassError::Unsupported(match &self.driver_error {
                Some(cause) => {
                    format!("iOS input and accessibility require idb_companion: {cause}")
                }
                None => "iOS input and accessibility require idb_companion \
                         (install with: brew install idb-companion)"
                    .into(),
            })
        })
    }

    /// The driver plus the running app's injector — the two pieces every input path needs.
    /// `Unsupported` when there's no companion; `NoActiveSession` when no app is running.
    /// (The injector is present whenever an app started with a driver available, so the
    /// `driver()` check is the one that gates observe-only input.)
    fn input(&self) -> Result<(&IdbDriver, &IdbInjector)> {
        let driver = self.driver()?;
        let injector = self.running()?.injector.as_ref().ok_or_else(|| {
            GlassError::Backend(
                "iOS input: injector uninitialized despite an available companion".into(),
            )
        })?;
        Ok((driver, injector))
    }

    /// The Unix socket the backing `idb_companion` serves on, when a driver is present.
    /// The accessibility reader connects a second client to this same socket.
    pub fn socket_path(&self) -> Option<&std::path::Path> {
        self.driver.as_ref().map(|d| d.companion.socket())
    }

    /// Build an accessibility reader over a second client to the same companion socket, when
    /// a driver is present. Returns `Ok(None)` in observe-only mode (no companion): the
    /// accessibility tree, like input, needs the companion. A genuine connect failure while
    /// the companion IS present is still propagated as `Err` rather than degraded away.
    ///
    /// The session holds the platform and the accessibility reader as separate boxed trait
    /// objects, so the reader owns its own client rather than borrowing this one.
    pub fn accessibility(&self) -> Result<Option<crate::a11y::IosA11y>> {
        let Some(driver) = self.driver.as_ref() else {
            return Ok(None);
        };
        let client = IdbClient::connect(driver.companion.socket())?;
        Ok(Some(crate::a11y::IosA11y::new(client)))
    }
}

/// How many times [`retry_for_scale`] polls for a positive scale before giving up.
const SCALE_ATTEMPTS: usize = 3;
/// How long [`retry_for_scale`] waits between polls while the tree is still empty.
const SCALE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Poll `next_scale` up to [`SCALE_ATTEMPTS`] times, `delay` apart, for the device's
/// point→pixel scale. Each outcome is handled distinctly:
///
/// - `Ok(Some(scale))` — discovered; returned immediately.
/// - `Ok(None)` — the tree parsed but has no positive root width yet (the app may still be
///   rendering). This is the transient case worth retrying.
/// - `Err(e)` — a real describe/RPC failure (dead transport, unparseable JSON, a timeout).
///   Retrying can't help and a per-attempt timeout would cost the full deadline each try, so
///   it is surfaced at once, wrapped with its cause.
///
/// If every attempt yields `Ok(None)`, the give-up error names the still-unrendered tree.
/// `delay` is a parameter (rather than the [`SCALE_RETRY_DELAY`] constant inline) purely so
/// tests can drive the loop with no sleeps.
fn retry_for_scale(
    delay: Duration,
    mut next_scale: impl FnMut() -> Result<Option<f64>>,
) -> Result<f64> {
    for attempt in 0..SCALE_ATTEMPTS {
        match next_scale() {
            Ok(Some(scale)) => return Ok(scale),
            Ok(None) => {}
            Err(e) => {
                return Err(GlassError::Backend(format!(
                    "could not determine the iOS display scale; last describe error: {e}"
                )));
            }
        }
        if attempt + 1 < SCALE_ATTEMPTS {
            std::thread::sleep(delay);
        }
    }
    Err(GlassError::Backend(
        "could not determine the iOS display scale from the accessibility tree; \
         the app may not have finished rendering"
            .into(),
    ))
}

/// Discover the device's point→pixel scale by dividing the capture's pixel width by the
/// accessibility root's logical-point width. `describe` can briefly lag a launch, so the
/// retry loop in [`retry_for_scale`] tolerates a transient empty tree. It never falls back
/// to a placeholder: an undetermined scale would place every tap at the wrong point, so the
/// failure surfaces as an error and the caller leaves the session unstarted.
fn discover_scale(client: &IdbClient, frame_px_width: u32) -> Result<f64> {
    retry_for_scale(SCALE_RETRY_DELAY, || {
        let json = client.describe_all()?;
        Ok(crate::axmap::scale_from_width(&json, frame_px_width))
    })
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
        // points — the two differ by the device's point-to-pixel scale factor.
        let frame = screenshot(self.target.simctl(), udid)?;
        let geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        };
        // With a driver present, learn the point→pixel scale and build the injector at it
        // before reporting the app as running, so no tap is ever issued at an unverified
        // scale (`describe` reports point frames while the screenshot is in pixels; their
        // ratio is the scale). On scale-discovery failure the app is left unregistered (no
        // active session) rather than driven at a wrong scale. In observe-only mode (no
        // companion) there is no injector — input is unsupported — but geometry is still
        // reported so capture/logs/clipboard work.
        let injector = match &self.driver {
            Some(driver) => Some(IdbInjector::new(discover_scale(
                &driver.client,
                frame.width,
            )?)),
            None => None,
        };
        self.app = Some(RunningApp {
            bundle_id,
            geometry: geometry.clone(),
            logs: LogStream::spawn(udid),
            injector,
        });
        Ok(geometry)
    }

    fn stop_app(&mut self) -> Result<()> {
        if let Some(app) = self.app.take() {
            // Best-effort teardown: the app may already be gone (it crashed, or a prior
            // launch's `--terminate-running-process` killed it), so a failing `terminate`
            // is not an error — dropping the session is the goal, and it is idempotent.
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
        let (driver, injector) = self.input()?;
        let events = injector.pointer_events(event)?;
        if events.is_empty() {
            // A `Move` has no touch equivalent, so there is nothing to inject.
            return Ok(());
        }
        driver.client.hid(events)
    }

    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        let (driver, injector) = self.input()?;
        driver.client.hid(injector.key_events(event)?)
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

    #[test]
    fn retry_for_scale_returns_the_first_positive_scale() {
        // Empty tree twice (still rendering), then a real scale on the third poll: it must
        // keep trying and return that scale — the `describe`-lags-a-launch case.
        let calls = std::cell::Cell::new(0usize);
        let scale = retry_for_scale(Duration::ZERO, || {
            let n = calls.get();
            calls.set(n + 1);
            Ok(if n < 2 { None } else { Some(3.0) })
        })
        .expect("scale should be discovered on the third poll");
        assert_eq!(scale, 3.0);
        assert_eq!(calls.get(), 3, "should have polled exactly three times");
    }

    #[test]
    fn retry_for_scale_gives_up_after_the_last_attempt() {
        // A tree that never gains a width: after exhausting every attempt, give up with the
        // still-rendering error rather than looping forever or inventing a scale.
        let calls = std::cell::Cell::new(0usize);
        let err = retry_for_scale(Duration::ZERO, || {
            calls.set(calls.get() + 1);
            Ok(None)
        })
        .expect_err("an always-empty tree must give up, not succeed");
        assert_eq!(calls.get(), SCALE_ATTEMPTS, "should exhaust every attempt");
        assert!(
            matches!(&err, GlassError::Backend(m) if m.contains("finished rendering")),
            "give-up error should name the unrendered tree: {err:?}"
        );
    }

    #[test]
    fn retry_for_scale_surfaces_a_describe_error_without_retrying() {
        // A hard describe/RPC failure can't be fixed by waiting, so it must surface on the
        // first poll with its cause — never retried (retrying would cost a full deadline).
        let calls = std::cell::Cell::new(0usize);
        let err = retry_for_scale(Duration::ZERO, || {
            calls.set(calls.get() + 1);
            Err(GlassError::Backend("dead transport".into()))
        })
        .expect_err("a describe error must surface, not be retried");
        assert_eq!(calls.get(), 1, "a hard error must not be retried");
        assert!(
            matches!(&err, GlassError::Backend(m)
                if m.contains("last describe error") && m.contains("dead transport")),
            "error should wrap the describe cause: {err:?}"
        );
    }
}

/// State-machine tests: `IosPlatform` built directly (bypassing `from_env`) with a fake
/// `RunningApp` (or none) and a stub driver (or none), so none of these touch a real
/// simulator or `idb_companion` — `xcrun` is never invoked and no RPC is made, only the
/// pure in-memory branching (session guards, driver presence, the `Move`-is-a-noop
/// short-circuit, the input-error surface).
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

    /// A running app with a driver present: input reaches the injector (its RPC would fail,
    /// but these tests only exercise events that error or short-circuit before any RPC).
    fn running_platform() -> IosPlatform {
        IosPlatform {
            target: SimTarget::for_test(),
            app: Some(RunningApp {
                bundle_id: "tech.fixedwidth.demo".into(),
                geometry: geometry(),
                logs: LogStream::spawn("fake"),
                injector: Some(IdbInjector::new(1.0)),
            }),
            driver: Some(IdbDriver::for_test()),
            driver_error: None,
        }
    }

    /// A driver present but no app started: the session guard is what fires.
    fn idle_platform() -> IosPlatform {
        IosPlatform {
            target: SimTarget::for_test(),
            app: None,
            driver: Some(IdbDriver::for_test()),
            driver_error: None,
        }
    }

    /// Observe-only: no companion, so a running app has no injector. Capture/logs/clipboard
    /// would work; input and the accessibility reader degrade.
    fn observe_only_platform() -> IosPlatform {
        IosPlatform {
            target: SimTarget::for_test(),
            app: Some(RunningApp {
                bundle_id: "tech.fixedwidth.demo".into(),
                geometry: geometry(),
                logs: LogStream::spawn("fake"),
                injector: None,
            }),
            driver: None,
            driver_error: Some(
                "idb_companion not found (install: brew install idb-companion)".into(),
            ),
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
        // A driver is present, so the `running()` guard is what fires: even a real tap is
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
    fn send_pointer_gesture_is_unsupported_while_running() {
        // A multi-touch gesture reaches the injector (driver + app present) and is rejected
        // as Unsupported there — not dropped silently.
        let mut p = running_platform();
        let g = PointerEvent::Gesture {
            pointers: vec![],
            duration_ms: 100,
        };
        assert!(matches!(
            p.send_pointer(&g).unwrap_err(),
            GlassError::Unsupported(_)
        ));
    }

    #[test]
    fn send_key_unknown_chord_is_invalid_key_while_running() {
        // An unknown modifier in a chord is an InvalidKey from the injector, again reached
        // only because the driver + app are present.
        let mut p = running_platform();
        assert!(matches!(
            p.send_key(&KeyEvent::Chord("hyper+x".into())).unwrap_err(),
            GlassError::InvalidKey(_)
        ));
    }

    #[test]
    fn send_pointer_without_a_driver_is_unsupported() {
        // Observe-only: input degrades to a clear Unsupported (not a hard session error),
        // even though an app is running.
        let mut p = observe_only_platform();
        assert!(matches!(
            p.send_pointer(&tap()).unwrap_err(),
            GlassError::Unsupported(_)
        ));
    }

    #[test]
    fn send_key_without_a_driver_is_unsupported() {
        let mut p = observe_only_platform();
        assert!(matches!(
            p.send_key(&KeyEvent::Text("hi".into())).unwrap_err(),
            GlassError::Unsupported(_)
        ));
    }

    #[test]
    fn accessibility_is_none_without_a_driver() {
        // With no companion, there is no accessibility reader — and no connect is attempted
        // (Ok(None), not an error), so observe-only start-up never blocks on it.
        let p = observe_only_platform();
        assert!(p
            .accessibility()
            .expect("no connect is attempted without a driver")
            .is_none());
    }

    #[test]
    fn socket_path_is_none_without_a_driver() {
        let p = observe_only_platform();
        assert!(p.socket_path().is_none());
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
