//! `IosPlatform`: the `glass_core::Platform` implementation that drives a single
//! foreground app on an iOS Simulator over `xcrun simctl`.

use std::process::Command;
use std::time::{Duration, Instant};

use glass_core::Deadline;
use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region, Result, Stream,
    WindowGeometry, WindowId, WindowInfo, WindowOp,
};

use crate::capture::screenshot;
use crate::idb::client::IdbClient;
use crate::idb::companion::IdbCompanion;
use crate::injector::IdbInjector;
use crate::logs::LogStream;
use crate::simctl::Simctl;
use crate::target::{SimTarget, SimulatorRegistry};

/// The single foreground app this backend drives.
struct RunningApp {
    bundle_id: String,
    /// The launched process, when `simctl launch` reported one in a shape that parsed. Kept for
    /// the same reason the Android backend keeps it — it is what `app_pid` answers with, and what
    /// a liveness check on the operation path would need.
    pid: Option<u32>,
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
/// [`IdbDriver`]; without it the backend degrades to observe-only and those two report a clear
/// [`GlassError::Unsupported`], carrying the `driver_error` that disabled the driver.
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

/// The launched process id from `simctl launch`'s stdout, which reports `<bundle id>: <pid>`.
///
/// `None` for output in any other shape — a pid guessed out of an unexpected line could belong to
/// another process entirely, and watching the wrong one is worse than not watching.
pub(crate) fn pid_from_launch_output(stdout: &str, bundle_id: &str) -> Option<u32> {
    // Matched against the bundle just launched, not "the last colon in the output": a trailing
    // note like `retrying: 42` would otherwise yield a pid belonging to something else, and
    // watching the wrong process is worse than watching none.
    let prefix = format!("{bundle_id}: ");
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .find_map(|pid| pid.trim().parse().ok())
}

/// Where `simctl launch` is told to write the app's stderr, in the simulator's own filesystem.
/// One fixed path rather than a per-launch name: it is read only on the failure path, immediately
/// after the launch that wrote it, and a stale file from a previous launch is overwritten by the
/// next one.
const LAUNCH_STDERR_DEVICE_PATH: &str = "/tmp/glass-launch-stderr.txt";

/// How many of the app's last log lines to carry in a launch-failure message. Enough for a
/// Swift `fatalError` (its message, then the trap) without turning an error into a log dump.
const LAUNCH_FAILURE_LOG_LINES: usize = 4;

/// How long after launch an app that is going to abort has actually gone.
///
/// Measured on a Simulator: an app that rejects a launch argument and calls `fatalError`
/// disappears from `ps` about 465 ms after `simctl launch` returns, repeatably. The window is set
/// above that so the check is not a race, and it is spent rather than assumed: the work between
/// launch and check is a screenshot, plus one scale RPC when a driver is present, which does not
/// reliably cover it.
///
/// A window can only ever bound "died at startup"; an app that exits later is a running app that
/// stopped, which the next operation reports.
const LAUNCH_DEATH_WINDOW: Duration = Duration::from_millis(750);

/// The tail of what the app wrote to stderr, read from the simulator's filesystem.
///
/// `simctl launch --stderr=` writes inside the device, so the host path is the device's `dataPath`
/// (from `simctl list devices`) plus that path. Resolved here, on the failure path only, so a
/// healthy launch pays nothing for it. `None` whenever any step does not work out — this is a
/// diagnostic, and a missing diagnostic must not become a second failure.
fn launch_stderr_tail(simctl: &Simctl, udid: &str) -> Option<String> {
    let json = simctl.run(&["list", "devices", "-j"]).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&json).ok()?;
    let data_path = parsed
        .get("devices")?
        .as_object()?
        .values()
        .filter_map(|runtime| runtime.as_array())
        .flatten()
        .find(|device| device.get("udid").and_then(|u| u.as_str()) == Some(udid))?
        .get("dataPath")?
        .as_str()?
        .to_string();

    let text = std::fs::read_to_string(
        std::path::Path::new(&data_path).join(LAUNCH_STDERR_DEVICE_PATH.trim_start_matches('/')),
    )
    .ok()?;
    let tail: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(LAUNCH_FAILURE_LOG_LINES)
        .collect();
    if tail.is_empty() {
        return None;
    }
    Some(tail.into_iter().rev().collect::<Vec<_>>().join(" | "))
}

/// Terminate a launch that failed after `simctl launch` had already put the app on the screen, and
/// hand its error back — `start_app` records `self.app` only on success, so neither `stop_app` nor
/// the `Drop` below reaps one that failed on the way up (glass#421).
///
/// The caller's error is returned, not the reap's — the caller needs the failure that stopped the
/// launch.
fn reap_failed_launch(simctl: &Simctl, udid: &str, bundle_id: &str, e: GlassError) -> GlassError {
    if let Err(reap) = simctl.run(&["terminate", udid, bundle_id]) {
        // A reap usually fails for the same reason the launch did, and says so in plainer words
        // than the error being returned — a simulator that shut down under us reads there as a
        // screenshot that could not be decoded.
        eprintln!(
            "glass-ios: {bundle_id} launched but failed on the way up, and could not then be \
             terminated on {udid}: {reap}"
        );
    }
    e
}

/// Whether `pid` is still running, asked of the host: a Simulator app is an ordinary host
/// process, so `ps` answers for it without this crate taking a `libc` dependency for one probe.
///
/// `true` when the answer is not knowable — `ps` missing or failing to run is not evidence the
/// app died, and refusing a launch on that basis would turn a broken diagnostic into a broken
/// backend.
fn pid_is_running(pid: u32) -> bool {
    // Absolute path: `ps` resolved through `PATH` could be a shim whose non-zero exit would be
    // read below as "the app died", turning a healthy launch into a confident false report.
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-p", &pid.to_string(), "-o", "state="]);
    // A timeout here reads the same as `ps` being missing: not knowable, so not evidence the app
    // died (see this function's doc). Bounded so a wedged `ps` cannot hold up a launch.
    match glass_core::run_bounded(&mut cmd, std::time::Duration::from_secs(10), "ps:pid-state") {
        // A process that has exited but not yet been reaped is still listed, in state `Z`. That
        // is a real window here — the app's parent is `launchd_sim`, not this process — and a
        // zombie is a dead app, so the state is read rather than just the exit status.
        Ok(out) if out.status.success() => {
            let state = String::from_utf8_lossy(&out.stdout);
            let state = state.trim();
            !state.is_empty() && !state.starts_with('Z')
        }
        // No such process: `ps` exits non-zero and says nothing. Anything it complains about —
        // a usage error, a restricted environment — is a broken diagnostic rather than evidence
        // the app died, so it fails open and says so.
        Ok(out) if out.stderr.is_empty() => false,
        Ok(out) => {
            eprintln!(
                "glass-ios: could not check whether the app is still running: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            true
        }
        // Its sibling above logs before failing open; a `ps` that did not answer in ten seconds on
        // a healthy host is at least as worth saying out loud as one that printed to stderr.
        Err(e) => {
            eprintln!(
                "glass-ios: could not check whether the app is still running ({e}); \
                 treating it as running"
            );
            true
        }
    }
}

/// The `simctl launch` sub-command argv: the verb, the target simulator and bundle, then the
/// app's own launch arguments.
///
/// `app_args` go after the bundle id, which is where `simctl launch` stops reading arguments as
/// its own and starts forwarding: measured against a Simulator, everything past it arrives in the
/// app's `ProcessInfo.arguments` verbatim — separated pairs (`--tab collection`) as two elements,
/// joined ones (`--tab=collection`) as one, and a token that collides with a simctl option of its
/// own (`--console`) as the app's flag rather than simctl's.
pub(crate) fn launch_args(udid: &str, bundle_id: &str, app_args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "launch".to_string(),
        "--terminate-running-process".to_string(),
        // The app's own stdout/stderr is not in the device's unified log — a Swift `fatalError`
        // writes to stderr and traps — so without this a launch failure can only report what the
        // whole device happened to be saying. The path is inside the simulator's filesystem; the
        // host side of it is `<device dataPath>/tmp/…`, resolved only when a launch actually
        // fails (see `launch_stderr_tail`).
        format!("--stderr={LAUNCH_STDERR_DEVICE_PATH}"),
        udid.to_string(),
        bundle_id.to_string(),
    ];
    argv.extend(app_args.iter().cloned());
    argv
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
        let mut cmd = Command::new("plutil");
        cmd.args(["-extract", "CFBundleIdentifier", "raw", "-o", "-", &plist]);
        let out = glass_core::run_bounded(
            &mut cmd,
            std::time::Duration::from_secs(10),
            "plutil:CFBundleIdentifier",
        )?;
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
    /// Terminate the app, under `deadline` when the caller has one to share.
    fn stop_app_until(&mut self, deadline: Deadline) -> Result<()> {
        if let Some(app) = self.app.take() {
            // Best-effort teardown: the app may already be gone (it crashed, or a prior
            // launch's `--terminate-running-process` killed it), so a failing `terminate`
            // is not an error — dropping the session is the goal, and it is idempotent.
            let stopped = self
                .target
                .simctl()
                .run_until(&["terminate", self.target.udid(), &app.bundle_id], deadline);
            glass_core::note_if_skipped("terminating the app", &stopped);
        }
        Ok(())
    }

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

    /// The input/accessibility driver, or a clear `Unsupported` when there is no companion to
    /// run (observe-only mode). The message carries the recorded cause, so a caller sees *why*
    /// input is disabled rather than only that it is.
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

/// How long [`Platform::start_app`] waits for the unified-log stream to prove itself live
/// before launching the app. A booted simulator emits system log lines continuously, so this
/// is reached in milliseconds in practice; the cap only bounds a pathologically quiet or
/// failed stream so launch is never blocked for long.
const LOG_STREAM_READY_TIMEOUT: Duration = Duration::from_secs(2);

/// Discover the device's point→pixel scale, which idb reports for the *target* — so it is
/// known before the app has drawn anything, unlike the accessibility tree this used to derive
/// it from. `IosA11y` prefers the tree's own ratio where it has one, that being the only
/// independent second opinion: idb derives the screen's point width from this same density.
/// It never falls back to a placeholder — an undetermined scale would place every tap at the
/// wrong point, so the failure surfaces as an error and the session is left unstarted.
fn discover_scale(client: &IdbClient) -> Result<f64> {
    checked_scale(client.describe()?.density)
}

/// Reject a density that cannot be a scale: idb's field is a plain `double`, and a zero from
/// a target that failed to report one would divide every tap to infinity. Split from the RPC
/// so it is testable without a simulator.
pub(crate) fn checked_scale(density: f64) -> Result<f64> {
    if !density.is_finite() || density <= 0.0 {
        return Err(GlassError::Backend(format!(
            "could not determine the iOS display scale: idb reported a screen density of \
             {density}"
        )));
    }
    Ok(density)
}

impl Platform for IosPlatform {
    /// idb reports a `UISwitch` in a grouped table cell with the whole cell as its frame and
    /// the control at the trailing edge, so a centered `click_element` tap lands on the inert
    /// label. Opt into the trailing-aim (see the trait default's rationale).
    fn a11y_toggle_control_at_trailing_edge(&self) -> bool {
        true
    }

    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        // `self.app` is overwritten below, so an app still recorded here is one nothing would
        // reap again, `Drop` included. A no-op on the path `Glass` takes — a platform per
        // session.
        self.stop_app()?;

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

        // Start the unified-log stream BEFORE launching, and do not launch until its
        // subscription is provably live. `log stream` is a live tail with no backlog, so a
        // subscription that attaches after `simctl launch` misses the app's launch-time lines
        // (an `onAppear` / `applicationDidFinishLaunching` `os_log`). Gating launch on a
        // confirmed-live stream closes that race. Logs stay best-effort: a stream that never
        // comes up does not fail the launch — it only means a launch-time line may be missed,
        // which is noted rather than swallowed.
        let logs = LogStream::spawn(self.target.simctl(), udid);
        if !logs.wait_until_ready(LOG_STREAM_READY_TIMEOUT) {
            eprintln!(
                "glass-ios: unified-log stream not confirmed live before launch; a \
                 launch-time log line may be missed"
            );
        }

        // `SIMCTL_CHILD_<KEY>` is Apple's convention for passing environment variables through
        // `simctl launch` to the launched process, so `spec.env` is set that way rather than on
        // this (glass's own) process.
        let mut launch = Command::new(self.target.simctl().program());
        // `spec.run[1..]` are the app's launch arguments; `run[0]` was consumed above as the
        // bundle id (or the `.app` to install first).
        // `run[1..]` is in bounds: `bundle_id_from_run` above already rejected an empty `run`.
        let argv = launch_args(udid, &bundle_id, &spec.run[1..]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        launch.args(self.target.simctl().full_args(&argv));
        for (k, v) in &spec.env {
            launch.env(format!("SIMCTL_CHILD_{k}"), v);
        }
        // The app can be on the screen from here on, while `self.app` is not set until the end,
        // so every failure below reaps for itself.
        //
        // Bounded like every other simctl call, but spelled out here because this one cannot go
        // through `Simctl::run`: it needs `SIMCTL_CHILD_*` in the child's environment.
        let out = glass_core::run_bounded(
            &mut launch,
            crate::simctl::SimctlOp::Lifecycle.budget(),
            "simctl:launch",
        )
        // A timeout kills the `xcrun` client, not the app — `launchd_sim` launched that — so a
        // simulator that answered late leaves it running with nothing holding it.
        .map_err(|e| reap_failed_launch(self.target.simctl(), udid, &bundle_id, e))?;
        if !out.status.success() {
            // Usually nothing launched — but `simctl launch` reports enough kinds of failure
            // that "usually" is not worth leaving an app running on.
            return Err(reap_failed_launch(
                self.target.simctl(),
                udid,
                &bundle_id,
                GlassError::Backend(format!(
                    "simctl launch failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
            ));
        }
        // `simctl launch` returns as soon as launchd has spawned the process, so a zero exit says
        // the app started, not that it is still running: an app that aborts on a bad launch
        // argument or a failed assertion is already gone. Without this check the screenshot below
        // captures whatever the Simulator is showing — the home screen — and glass reports that
        // as the app's window, so the caller's next snapshot describes SpringBoard.
        let launched_at = Instant::now();
        let launch_pid = pid_from_launch_output(&String::from_utf8_lossy(&out.stdout), &bundle_id);
        if launch_pid.is_none() {
            // Not fatal — the launch itself succeeded — but say so, because the liveness check
            // below is now off and its absence is otherwise invisible: a future `simctl` that
            // moves or reshapes this line would silently restore the bug it exists to catch.
            eprintln!(
                "glass-ios: could not read the launched pid from `simctl launch` output; an app \
                 that exits at startup will not be noticed until the next operation"
            );
        }

        // Capture once, purely to learn the device's pixel dimensions for the geometry we
        // report. These are device *pixels* (the screenshot's raw resolution), not UIKit
        // points — the two differ by the device's point-to-pixel scale factor.
        let frame = screenshot(self.target.simctl(), udid)
            .map_err(|e| reap_failed_launch(self.target.simctl(), udid, &bundle_id, e))?;
        let geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        };
        // With a driver present, learn the point→pixel scale and build the injector at it
        // before reporting the app as running, so no tap is ever issued at an unverified
        // scale. On scale-discovery failure the app is left unregistered (no active session)
        // rather than driven at a wrong scale. In observe-only mode (no companion) there is
        // no injector — input is unsupported — but geometry is still reported so
        // capture/logs/clipboard work.
        let injector = match &self.driver {
            Some(driver) => Some(IdbInjector::new(discover_scale(&driver.client).map_err(
                |e| reap_failed_launch(self.target.simctl(), udid, &bundle_id, e),
            )?)),
            None => None,
        };
        // Everything above would look identical for an app that died on launch: `simctl launch`
        // exits 0 once launchd has spawned the process, and the screenshot then captures
        // whatever the Simulator is showing — the home screen — reported as the app's window.
        //
        // Asked once at least [`LAUNCH_DEATH_WINDOW`] has passed since the launch: an app that
        // aborts takes a beat to actually go, and an observe-only launch skips scale discovery,
        // so the remainder is waited for rather than assumed.
        //
        // This bounds startup only. An app that dies later is NOT noticed — every operation
        // gates on the session being registered, not on the process being alive — which is why
        // the pid is kept.
        if let Some(pid) = launch_pid {
            let elapsed = launched_at.elapsed();
            if let Some(remaining) = LAUNCH_DEATH_WINDOW.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
            if !pid_is_running(pid) {
                // The one failure below the launch that does not reap: `pid_is_running` fails
                // open, so reaching here is evidence the app is already gone.
                //
                // What the app itself said travels in the message rather than being left for
                // `glass_logs`: this return drops the `LogStream` and never registers a session,
                // so a caller sent to that tool would be told there is no active session. The
                // device's unified log would not help anyway — it carries the whole device, so
                // the app's last words arrive buried in whatever else was running.
                let printed = match launch_stderr_tail(self.target.simctl(), udid) {
                    Some(said) => format!("it said: {said}"),
                    None => "it printed nothing to stderr on the way out".to_string(),
                };
                return Err(GlassError::AppNotStarted(format!(
                    "{bundle_id} launched as pid {pid} and exited before its window could be \
                     reported. An app that rejects one of its launch arguments, or trips an \
                     assertion at startup, goes this way — {printed}"
                )));
            }
        }

        self.app = Some(RunningApp {
            bundle_id,
            pid: launch_pid,
            geometry: geometry.clone(),
            logs,
            injector,
        });
        Ok(geometry)
    }

    /// The launched app's pid, when `simctl launch` reported one this could parse.
    ///
    /// The iOS accessibility reader does not correlate by pid — idb describes the whole screen —
    /// so nothing in this backend consumes it today. It is answered anyway because the trait asks
    /// a question this backend now knows the answer to, and a caller comparing backends should
    /// not find iOS uniquely silent.
    fn app_pid(&self) -> Option<u32> {
        self.app.as_ref().and_then(|a| a.pid)
    }

    /// `terminate` measures ~101ms against the 3s all of teardown gets, so the deadline is not
    /// expected to bind. It is for the simulator that stopped answering, which would otherwise run
    /// out the 10s `SimctlOp::Query` budget and leave nothing for the hook behind it
    /// (glass#427).
    fn stop_app_by(&mut self, deadline: Deadline) -> Result<()> {
        self.stop_app_until(deadline)
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
        // `simctl pbcopy` takes the clipboard text on stdin, so it cannot go through `Simctl::run`,
        // which closes stdin — but it is a one-shot `simctl` call like any other and gets the same
        // deadline. The runner writes stdin on its own thread, so a `pbcopy` that exits without
        // reading everything cannot leave this blocked in `write`.
        let mut cmd = Command::new(self.target.simctl().program());
        cmd.args(
            self.target
                .simctl()
                .full_args(&["pbcopy", self.target.udid()]),
        );
        // Budget and label from the classifier, like every other simctl call — this site only
        // exists apart because the payload goes on stdin, not because it is special.
        let op = crate::simctl::SimctlOp::for_sub(&["pbcopy"]);
        let out =
            glass_core::run_bounded_with_stdin(&mut cmd, op.budget(), op.label(), text.as_bytes())?;
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
            WindowOp::Resize { .. } | WindowOp::Move { .. } => Err(GlassError::unsupported(
                "window_move_resize",
                crate::BACKEND,
                crate::capabilities().window_move_resize.note,
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

impl Drop for IosPlatform {
    /// Terminate the app on drop, for a platform dropped without an explicit `stop_app()` —
    /// parity with the other backends. The Simulator outlives the process, so the app would
    /// otherwise still be in the foreground for the next run (glass#421).
    ///
    /// Not a guarantee, as the X11 and Wayland drops also note: glass-mcp bounds teardown by
    /// `glass_core::TEARDOWN_BUDGET` and exits regardless, and this backend's `terminate` carries
    /// a longer budget than that (glass#427).
    ///
    /// `stop_app` takes `self.app`, so this is a no-op after it — and `Glass::stop`,
    /// `Glass::shutdown` and a session replacement all call it. The path they do not reach is a
    /// launch that failed on the way up, which is [`reap_failed_launch`]'s.
    fn drop(&mut self) {
        let _ = self.stop_app();
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
    fn a_reported_density_is_the_scale() {
        // 3x is what the Simulator reports for an iPhone 17.
        assert_eq!(checked_scale(3.0).unwrap(), 3.0);
    }

    #[test]
    fn a_density_that_cannot_be_a_scale_is_rejected() {
        // Guessing here would place every tap at the wrong point, so it is an error and the
        // caller leaves the session unstarted.
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let err = match checked_scale(bad) {
                Ok(scale) => panic!("{bad} must be rejected, was accepted as scale {scale}"),
                Err(e) => e,
            };
            assert!(
                matches!(&err, GlassError::Backend(m) if m.contains("display scale")),
                "{err}"
            );
        }
    }
}

/// State-machine tests: `IosPlatform` built directly (bypassing `from_env`) with a fake
/// `RunningApp` (or none) and a stub driver (or none), so none of these touch a real
/// simulator or `idb_companion` — no RPC is made, only the pure in-memory branching (session
/// guards, driver presence, the `Move`-is-a-noop short-circuit, the input-error surface).
///
/// Every `simctl` call a fixture does make — the log stream, and the `terminate` each platform
/// now issues as it drops — goes to `SimTarget::for_test`'s stand-in program, not to `xcrun`.
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
        let target = SimTarget::for_test();
        IosPlatform {
            app: Some(RunningApp {
                bundle_id: "tech.fixedwidth.demo".into(),
                pid: None,
                geometry: geometry(),
                logs: LogStream::spawn(target.simctl(), "fake"),
                injector: Some(IdbInjector::new(1.0)),
            }),
            target,
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
        let target = SimTarget::for_test();
        IosPlatform {
            app: Some(RunningApp {
                bundle_id: "tech.fixedwidth.demo".into(),
                pid: None,
                geometry: geometry(),
                logs: LogStream::spawn(target.simctl(), "fake"),
                injector: None,
            }),
            target,
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
        assert!(
            p.accessibility()
                .expect("no connect is attempted without a driver")
                .is_none()
        );
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
        let msg = err.to_string();
        assert!(msg.contains("ios backend"), "{msg}");
        assert!(msg.contains("window_move_resize"), "{msg}");
        assert!(msg.contains("full-screen"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
    }

    #[test]
    fn window_move_is_unsupported() {
        let mut p = running_platform();
        let err = p.window(&WindowOp::Move { x: 1, y: 1 }).unwrap_err();
        assert!(matches!(err, GlassError::Unsupported(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("ios backend"), "{msg}");
        assert!(msg.contains("window_move_resize"), "{msg}");
        assert!(msg.contains("full-screen"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
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

#[cfg(test)]
mod launch_args_tests {
    use super::launch_args;

    fn args(extra: &[&str]) -> Vec<String> {
        let extra: Vec<String> = extra.iter().map(|s| (*s).to_string()).collect();
        launch_args("UDID", "com.example.app", &extra)
    }

    #[test]
    fn a_run_with_no_arguments_launches_the_bundle_alone() {
        assert_eq!(
            args(&[]),
            [
                "launch",
                "--terminate-running-process",
                "--stderr=/tmp/glass-launch-stderr.txt",
                "UDID",
                "com.example.app"
            ]
        );
    }

    #[test]
    fn arguments_follow_the_bundle_id_in_order() {
        assert_eq!(
            args(&["--tab=collection", "--verbose"]),
            [
                "launch",
                "--terminate-running-process",
                "--stderr=/tmp/glass-launch-stderr.txt",
                "UDID",
                "com.example.app",
                "--tab=collection",
                "--verbose"
            ]
        );
    }

    #[test]
    fn simctl_options_stay_ahead_of_the_bundle_id_and_app_arguments_behind_it() {
        // Everything before the bundle id is simctl's, everything after is the app's. The stderr
        // capture belongs to simctl, so it must not drift into the app's argv.
        let built = args(&["--tab=collection"]);
        let bundle = built.iter().position(|a| a == "com.example.app").unwrap();
        let capture = built
            .iter()
            .position(|a| a.starts_with("--stderr="))
            .expect("the launch captures the app's stderr");
        assert!(capture < bundle, "--stderr is simctl's own option");
    }

    #[test]
    fn an_argument_that_looks_like_a_simctl_flag_still_lands_after_the_bundle_id() {
        // Everything after the bundle id belongs to the app, not to simctl: a caller passing
        // `--console` means their app's flag, and it must not be read as simctl's own.
        let built = args(&["--console"]);
        let bundle = built.iter().position(|a| a == "com.example.app").unwrap();
        let flag = built.iter().position(|a| a == "--console").unwrap();
        assert!(flag > bundle, "app arguments must follow the bundle id");
    }
}

#[cfg(test)]
mod launch_wiring_tests {
    use super::{bundle_id_from_run, launch_args};

    /// The builder is unit-tested above; this pins the *wiring* — that `start_app` feeds it
    /// everything after `run[0]` and nothing else. A slice off by one compiles and passes every
    /// builder test, and that is exactly the bug this backend shipped with.
    #[test]
    fn every_element_after_the_launch_target_becomes_an_app_argument() {
        // Not `com.example.app`: `looks_like_app_path` treats any element ending in `.app` as a
        // bundle path, so a bundle id whose last label is `app` takes the install branch and
        // reaches for `plutil`.
        let run: Vec<String> = ["com.example.myapp", "--first", "second", "--third=4"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let (_install, bundle_id) = bundle_id_from_run(&run).expect("a bare bundle id parses");

        let argv = launch_args("UDID", &bundle_id, &run[1..]);

        let bundle_at = argv.iter().position(|a| a == "com.example.myapp").unwrap();
        assert_eq!(
            &argv[bundle_at + 1..],
            ["--first", "second", "--third=4"],
            "the app's arguments are run[1..], in order, with nothing dropped or duplicated"
        );
    }
}

#[cfg(test)]
mod launch_pid_tests {
    use super::pid_from_launch_output;

    #[test]
    fn the_pid_follows_the_bundle_id_on_the_launch_line() {
        assert_eq!(
            pid_from_launch_output(
                "tech.fixedwidth.glassfixture: 78539\n",
                "tech.fixedwidth.glassfixture"
            ),
            Some(78539)
        );
    }

    #[test]
    fn surrounding_whitespace_does_not_hide_the_pid() {
        assert_eq!(
            pid_from_launch_output("  com.example.app: 42  \n\n", "com.example.app"),
            Some(42)
        );
    }

    #[test]
    fn a_pid_on_a_line_belonging_to_something_else_is_not_taken() {
        // The bug this guards: reading "the last colon-space in the output" would take 42 here,
        // which is some other process entirely.
        assert_eq!(
            pid_from_launch_output(
                "com.example.app: 7\nnote: retrying: 42\n",
                "com.example.app"
            ),
            Some(7)
        );
        assert_eq!(
            pid_from_launch_output("note: retrying: 42\n", "com.example.app"),
            None
        );
    }

    #[test]
    fn output_in_an_unrecognized_shape_yields_no_pid_rather_than_a_wrong_one() {
        // Better to lose the liveness check than to watch the wrong process: a pid parsed out of
        // an unexpected line could belong to anything.
        assert_eq!(pid_from_launch_output("", "com.example.app"), None);
        assert_eq!(
            pid_from_launch_output("com.example.app\n", "com.example.app"),
            None
        );
        assert_eq!(
            pid_from_launch_output("com.example.app: not-a-pid\n", "com.example.app"),
            None
        );
    }
}

#[cfg(test)]
mod pid_liveness_tests {
    use super::pid_is_running;

    #[test]
    fn this_very_process_reads_as_running() {
        assert!(pid_is_running(std::process::id()));
    }

    #[test]
    fn a_reaped_child_reads_as_gone() {
        // Reaped on purpose: an exited-but-unreaped process is a zombie, which `ps` still lists.
        // That case is handled by reading the state column, but this test is about the ordinary
        // one — a process that is simply no longer there.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a process that exits immediately");
        let pid = child.id();
        child.wait().expect("reap it");
        assert!(!pid_is_running(pid));
    }
}

/// Teardown: what reaches the Simulator when a session ends, however it ends. Each asserts on the
/// argv a `FakeSimctl` recorded — a `terminate` that never ran and one that did are otherwise the
/// same green test.
#[cfg(test)]
mod teardown_tests {
    use super::*;
    use crate::simctl::FakeSimctl;
    use glass_core::SandboxLevel;

    const BUNDLE: &str = "tech.fixedwidth.demo";

    /// An observe-only platform (no companion, so no RPC is reachable), holding a running app or
    /// nothing. One `SimTarget` throughout — a call escaping the fake is one the assertions
    /// cannot see.
    fn platform(fake: &FakeSimctl, running: bool) -> IosPlatform {
        let target = SimTarget::for_test_at(fake.program());
        let app = running.then(|| RunningApp {
            bundle_id: BUNDLE.into(),
            pid: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            logs: LogStream::spawn(target.simctl(), target.udid()),
            injector: None,
        });
        IosPlatform {
            target,
            app,
            driver: None,
            driver_error: Some("no companion in this test".into()),
        }
    }

    fn spec() -> AppSpec {
        spec_for(BUNDLE)
    }

    fn spec_for(bundle: &str) -> AppSpec {
        AppSpec {
            build: None,
            run: vec![bundle.to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 2_000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        }
    }

    /// The `terminate` calls, matched on the verb in argv position — `launch` carries
    /// `--terminate-running-process`, which a bare substring test counts as a teardown.
    fn terminations(fake: &FakeSimctl) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("simctl terminate "))
            .collect()
    }

    #[test]
    fn dropping_a_platform_that_still_holds_an_app_terminates_it() {
        let fake = FakeSimctl::new();
        drop(platform(&fake, true));

        assert_eq!(
            terminations(&fake),
            vec![format!("simctl terminate test-udid {BUNDLE}")],
            "all calls: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn a_platform_that_never_started_an_app_runs_nothing_at_all_on_drop() {
        let fake = FakeSimctl::new();
        drop(platform(&fake, false));

        // The whole list, not just the terminates — a drop that reached for anything else, a
        // `shutdown` of the simulator itself say, is as wrong as one that terminates nothing.
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    /// Named for the mechanism it can actually see: with the `Drop` deleted the count is still
    /// one, so what this discriminates is `stop_app` taking the session.
    #[test]
    fn stop_app_takes_the_session_so_the_drop_behind_it_repeats_nothing() {
        let fake = FakeSimctl::new();
        let mut p = platform(&fake, true);
        // Best-effort by construction today; when that changes (glass#428) this line changes with
        // it.
        p.stop_app().expect("stop is best-effort and cannot fail");
        drop(p);

        assert_eq!(terminations(&fake).len(), 1, "{:?}", fake.calls());
    }

    /// The iOS half of the composition guarantee: a `terminate` that never answers must not spend
    /// what the hook behind it needs (glass#427).
    #[test]
    fn a_terminate_that_never_answers_gives_up_at_the_shared_deadline() {
        let fake = FakeSimctl::new();
        fake.slow("terminate", 5);
        let mut p = platform(&fake, true);

        let started = std::time::Instant::now();
        p.stop_app_by(Deadline::at(
            std::time::Instant::now() + std::time::Duration::from_millis(300),
        ))
        .expect("teardown is best-effort and reports success either way");
        let waited = started.elapsed();

        assert!(
            waited < std::time::Duration::from_secs(2),
            "waited {waited:?} on a terminate that never answers — the 10s SimctlOp budget was \
             used instead of the deadline"
        );
        assert!(
            fake.called("terminate test-udid"),
            "it still has to ask: {:?}",
            fake.calls()
        );
    }

    /// A `w`x`h` opaque PNG, as `simctl io screenshot` writes one.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode a PNG");
        bytes
    }

    /// The leak the `Drop` alone does not cover: `start_app` records `self.app` on its last
    /// statement, so an app that launched and then failed to come up is held by nothing.
    ///
    /// Named for the terminate, not the app — the fake runs no app, so these tests see the call,
    /// never its effect.
    #[test]
    fn a_launch_that_fails_after_the_app_is_up_terminates_it_before_returning() {
        let fake = FakeSimctl::new();
        let mut p = platform(&fake, false);

        // The fake exits 0 having written no PNG, so the launch succeeds and the screenshot
        // taken for the window geometry is what fails.
        let err = p
            .start_app(&spec())
            .expect_err("a screenshot that decodes to nothing must not report a started app");

        assert!(
            matches!(err, GlassError::CaptureFailed(_)),
            "the launch failure must survive the reap: {err}"
        );
        assert!(p.app.is_none(), "no session may be registered");
        assert_eq!(
            terminations(&fake),
            vec![format!("simctl terminate test-udid {BUNDLE}")],
            "all calls: {:?}",
            fake.calls()
        );
        // The log stream goes through the same runner — hardcoding `xcrun` there is silent on a
        // host without it, and reaches the real tool with a fake UDID on macOS.
        assert!(
            fake.called("spawn test-udid log stream"),
            "all calls: {:?}",
            fake.calls()
        );
    }

    /// A reap is best-effort, but it must not swallow the failure it was called about.
    #[test]
    fn a_reap_that_itself_fails_still_returns_the_launch_failure() {
        let fake = FakeSimctl::new();
        fake.fails("terminate");
        let mut p = platform(&fake, false);

        let err = p
            .start_app(&spec())
            .expect_err("the screenshot still fails");

        assert!(
            matches!(err, GlassError::CaptureFailed(_)),
            "a failed reap must not replace the error it was reaping for: {err}"
        );
        assert_eq!(terminations(&fake).len(), 1, "{:?}", fake.calls());
    }

    /// The launch whose result this backend never sees is the one that leaves an app up.
    #[test]
    fn a_launch_simctl_reported_as_failed_is_reaped_too() {
        let fake = FakeSimctl::new();
        fake.fails("launch");
        let mut p = platform(&fake, false);

        p.start_app(&spec()).expect_err("the launch itself fails");

        assert_eq!(
            terminations(&fake),
            vec![format!("simctl terminate test-udid {BUNDLE}")],
            "all calls: {:?}",
            fake.calls()
        );
    }

    /// Starting a second app overwrites `self.app`, leaving the first running with nothing able
    /// to reap it — the failed-launch leak from the other end. Not reachable through `Glass`,
    /// which builds a platform per session.
    #[test]
    fn starting_a_second_app_terminates_the_one_the_platform_was_holding() {
        let fake = FakeSimctl::new();
        let mut p = platform(&fake, true);

        p.start_app(&spec_for("tech.fixedwidth.other"))
            .expect_err("the screenshot fails, as ever with this fake");

        assert_eq!(
            terminations(&fake),
            vec![
                format!("simctl terminate test-udid {BUNDLE}"),
                "simctl terminate test-udid tech.fixedwidth.other".to_string(),
            ],
            "the old app first, then the reap of the new one: {:?}",
            fake.calls()
        );
    }

    /// The same leak one step later, on the path a real device takes: capture succeeds and it is
    /// the companion's scale RPC that fails, so the app is up with no session to hold it.
    #[test]
    fn a_launch_whose_scale_never_resolves_terminates_the_app_before_returning() {
        let fake = FakeSimctl::new();
        fake.writes_screenshot(&png(2, 3));
        let mut p = IosPlatform {
            target: SimTarget::for_test_at(fake.program()),
            app: None,
            // The stub client dials its own dead endpoint — what a companion that died between
            // spawn and launch looks like from here.
            driver: Some(IdbDriver::for_test()),
            driver_error: None,
        };

        let err = p
            .start_app(&spec())
            .expect_err("a scale that never resolved must not report a started app");

        assert!(
            err.to_string().contains("idb describe"),
            "the failure must be the scale RPC, not something before it: {err}"
        );
        assert!(
            fake.called("io test-udid screenshot"),
            "capture must have been reached and answered: {:?}",
            fake.calls()
        );
        assert!(p.app.is_none(), "no session may be registered");
        assert_eq!(
            terminations(&fake),
            vec![format!("simctl terminate test-udid {BUNDLE}")],
            "all calls: {:?}",
            fake.calls()
        );
    }
}
