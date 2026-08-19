//! The macOS `Platform` backend. `new()` runs the TCC preflight so a missing grant fails fast.

use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_application_services::AXUIElement;

use glass_core::frame::{Frame, Region};
use glass_core::logbuf::Stream;
use glass_core::platform::{
    AppSpec, KeyEvent, Platform, PointerEvent, SandboxLevel, WindowGeometry, WindowId, WindowInfo,
    WindowOp,
};
use glass_core::{GlassError, Result};

use crate::adoption_log::adoption_line;
use crate::axwindow;
use crate::clipboard_route::ClipboardRoute;
use crate::coords;
use crate::permissions;
use crate::process::{self, ClipLaunch, LogSink};
use crate::settle::{SettleOutcome, settle_by_polling};

/// Poll interval between discovery attempts in [`MacosPlatform::discover_window`], which
/// reimplements `scwindow::find_window_for_pids`'s `poll_until(100, ..)` loop so it can also
/// race `child.try_wait()`.
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Timeout for the fresh per-call window re-resolution [`MacosPlatform::capture_frame`],
/// [`MacosPlatform::send_pointer`], and [`MacosPlatform::send_key`] each do (via
/// `scwindow::find_window_by_id` when `active_window` is set, else
/// `scwindow::find_window_for_pids`). Covers a query round trip, not a launch: the window
/// existed as of the last successful call, unlike `start_app`'s `spec.timeout_ms`.
const WINDOW_RESOLVE_TIMEOUT: Duration = Duration::from_millis(2000);

/// How often to re-read an adopted window's geometry while waiting for it to stop changing, and
/// how long to keep waiting. macOS reports a window's geometry while it is still opening, so the
/// reading adoption captured is routinely a frame of the open animation — 11 of the 12 cold
/// launches measured for #263 needed at least one further poll before two readings agreed. This
/// path's own per-poll cost was never isolated; the nearest measurement is a different reader's,
/// `tests/window_adopt_probe.rs` clocking an AX `WindowOp::Geometry` read at ~45-100ms. The
/// budget bounds a window that never stops moving at all (a clock, a video player, a splash).
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SETTLE_BUDGET: Duration = Duration::from_millis(500);
/// Per-poll resolution bound, a tenth of [`WINDOW_RESOLVE_TIMEOUT`]: a settle poll that cannot
/// find the window ends the settle attempt outright rather than retrying (see
/// [`crate::settle::settle_by_polling`]'s `ReadFailed` handling). `settle_by_polling` checks its
/// deadline only between reads, so `SETTLE_BUDGET` is a soft cap — a resolve starting just
/// before the deadline can overrun it by nearly this much.
const SETTLE_RESOLVE_TIMEOUT: Duration = Duration::from_millis(200);

/// Read-back tolerance (pixels) [`MacosPlatform::window`]'s mutating ops use to decide
/// whether a `Move`/`Resize`'s read-back position/size actually matches what was requested
/// (see [`move_took_effect`]/[`resize_was_refused`]'s docs — a legitimate min/max-size clamp
/// is not a refusal). `8`px mirrors `axwindow.rs`'s own `FALLBACK_TOLERANCE_PX` — generous
/// enough to absorb point<->pixel rounding across a position/size round trip without masking
/// a real refusal.
///
/// Do not also use this to decide whether a change was requested at all (that is
/// [`REQUEST_EPSILON_PX`]): with one 8px constant for both roles, a fully-refused Move/Resize
/// whose target sat within 8px of the window's starting position reported success.
const WINDOW_OP_TOLERANCE_PX: i32 = 8;

/// Threshold (pixels) [`move_took_effect`]/[`resize_was_refused`] use to decide whether a
/// `Move`/`Resize` asked for a genuinely different position/size than the window already had —
/// as opposed to [`WINDOW_OP_TOLERANCE_PX`], which judges whether the *read-back* matches the
/// *request*. Must stay well under it, or a small real change that was totally refused reads as
/// no change requested.
const REQUEST_EPSILON_PX: i32 = 2;

/// macOS backend, driving the target app with ScreenCaptureKit + CGEvent + AXUIElement.
pub struct MacosPlatform {
    /// Logs drained by `drain_logs`, filled by `process::spawn`'s per-stream reader threads —
    /// `Arc<Mutex<_>>` because those threads push while `drain_logs` reads. Empty until
    /// `start_app` launches a child.
    logs: LogSink,
    /// The launched app's root pid; `None` until `start_app`.
    app_pid: Option<u32>,
    /// The launched child process, kept so `stop_app`/`Drop` can `process::terminate`
    /// it. `None` until `start_app` and after `stop_app` (idempotent).
    child: Option<Child>,
    /// The launched app's stdout/stderr readers, dropped in `stop_app`/`Drop` once the child is
    /// terminated. The app's write ends are inherited by everything it spawns, so a reader that
    /// ended only at EOF would park on a survivor's pipe (glass#477). Empty until `start_app`,
    /// and for a LaunchServices-adopted app, which glass never gave a pipe.
    taps: Vec<glass_pipe_unix::LineTap>,
    /// The active window's `CGWindowID` — the implicit target of `capture_frame`/
    /// `send_pointer`/`send_key`, per the `Platform` contract. Set by `start_app` to the first
    /// window discovered, and by `select_window`; `None` before `start_app` and after
    /// `stop_app`, where every per-call resolver below falls back to the
    /// first-on-screen-by-pid lookup.
    active_window: Option<u32>,
    /// How `get_clipboard`/`set_clipboard` route the active session's clipboard. Decided in
    /// `start_app` (success path, from `clip` below plus a live `clipboard::shim_present`
    /// check), reset to the fail-closed default (`Unsupported`) in `stop_app` — see
    /// `crate::clipboard_route`'s module doc for the full decision and the three routes.
    clipboard_route: ClipboardRoute,
    /// The clip-shim launch facts `process::spawn` produced for the current session's app —
    /// `Some` only for a contained, injectable launch (see `process::ClipLaunch`'s doc), `None`
    /// otherwise and until `start_app`. Held here, not consumed inside `start_app`, so the
    /// clipboard-routing decision can run after the launched window is confirmed; reset in
    /// `stop_app`.
    clip: Option<ClipLaunch>,
    /// Set when the active window belongs to a LaunchServices-adopted app — one `start_bundle`
    /// handed off to `NSWorkspace` (see its doc). `None` otherwise, including a bundle that
    /// direct-spawned successfully. The [`Adopted`]'s `disposition` records how `stop_app`/
    /// `Drop` must reap it: `Fresh` when `ffi::launch_bundle` started the app (glass terminates
    /// it), `PreExisting` when it was already running (glass leaves it).
    adopted: Option<Adopted>,
}

/// A LaunchServices-adopted app tracked by `start_bundle`'s handoff path, paired with how
/// `stop_app`/`Drop` must reap it ([`crate::bundle::Disposition::should_terminate`]).
struct Adopted {
    /// The adopted app's pid — an `NSRunningApplication` process id, always non-negative.
    pid: i32,
    /// Whether glass started this instance (reap it) or merely re-found one already running
    /// (leave it alive) — see [`crate::bundle::Disposition`].
    disposition: crate::bundle::Disposition,
}

impl Adopted {
    /// Reap the adopted app iff glass started it
    /// ([`crate::bundle::Disposition::should_terminate`]). [`crate::ffi::terminate_app`] is
    /// graceful-only (`-[NSRunningApplication terminate]`, no `SIGKILL` escalation), so an app
    /// that vetoes quit (an unsaved-changes sheet) is left running by design rather than killed
    /// mid-edit. With no escalation to fall back to, a failed request is reported: otherwise
    /// `stop_app` returns success while an app glass launched is still on screen.
    fn reap(self) {
        if self.disposition.should_terminate() && !crate::ffi::terminate_app(self.pid) {
            eprintln!(
                "glass: could not ask the app (pid {}) to quit — it is already gone, or no \
                 longer registered as a running application. If it is still running, quit it \
                 manually; glass does not force-kill an app it only adopted.",
                self.pid
            );
        }
    }
}

impl MacosPlatform {
    /// Construct the backend, failing fast if a required TCC grant is missing.
    pub fn new() -> Result<Self> {
        permissions::preflight()?;
        Ok(Self {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: None,
            child: None,
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: None,
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

    /// Poll for `child`'s window, alternating a single
    /// [`crate::scwindow::query_once_with_candidates`] discovery attempt with
    /// `child.try_wait()` so a crashed launch fails fast with [`GlassError::AppExited`]
    /// instead of riding out the whole `timeout_ms` budget — mirrors
    /// `glass-x11/src/platform.rs`'s `discover_window`. Can't delegate to
    /// `scwindow::find_window_for_pids`: that helper owns its *entire* poll loop internally,
    /// with no child handle to race against.
    ///
    /// Returns the whole [`crate::scwindow::WindowMatch`], whose `geometry` is the settled one
    /// — [`Self::settle_window`] re-reads past the adoption instant before this returns (#263).
    fn discover_window(
        child: &mut Child,
        pid: u32,
        timeout_ms: u64,
    ) -> Result<crate::scwindow::WindowMatch> {
        crate::ffi::app_kit_init();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            if let Some((m, candidates)) =
                crate::scwindow::query_once_with_candidates(&[pid as i32])?
            {
                // Stderr-only diagnostic, no behaviour change — see adoption_log's module doc (#263).
                eprintln!("{}", adoption_line(pid as i32, &candidates));
                return Ok(Self::settle_window(pid as i32, m));
            }
            match child.try_wait() {
                Ok(Some(status)) => return Err(GlassError::AppExited(status.code())),
                // A `try_wait` error means the child can no longer be reaped (e.g. the pid
                // is already gone): treat it as an exit, not a bare poll hiccup, so a
                // transient error here can't mask a handoff as a `Timeout` — `start_bundle`'s
                // handoff decision keys on `AppExited`.
                Err(_) => return Err(GlassError::AppExited(None)),
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Timeout(timeout_ms));
            }
            std::thread::sleep(DISCOVERY_POLL_INTERVAL);
        }
    }

    /// Poll for a window owned by `pid` alone, with no `Child` to race against —
    /// `start_bundle`'s handoff path adopts a pid LaunchServices owns, so unlike
    /// [`discover_window`] there is no `std::process::Child` to `try_wait` if the adopted app
    /// dies before its window appears; this loop only re-queries
    /// `scwindow::query_once_with_candidates` until `timeout_ms` elapses. The returned match's
    /// `geometry` is the settled one.
    fn discover_window_pid(pid: i32, timeout_ms: u64) -> Result<crate::scwindow::WindowMatch> {
        crate::ffi::app_kit_init();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            if let Some((m, candidates)) = crate::scwindow::query_once_with_candidates(&[pid])? {
                // Same adoption record as `discover_window` — see there (#263).
                eprintln!("{}", adoption_line(pid, &candidates));
                return Ok(Self::settle_window(pid, m));
            }
            if Instant::now() >= deadline {
                return Err(GlassError::Timeout(timeout_ms));
            }
            std::thread::sleep(DISCOVERY_POLL_INTERVAL);
        }
    }

    /// Re-read `adopted`'s geometry until two consecutive readings agree, and return the match
    /// carrying that settled geometry.
    ///
    /// Adoption reads the window's geometry the moment it finds it, which on macOS is routinely
    /// mid-open-animation — 11 of 12 cold launches measured on 2026-08-01 returned a geometry a
    /// few pixels off the settled one, and that geometry is what `Glass::start_on_inner` hands
    /// the caller and caches as the session's geometry.
    ///
    /// Never fails the launch: a window that never stops changing (a clock, a video player, an
    /// animating splash) would otherwise become unlaunchable, so a budget expiry reports the
    /// freshest reading rather than an error, as does a resolve that fails mid-settle. Neither
    /// path is silent.
    ///
    /// Settles on `geometry` alone, not the whole [`crate::scwindow::WindowMatch`] — `geometry`
    /// is rounded-to-pixel (`(v * scale).round()`), but `WindowMatch::origin_pt` is the raw
    /// unrounded point origin, so two readings can agree on `geometry` while `origin_pt` keeps
    /// drifting inside the same pixel. Comparing the whole match makes that drift block settling
    /// forever for a window that had already stopped moving on screen (#263 review). The closure
    /// captures every other field of the freshest successful read, so the returned match still
    /// carries them.
    fn settle_window(
        pid: i32,
        adopted: crate::scwindow::WindowMatch,
    ) -> crate::scwindow::WindowMatch {
        let window_id = adopted.window_id;
        let seed_geometry = adopted.geometry.clone();
        let mut freshest = adopted;
        let (geometry, outcome) =
            settle_by_polling(seed_geometry, SETTLE_BUDGET, SETTLE_POLL_INTERVAL, || {
                // `.map`, not `?`: `?` asks for `E: From<GlassError>` and leaves `E` itself
                // unconstrained, which `rustc` can't resolve on its own. `.map` pins the error
                // type to exactly `GlassError` without a turbofish.
                crate::scwindow::find_window_by_id(window_id, &[pid], SETTLE_RESOLVE_TIMEOUT).map(
                    |m| {
                        let geometry = m.geometry.clone();
                        freshest = m;
                        geometry
                    },
                )
            });
        // No-op on every path: `geometry` and `freshest.geometry` always come from the same read.
        freshest.geometry = geometry;
        match outcome {
            SettleOutcome::Settled => {}
            SettleOutcome::ReadFailed(e) => eprintln!(
                "glass-macos: window {} stopped resolving while settling ({e}); reporting {}x{} \
                 @({},{}), the last geometry read",
                freshest.window_id,
                freshest.geometry.width,
                freshest.geometry.height,
                freshest.geometry.x,
                freshest.geometry.y
            ),
            SettleOutcome::BudgetExpired => eprintln!(
                "glass-macos: window {} did not settle within {}ms; reporting {}x{} @({},{}), \
                 the last geometry read",
                freshest.window_id,
                SETTLE_BUDGET.as_millis(),
                freshest.geometry.width,
                freshest.geometry.height,
                freshest.geometry.x,
                freshest.geometry.y
            ),
        }
        freshest
    }

    /// Resolve the window `capture_frame`/`send_pointer`/`send_key` should target *this*
    /// call: `scwindow::find_window_by_id(active_window, [pid], ..)` once an active
    /// `CGWindowID` is set, falling back to the "first on-screen window for this pid" lookup
    /// when nothing is selected yet. Always fresh (never cached): the window may have
    /// moved/resized/closed since the last call.
    ///
    /// Scoped to `&[pid]` so a stale or foreign id can never silently resolve to another app's
    /// window — it surfaces `GlassError::WindowNotFound` instead.
    fn resolve_active_window(&self, pid: i32) -> Result<crate::scwindow::WindowMatch> {
        match self.active_window {
            Some(id) => crate::scwindow::find_window_by_id(id, &[pid], WINDOW_RESOLVE_TIMEOUT),
            None => crate::scwindow::find_window_for_pids(&[pid], WINDOW_RESOLVE_TIMEOUT),
        }
    }

    /// `start_app`'s bundle path, taken when `spec.run[0]` names a `.app` directory
    /// (`bundle::is_app_bundle`): direct-spawn the bundle's inner executable
    /// (`bundle::resolve_inner_exec`) exactly like a plain exec — same logs, containment, and
    /// window-tracking as the non-bundle path — and fall back to handing the launch off to
    /// `NSWorkspace` only if that direct spawn's process exits before a window ever appears
    /// (`GlassError::AppExited`). Any other discovery failure (e.g. `GlassError::Timeout`) is
    /// NOT a handoff signal — the direct-spawned process is still alive and simply never grew a
    /// window, so it's terminated and the error propagated.
    ///
    /// The handoff can't be Seatbelt-contained (`bundle::handoff_gate` fails closed: only
    /// `sandbox: off` may adopt it) and has no clipboard shim (an `NSWorkspace`-launched process
    /// is never `direct.env`'s `DYLD_INSERT_LIBRARIES` target), so its `ClipboardRoute` is
    /// decided directly rather than via [`decide_clip`].
    ///
    /// **Apple platform code is handed off without attempting the spawn**, when the spec allows
    /// a handoff at all. macOS gives a system app a launch constraint requiring it to be started
    /// by launchd/LaunchServices, and the kernel `SIGKILL`s one spawned as another process's
    /// child — `EXC_CRASH SIGKILL (Code Signature Invalid)`, `CODESIGNING` / "Launch Constraint
    /// Violation" — leaving a crash report and a "quit unexpectedly" dialog behind each time.
    /// Measured: 10 of 12 stock apps tried (Notes, Preview, Calculator, TextEdit, Reminders,
    /// Music, Chess, Dictionary, Console, Activity Monitor) die that way; Terminal, Disk Utility
    /// and Safari survive. Nothing in the signature distinguishes the two groups — both are
    /// `Platform identifier=` code, and App Sandbox does not correlate (Music and Console carry
    /// no app-sandbox entitlement and still die) — so this cannot predict which system apps
    /// *would* have spawned, and gives up the attempt for all of them at the cost of their piped
    /// logs (an `NSWorkspace` launch has no stdio to capture).
    ///
    /// A contained launch can't be handed off at all, so a constrained app asked for with
    /// containment still attempts the spawn, dies, and reaches the gate error — rather than
    /// glass pushing the caller to `sandbox: off` to avoid a crash dialog.
    fn start_bundle(&mut self, spec: &AppSpec, bundle: &Path) -> Result<WindowGeometry> {
        if spec.sandbox == SandboxLevel::Off && process::is_apple_platform_code(bundle) {
            return self.adopt_via_launch_services(spec, bundle);
        }
        // A-preferred: direct-spawn the inner executable (logs + containment + window
        // tracking), exactly as the plain-exec path does for a non-bundle `run[0]`.
        let inner = crate::bundle::resolve_inner_exec(bundle)?;
        // Swap only `run[0]` (the `.app` path) for the resolved inner executable, preserving
        // any launch args. `run[0]` is guaranteed present — `start_app` checked
        // `spec.run.first()` before delegating here.
        let mut direct = spec.clone();
        direct.run[0] = inner.to_string_lossy().into_owned();
        let process::Launch {
            mut child,
            clip,
            taps,
        } = process::spawn(&direct, self.logs.clone())?;
        let pid = child.id();
        match Self::discover_window(&mut child, pid, spec.timeout_ms) {
            Ok(m) => {
                // Foreground app: adopt exactly as the plain-exec Ok arm does.
                self.child = Some(child);
                self.taps = taps;
                self.app_pid = Some(pid);
                self.active_window = Some(m.window_id);
                // Defensive: a direct-spawn is never an adopted session — clear any stale
                // handoff record from a prior un-stopped session so it can't widen
                // `stop_app`/`Drop`'s reap blast radius (symmetric with the handoff path
                // below clearing `self.child`).
                self.adopted = None;
                self.clipboard_route = decide_clip(spec.sandbox, clip.as_ref());
                self.clip = clip;
                Ok(m.geometry)
            }
            Err(e) => {
                // The direct-spawned process is done with — dead, or it never grew a window.
                // On a *successful* handoff `clip` is always `None` (handoff requires
                // sandbox:off, which is never shim-injected), so `release_clip` does real work
                // only for a contained launch that failed.
                process::terminate(&mut child);
                release_clip(clip.as_ref());
                // Terminated first, so the final drain catches what the inner exec said on its
                // way out — which on this path is the diagnosis. Explicit rather than left to
                // the end of the scope: the handoff below runs in between.
                drop(taps);
                // Only an early inner-exec exit (`AppExited`) is the handoff signal; any other
                // failure is propagated as-is.
                //
                // `AppExited` conflates an app that re-execs itself through LaunchServices and
                // leaves a dead stub, a genuine early crash of the inner exec, and the kernel
                // killing a system app for a launch-constraint violation. All fall through to
                // the handoff.
                let GlassError::AppExited(_) = e else {
                    return Err(e);
                };
                self.adopt_via_launch_services(spec, bundle)
            }
        }
    }

    /// Hand the launch to `NSWorkspace`/LaunchServices and adopt the resulting process: the
    /// fallback when a direct spawn produced no window, and the direct route for Apple platform
    /// code (see [`Self::start_bundle`], which explains both entries).
    ///
    /// Fails closed unless `sandbox: off` — glass cannot Seatbelt-contain an app LaunchServices
    /// owns, so it refuses rather than silently dropping the containment the caller asked for.
    fn adopt_via_launch_services(
        &mut self,
        spec: &AppSpec,
        bundle: &Path,
    ) -> Result<WindowGeometry> {
        // Fail-closed: only sandbox:off may adopt an app glass can no longer contain.
        crate::bundle::handoff_gate(spec.sandbox)?;
        let id = crate::bundle::bundle_identifier(bundle)?;
        let (pid, disposition) = match crate::ffi::running_pid_for_bundle_id(&id) {
            Some(pid) => (pid, crate::bundle::Disposition::PreExisting),
            // `ffi::launch_bundle` blocks this thread on a channel until `NSWorkspace`'s
            // completion handler fires — on the true main thread, that handler could need a
            // run-loop turn this blocked thread isn't pumping.
            None => match crate::ffi::launch_bundle(
                bundle,
                spec.run.get(1..).unwrap_or_default(),
                spec.timeout_ms,
            ) {
                Ok(pid) => (pid, crate::bundle::Disposition::Fresh),
                // `launch_bundle` can start the app yet still error before its pid is
                // delivered (e.g. its completion handler timed out) — an orphan. The
                // `None` just found above means any instance running now is one we
                // spawned, so best-effort reclaim it before propagating the error.
                Err(e) => {
                    if let Some(pid) = crate::ffi::running_pid_for_bundle_id(&id)
                        && !crate::ffi::terminate_app(pid)
                    {
                        // The launch error propagating below says nothing about this
                        // stray app, and there is no `Child` here to escalate with.
                        eprintln!(
                            "glass: the launch failed and the instance it started \
                             (pid {pid}) could not be asked to quit; if it is still \
                             running, quit it manually"
                        );
                    }
                    return Err(e);
                }
            },
        };
        // Don't orphan a fresh launch whose window never appears in time: adopt only
        // after discovery succeeds, since `self.adopted` (what `stop_app`/`Drop` reap)
        // isn't set until below. Reaping here terminates only a `Fresh` launch — an
        // already-running instance we merely re-found is not ours to kill.
        let m = match Self::discover_window_pid(pid, spec.timeout_ms) {
            Ok(m) => m,
            Err(e) => {
                Adopted { pid, disposition }.reap();
                return Err(e);
            }
        };
        self.child = None;
        // AppKit's pids are `i32` but always non-negative for a real process, so the cast to
        // `app_pid`'s `u32` is exact.
        self.app_pid = Some(pid as u32);
        self.active_window = Some(m.window_id);
        self.adopted = Some(Adopted { pid, disposition });
        self.clip = None;
        self.clipboard_route = crate::clipboard_route::decide_route(SandboxLevel::Off, None);
        Ok(m.geometry)
    }
}

/// Decide the launched session's `ClipboardRoute` from the sandbox level and the clip-shim
/// launch facts `process::spawn` produced (`None` for an uncontained or non-injectable launch)
/// — shared by `start_app`'s and `start_bundle`'s direct-spawn success paths. `clip`'s presence
/// only means the launch was injectable; a live `clipboard::shim_present` check confirms the
/// swizzle actually took, so an injectable target whose injection silently failed lands on
/// `Unsupported` rather than a `Private` route to a pasteboard the app never saw.
fn decide_clip(sandbox: SandboxLevel, clip: Option<&ClipLaunch>) -> ClipboardRoute {
    match clip {
        Some(c) => {
            let confirmed = crate::clipboard::shim_present(&c.name);
            crate::clipboard_route::decide_route(sandbox, Some((&c.name, confirmed)))
        }
        None => crate::clipboard_route::decide_route(sandbox, None),
    }
}

/// Release `clip`'s named pasteboards (the content board and its `.ready` sentinel), a no-op
/// when `clip` is `None`. Named pasteboards persist system-wide until released, so every path
/// that ends a `ClipLaunch`'s lifetime — the discovery-failure arms, `stop_app`, `Drop` — must
/// call this or the boards leak for the life of the host process.
fn release_clip(clip: Option<&ClipLaunch>) {
    if let Some(c) = clip {
        crate::clipboard::release_named(&c.name);
        crate::clipboard::release_named(&format!("{}.ready", c.name));
    }
}

/// Bring the launched app (`app_pid`) frontmost so its windows register with the AX API —
/// otherwise `glass_a11y_snapshot` `WindowNotFound`s until the first input activates it. This
/// front-loads the raise the first `glass_click`/`glass_type` would do anyway.
///
/// **Best-effort — never fatal.** `input::focus` discards an OS-declined activation and returns
/// `Ok`, so its only possible error is `AppExited`: the app died after `discover_window`
/// confirmed its window. The launch still succeeded, and the next capture/a11y/input op
/// surfaces that `AppExited`, so this logs and continues.
fn activate_launched(app_pid: Option<u32>) {
    if let Some(pid) = app_pid
        && let Err(e) = crate::input::focus(pid as i32)
    {
        eprintln!("glass-macos: launched app (pid {pid}) exited before it could be activated: {e}");
    }
}

/// Bounds-check every window-relative coordinate `event` carries against `geom` via
/// `coords::check_in_bounds`, failing with `GlassError::CoordOutOfBounds` before
/// `input::send_pointer` maps a coordinate to a global point — the "no silently-wrong click"
/// invariant. Do not drop this as redundant with `glass_core::session`'s equivalent
/// `Session::check_bounds`: this crate's mac-gated integration tests call
/// `MacosPlatform::send_pointer` directly, bypassing the session layer. Mirrors
/// `Session::check_bounds`'s per-variant coverage (both endpoints of a `Drag`, every pointer of
/// a `Gesture`) so an out-of-bounds `Gesture` reports `CoordOutOfBounds`, not `Unsupported`.
fn check_pointer_bounds(event: &PointerEvent, geom: &WindowGeometry) -> Result<()> {
    let check = |x: i32, y: i32| coords::check_in_bounds(x, y, geom.width, geom.height);
    match *event {
        PointerEvent::Move { x, y } => check(x, y),
        PointerEvent::Click { x, y, .. } => check(x, y),
        PointerEvent::Scroll { x, y, .. } => check(x, y),
        PointerEvent::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            ..
        } => {
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

/// Map one `scwindow::AppWindow` into the `WindowInfo` [`MacosPlatform::list_windows`] returns,
/// given the backend's current `active_window`. A pure step so it's unit-testable without a live
/// `SCShareableContent` query.
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
/// `coords::pixel_geometry_from_content_rect`'s point->pixel scaling so this crate keeps one
/// scaling implementation, not two.
fn read_ax_geometry(el: &AXUIElement, scale: f64) -> Result<WindowGeometry> {
    let (x, y) = axwindow::ax_position(el)?;
    let (width, height) = axwindow::ax_size(el)?;
    Ok(coords::pixel_geometry_from_content_rect(
        x, y, width, height, scale,
    ))
}

/// True if a `Move { x, y }` request succeeded: either `x`/`y` was already (within
/// [`REQUEST_EPSILON_PX`]) `before`'s own position, or `after` is within
/// [`WINDOW_OP_TOLERANCE_PX`] of the target AND the window actually moved away from `before`.
/// Both checks are needed — see those constants' docs. The signal `window(op)`'s `Move` branch
/// uses to catch a macOS window that silently ignores `AXPosition`. Computed in `i64` before
/// `.abs()` so no cast can wrap (matches `coords.rs`'s house rule).
fn move_took_effect(before: &WindowGeometry, after: &WindowGeometry, x: i32, y: i32) -> bool {
    let requested_a_change = (i64::from(x) - i64::from(before.x)).abs()
        > i64::from(REQUEST_EPSILON_PX)
        || (i64::from(y) - i64::from(before.y)).abs() > i64::from(REQUEST_EPSILON_PX);
    if !requested_a_change {
        return true;
    }
    let reached_target = (i64::from(after.x) - i64::from(x)).abs()
        <= i64::from(WINDOW_OP_TOLERANCE_PX)
        && (i64::from(after.y) - i64::from(y)).abs() <= i64::from(WINDOW_OP_TOLERANCE_PX);
    let stayed_put = (i64::from(after.x) - i64::from(before.x)).abs()
        <= i64::from(REQUEST_EPSILON_PX)
        && (i64::from(after.y) - i64::from(before.y)).abs() <= i64::from(REQUEST_EPSILON_PX);
    reached_target && !stayed_put
}

/// True if a `Resize { width, height }` had no visible effect at all: `after` is (within
/// [`WINDOW_OP_TOLERANCE_PX`]) the same size as `before`, even though `width`/`height` asked
/// for a genuinely different size than `before`'s own (more than [`REQUEST_EPSILON_PX`]
/// different). Deliberately narrower than "does `after` exactly match the request": macOS may
/// legitimately clamp to an intermediate size (a min/max content-size constraint), and that
/// `after` geometry is still returned to the caller. Only a total no-op counts as a refusal.
/// Computed in `i64` before `.abs()` so no cast can wrap (matches `coords.rs`'s house rule).
fn resize_was_refused(
    before: &WindowGeometry,
    after: &WindowGeometry,
    width: u32,
    height: u32,
) -> bool {
    let requested_a_change = (i64::from(width) - i64::from(before.width)).abs()
        > i64::from(REQUEST_EPSILON_PX)
        || (i64::from(height) - i64::from(before.height)).abs() > i64::from(REQUEST_EPSILON_PX);
    let nothing_moved = (i64::from(after.width) - i64::from(before.width)).abs()
        <= i64::from(WINDOW_OP_TOLERANCE_PX)
        && (i64::from(after.height) - i64::from(before.height)).abs()
            <= i64::from(WINDOW_OP_TOLERANCE_PX);
    requested_a_change && nothing_moved
}

impl Platform for MacosPlatform {
    /// Run the optional build step, spawn the app, then confirm a window appears for its pid
    /// within `spec.timeout_ms` via ScreenCaptureKit's `SCShareableContent` enumeration —
    /// alternated with `child.try_wait()` so a crashed launch fails fast with
    /// `GlassError::AppExited` (see `discover_window`).
    ///
    /// **Main-thread affinity:** `discover_window` calls `ffi::app_kit_init()`, which requires
    /// the true main thread (`MainThreadMarker` panics off it).
    ///
    /// **`.app` bundles:** when `spec.run[0]` names a `.app` bundle directory
    /// (`bundle::is_app_bundle`), this delegates to [`Self::start_bundle`]; every other
    /// `run[0]` takes the plain-exec path below.
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        Self::run_build(spec)?;
        let run0 = spec
            .run
            .first()
            .ok_or_else(|| GlassError::AppNotStarted("empty run".into()))?;
        let geometry = if crate::bundle::is_app_bundle(run0) {
            self.start_bundle(spec, Path::new(run0))?
        } else {
            let process::Launch {
                mut child,
                clip,
                taps,
            } = process::spawn(spec, self.logs.clone())?;
            let pid = child.id();
            match Self::discover_window(&mut child, pid, spec.timeout_ms) {
                Ok(m) => {
                    self.child = Some(child);
                    self.taps = taps;
                    self.app_pid = Some(pid);
                    // Defensive: a plain exec is never an adopted session — clear any stale
                    // handoff record from a prior un-stopped session so it can't widen
                    // `stop_app`/`Drop`'s reap blast radius (symmetric with `start_bundle`'s
                    // handoff path clearing `self.child`).
                    self.adopted = None;
                    // The first window discovered for the launched app becomes the implicit
                    // target of capture/input, until `select_window` retargets it.
                    // `resolve_active_window` is what honors this field on every call.
                    self.active_window = Some(m.window_id);
                    // Decide the session's clipboard route now that the launched window is
                    // confirmed — see `decide_clip`'s doc.
                    self.clipboard_route = decide_clip(spec.sandbox, clip.as_ref());
                    self.clip = clip;
                    // Scale/origin/geometry are NOT cached here: `send_pointer` re-resolves the
                    // window on every call, since it may move or resize after this discovery.
                    m.geometry
                }
                Err(e) => {
                    // The window never appeared (or discovery otherwise failed): don't leak
                    // the spawned child.
                    process::terminate(&mut child);
                    // Nor the named pasteboards the shim may already have created — the app (and
                    // its ctor) ran before discovery. `self.clip` is never set on this path, so
                    // `stop_app` wouldn't release them; do it here (`release_clip`'s doc).
                    release_clip(clip.as_ref());
                    return Err(e);
                }
            }
        };
        // Activate the launched app so its window registers with the AX API (best-effort — see
        // `activate_launched`); the launch itself already succeeded above.
        activate_launched(self.app_pid);
        Ok(geometry)
    }

    /// Terminate the launched app (however it was launched) and clear the active pid/window.
    /// Idempotent — a call with nothing running is `Ok(())`.
    ///
    /// An `adopted` app (`start_bundle`'s NSWorkspace-handoff path, no `self.child`) is reaped
    /// first, per the `adopted` field's doc. This then falls through to the child/pasteboard
    /// cleanup rather than returning early: a session is `adopted` XOR `child`-backed, so the
    /// fall-through is a no-op, and it keeps `stop_app` and `Drop` structurally identical.
    /// Ignores the deadline — the quit-then-signal ladder is asserted against `TEARDOWN_BUDGET`
    /// instead.
    fn stop_app_by(&mut self, _deadline: glass_core::Deadline) -> Result<()> {
        if let Some(a) = self.adopted.take() {
            a.reap();
        }
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child);
        }
        // Terminated first, so each tap's final drain sees what the app wrote on the way out.
        // Dropping them ends the reader threads and releases the pipes, which anything the launch
        // left running would otherwise hold for the life of this process (glass#477).
        self.taps.clear();
        // Named pasteboards persist system-wide until released, and a leftover `.ready`
        // sentinel could mask a failed injection in a later session. Only released when a shim
        // launch name is known — uncontained/hardened launches have none.
        release_clip(self.clip.as_ref());
        self.app_pid = None;
        self.active_window = None;
        // Reset the route too, so a previous session's `Private(name)` can't leak into
        // get_clipboard/set_clipboard before the next start_app decides fresh.
        self.clipboard_route = ClipboardRoute::default();
        // Same for the clip-shim facts.
        self.clip = None;
        Ok(())
    }

    /// Capture the active window as an RGBA8 frame, re-resolving it fresh on every call
    /// (see `scwindow.rs`'s module doc — a `Retained<SCWindow>` can't be cached across calls).
    ///
    /// Targets `active_window` when set (`capture::capture_window_by_id`, exact `CGWindowID`
    /// match scoped to `&[pid]`), else the first-on-screen-window-for-this-pid path
    /// (`capture::capture_window`). Takes its own branch rather than calling
    /// `resolve_active_window` because capture needs the live `SCWindow` inside a single
    /// completion-handler callback to build its `SCContentFilter`, not just a `WindowMatch`
    /// snapshot.
    ///
    /// **Main-thread affinity:** like `start_app`, this reaches `ffi::app_kit_init()` and must
    /// run on the true main thread.
    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        match self.active_window {
            Some(id) => crate::capture::capture_window_by_id(id, &[pid as i32], region),
            None => crate::capture::capture_window(&[pid as i32], region),
        }
    }
    /// Map the active window into `input::send_pointer` — see `input.rs`'s module doc for the
    /// CGEvent details and its main-thread-affinity note.
    ///
    /// Re-resolves the window fresh via `resolve_active_window` on every call, like
    /// `capture_frame`: the window may have moved or resized since `start_app`, so a
    /// `scale`/`origin_pt`/geometry cached once would go stale. Both the bounds check
    /// (`check_pointer_bounds`) and the coordinate mapping use the freshly-resolved values. The
    /// CGEvent focus target is the resolved window's own owning pid (`m.pid`), not
    /// `self.app_pid` — the same pid today, but `m.pid` is the one tied to the window being
    /// clicked.
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
    /// Re-resolves the active window via `resolve_active_window` first, same as `send_pointer`,
    /// even though keyboard CGEvents target a *process*: `focus(pid)` activates the app and the
    /// event goes to whatever window that app currently has key/main, so there is no per-window
    /// keyboard targeting. The resolution still matters — if `active_window` is set but that
    /// window has closed, this surfaces `GlassError::WindowNotFound` instead of silently posting
    /// keys to whatever else is focused.
    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let m = self.resolve_active_window(pid as i32)?;
        crate::input::send_key(event, m.pid)
    }
    /// Resolve the active window's `AXUIElement` (fresh every call, same rationale as
    /// `resolve_active_window`) and dispatch `op` onto it:
    ///
    /// - `Focus` activates the owning app (`input::focus`), then `AXRaise`s and marks the
    ///   window `AXMain` — CGEvents alone can't target a specific window (see `send_key`'s
    ///   doc), this is what does. Propagates `input::focus`'s error if the owning app is gone
    ///   rather than raising an `AXUIElement` for a dead process.
    /// - `Move { x, y }` converts the target from global-screen PIXELS to Quartz POINTS
    ///   (`coords::global_pixel_to_point`) and sets `AXPosition`.
    /// - `Resize { width, height }` sets `AXSize`, re-sets `AXPosition` to its just-read
    ///   current value, then sets `AXSize` again — a window won't grow past its current
    ///   on-screen bounds via a single `AXSize` set alone.
    /// - `Geometry` performs no mutation.
    ///
    /// Every branch reads back the window's position/size afterward ([`read_ax_geometry`]) and
    /// returns it in pixels — the tool boundary's unit. `Move`/`Resize` additionally check the
    /// read-back reflects the request ([`move_took_effect`]/[`resize_was_refused`]), returning
    /// `GlassError::Backend` naming what didn't take rather than reporting success on a window
    /// that refused the change.
    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        permissions::preflight()?;
        let id = self.active_window.ok_or(GlassError::NoActiveSession)?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;

        // `&[pid as i32]`-scoped (final-review fix 1): `id` came from this backend's own
        // `active_window`, but scoping the lookup here too means a stale/foreign id can
        // never resolve to another app's window.
        let m = crate::scwindow::find_window_by_id(id, &[pid as i32], WINDOW_RESOLVE_TIMEOUT)?;
        let el = axwindow::ax_window_for_cgwindowid(pid as i32, id, m.geometry.clone(), m.scale)?;

        match *op {
            WindowOp::Focus => {
                crate::input::focus(pid as i32)?;
                axwindow::ax_raise(&el)?;
                axwindow::ax_set_main(&el)?;
                read_ax_geometry(&el, m.scale)
            }
            WindowOp::Move { x, y } => {
                let target_pt = coords::global_pixel_to_point((x, y), m.scale);
                axwindow::ax_set_position(&el, target_pt)?;
                let geom = read_ax_geometry(&el, m.scale)?;
                if !move_took_effect(&m.geometry, &geom, x, y) {
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
    /// `scwindow::list_app_windows` (one `SCShareableContent` query, all matches), mapping each
    /// into a `WindowInfo` via [`window_info_from`].
    ///
    /// **Main-thread affinity:** reaches `ffi::app_kit_init()` and must run on the true main
    /// thread.
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let windows = crate::scwindow::list_app_windows(&[pid as i32])?;
        Ok(windows
            .into_iter()
            .map(|w| window_info_from(w, self.active_window))
            .collect())
    }
    /// Retarget `active_window` to `id` — the `Platform` contract's "make `id` the active
    /// window, the implicit target of capture/input/window ops".
    ///
    /// First confirms `id` is on-screen AND owned by `self.app_pid` via
    /// `scwindow::find_window_by_id(id, &[app_pid], ..)`, which already maps a lookup that never
    /// turns up `id` before `WINDOW_RESOLVE_TIMEOUT` elapses to `GlassError::WindowNotFound` —
    /// exactly this method's "not currently one of the app's windows" contract, so no extra
    /// `Timeout` -> `WindowNotFound` mapping is needed. Only then does `active_window` change: a
    /// failed `select_window` leaves the previous target in place rather than clearing it.
    ///
    /// Then raises and focuses the window by delegating to `self.window(&WindowOp::Focus)`
    /// rather than duplicating its AXUIElement logic; that path's own read-back
    /// ([`read_ax_geometry`]) supplies the pixel `WindowGeometry` returned here, so the window
    /// is queried fresh once more rather than reusing the first lookup's stale geometry.
    ///
    /// If `self.window(&WindowOp::Focus)` fails after `active_window` was retargeted — the
    /// window closed in the gap between the check and the `AXRaise`/`AXMain` calls —
    /// `active_window` is rolled back rather than left pointing at a window this call never
    /// confirmed glass can operate on.
    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        permissions::preflight()?;
        let pid = self.app_pid.ok_or(GlassError::NoActiveSession)?;
        let previous = self.active_window;
        crate::scwindow::find_window_by_id(id.0 as u32, &[pid as i32], WINDOW_RESOLVE_TIMEOUT)?;
        self.active_window = Some(id.0 as u32);
        self.window(&WindowOp::Focus)
            .inspect_err(|_| self.active_window = previous)
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        std::mem::take(&mut *self.logs.lock().expect("log buffer mutex"))
    }
    fn app_pid(&self) -> Option<u32> {
        self.app_pid
    }
    /// Read the clipboard, routed per `clipboard_route` (decided in `start_app`; see
    /// `crate::clipboard_route`'s module doc): `RealGeneral` (uncontained) reads the real
    /// system pasteboard; `Private(name)` (contained + injectable + shim-confirmed) reads the
    /// shim-redirected named pasteboard; `Unsupported` (contained, non-injectable, or
    /// injection unconfirmed) fails closed — glass does not bridge one.
    fn get_clipboard(&mut self) -> Result<String> {
        match &self.clipboard_route {
            ClipboardRoute::RealGeneral => crate::clipboard::get(),
            ClipboardRoute::Private(name) => crate::clipboard::get_named(name),
            ClipboardRoute::Unsupported => Err(GlassError::Unsupported(
                "clipboard is isolated under macOS containment (target not injectable)".into(),
            )),
        }
    }

    /// Write the clipboard. Same routing as `get_clipboard`.
    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        match &self.clipboard_route {
            ClipboardRoute::RealGeneral => crate::clipboard::set(text),
            ClipboardRoute::Private(name) => crate::clipboard::set_named(name, text),
            ClipboardRoute::Unsupported => Err(GlassError::Unsupported(
                "clipboard is isolated under macOS containment (target not injectable)".into(),
            )),
        }
    }
}

impl Drop for MacosPlatform {
    /// Reap a still-running launched app on drop — parity with the X11/Wayland/Windows
    /// backends, so a backend dropped without an explicit `stop_app()` (panic-unwind, or the
    /// process-exit backstop) does not orphan its child. `process::terminate` is idempotent, and
    /// `child.take()` in `stop_app` means this is a no-op if `stop_app` already ran.
    ///
    /// A freshly-launched `adopted` app gets the same treatment via `ffi::terminate_app`; an
    /// activated-existing adoptee is left running, same as in `stop_app`.
    fn drop(&mut self) {
        if let Some(a) = self.adopted.take() {
            a.reap();
        }
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child);
        }
        // Release this session's named pasteboards too if `stop_app` didn't run (panic-unwind
        // or the process-exit backstop). `stop_app` sets `self.clip = None`, so this is a no-op
        // when it already ran; otherwise the boards would leak system-wide.
        release_clip(self.clip.as_ref());
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
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: None,
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
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: None,
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
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: None,
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
            taps: Vec::new(),
            active_window: Some(7),
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: None,
        };
        assert!(p.stop_app().is_ok());
        assert_eq!(p.active_window, None);
    }

    #[test]
    fn stop_app_clears_clipboard_route() {
        // `stop_app` must reset the route to the fail-closed default, so a later `start_app`
        // on the same MacosPlatform never inherits a stale `Private(name)` that would route
        // get_clipboard/set_clipboard to a pasteboard no longer in play. `clip` is None here,
        // so no pasteboard is touched.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: Some(42),
            child: None,
            taps: Vec::new(),
            active_window: Some(7),
            clipboard_route: ClipboardRoute::Private("tech.fixedwidth.glass.clip.42.1.1".into()),
            clip: None,
            adopted: None,
        };
        assert!(p.stop_app().is_ok());
        assert_eq!(p.clipboard_route, ClipboardRoute::default());
    }

    #[test]
    fn stop_app_clears_clip() {
        // `stop_app` must clear the stored `ClipLaunch` facts too, so a later session never
        // inherits a previous one's private pasteboard name. With `clip` set, this also runs
        // the `release_named` path for the content + `.ready` boards.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: Some(42),
            child: None,
            taps: Vec::new(),
            active_window: Some(7),
            clipboard_route: ClipboardRoute::Private("tech.fixedwidth.glass.clip.42.1.1".into()),
            clip: Some(ClipLaunch {
                name: "tech.fixedwidth.glass.clip.42.1.1".into(),
            }),
            adopted: None,
        };
        assert!(p.stop_app().is_ok());
        assert!(p.clip.is_none());
    }

    #[test]
    fn stop_app_reaps_a_fresh_adoption_and_resets_state() {
        // A `Fresh` adoptee is terminated on `stop_app` (glass started it). Pid 999_999
        // resolves to no live app, so `ffi::terminate_app` is a no-op — this exercises the
        // reap + full state reset without a real process, needing no TCC grant or main
        // thread. Every per-session field must clear so a later `start_app` decides fresh.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: Some(999_999),
            child: None,
            taps: Vec::new(),
            active_window: Some(7),
            clipboard_route: ClipboardRoute::Private("tech.fixedwidth.glass.clip.9.1.1".into()),
            clip: None,
            adopted: Some(Adopted {
                pid: 999_999,
                disposition: crate::bundle::Disposition::Fresh,
            }),
        };
        assert!(p.stop_app().is_ok());
        assert!(p.adopted.is_none());
        assert_eq!(p.app_pid(), None);
        assert_eq!(p.active_window, None);
        assert_eq!(p.clipboard_route, ClipboardRoute::default());
    }

    #[test]
    fn stop_app_leaves_a_pre_existing_adoption_but_resets_state() {
        // A `PreExisting` adoptee is left running (glass only raised it), but the adopted
        // record and the rest of the per-session state must still clear on `stop_app`.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: Some(999_999),
            child: None,
            taps: Vec::new(),
            active_window: Some(7),
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: Some(Adopted {
                pid: 999_999,
                disposition: crate::bundle::Disposition::PreExisting,
            }),
        };
        assert!(p.stop_app().is_ok());
        assert!(p.adopted.is_none());
        assert_eq!(p.app_pid(), None);
        assert_eq!(p.active_window, None);
    }

    #[test]
    fn start_app_rejects_an_empty_run() {
        // `start_app` must reject a spec with an empty `run` (no executable to launch) with a
        // structured `AppNotStarted`, before any FFI/main-thread work — construct the struct
        // directly (as the other tests here do) to bypass `new()`'s TCC preflight.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: None,
            child: None,
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::default(),
            clip: None,
            adopted: None,
        };
        let spec = AppSpec {
            build: None,
            run: vec![],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };
        assert!(matches!(
            p.start_app(&spec),
            Err(GlassError::AppNotStarted(_))
        ));
    }

    #[test]
    fn activate_launched_none_is_a_noop() {
        // No launched pid → nothing to activate; must short-circuit before touching AppKit.
        activate_launched(None);
    }

    #[test]
    fn activate_launched_swallows_a_dead_pid() {
        // The best-effort contract: a pid with no live app makes `input::focus` return
        // `AppExited` — it resolves `runningApplicationWithProcessIdentifier` to `None` before
        // ever calling `activateWithOptions`, so this needs no grant, no app, and no main
        // thread (same as `stop_app`'s pid-999_999 tests). `activate_launched` must log and
        // return, never panic or propagate: a regression tightening its `if let Err` into `?`
        // would change its `()` signature and fail to compile against `start_app`'s call.
        activate_launched(Some(999_999));
    }

    #[test]
    fn clipboard_is_unsupported_under_containment() {
        // Construct the struct directly (not `MacosPlatform::new()`, which runs a
        // Screen-Recording TCC preflight that the ungranted CI runner can't pass) — the
        // `Unsupported` route below returns before either clipboard method touches any
        // pasteboard, so this exercises the routing grant-free.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: None,
            child: None,
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::Unsupported,
            clip: None,
            adopted: None,
        };
        assert!(matches!(p.get_clipboard(), Err(GlassError::Unsupported(_))));
        assert!(matches!(
            p.set_clipboard("x"),
            Err(GlassError::Unsupported(_))
        ));
    }

    #[test]
    fn clipboard_real_general_route_reaches_the_real_pasteboard() {
        // RealGeneral must route to `crate::clipboard::get` rather than short-circuiting to
        // `Unsupported`. Read-only (never calls `set_clipboard`), matching `clipboard.rs`'s
        // own test-module discipline of never mutating the shared system pasteboard from an
        // automated test — this only proves the route reaches the real implementation.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(Vec::new())),
            app_pid: None,
            child: None,
            taps: Vec::new(),
            active_window: None,
            clipboard_route: ClipboardRoute::RealGeneral,
            clip: None,
            adopted: None,
        };
        assert!(!matches!(
            p.get_clipboard(),
            Err(GlassError::Unsupported(_))
        ));
    }

    #[test]
    fn new_agrees_with_preflight() {
        // The central invariant: new() must error iff preflight() errors. Guards against a
        // future edit that swallows the missing-grant propagation. On an ungranted CI runner
        // both are Err; on a granted box both are Ok.
        assert_eq!(
            crate::permissions::preflight().is_err(),
            MacosPlatform::new().is_err()
        );
    }

    #[test]
    fn check_pointer_bounds_accepts_inside_rejects_outside() {
        let geom = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
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
        let geom = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
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
        assert!(matches!(
            check_pointer_bounds(&ev, &geom),
            Err(GlassError::CoordOutOfBounds { .. })
        ));
    }

    #[test]
    fn window_info_from_marks_the_active_window() {
        let w = crate::scwindow::AppWindow {
            window_id: 7,
            geometry: WindowGeometry {
                x: 1,
                y: 2,
                width: 640,
                height: 480,
            },
            title: Some("Untitled".into()),
            application_name: Some("TestApp".into()),
        };
        let info = window_info_from(w.clone(), Some(7));
        assert_eq!(info.id, WindowId(7));
        assert_eq!(info.title, Some("Untitled".into()));
        assert_eq!(info.class, Some("TestApp".into()));
        assert_eq!(
            info.geometry,
            WindowGeometry {
                x: 1,
                y: 2,
                width: 640,
                height: 480
            }
        );
        assert!(info.active, "window_id matches active_window");

        let not_active = window_info_from(w, Some(8));
        assert!(
            !not_active.active,
            "window_id does not match a different active_window"
        );
    }

    #[test]
    fn window_info_from_is_not_active_when_no_window_is_selected() {
        let w = crate::scwindow::AppWindow {
            window_id: 7,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            title: None,
            application_name: None,
        };
        let info = window_info_from(w, None);
        assert!(!info.active);
        assert_eq!(info.title, None);
        assert_eq!(info.class, None);
    }

    #[test]
    fn move_took_effect_accepts_a_genuine_move_that_reached_target() {
        let before = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        let after_exact = WindowGeometry {
            x: 300,
            y: 200,
            width: 640,
            height: 480,
        };
        assert!(
            move_took_effect(&before, &after_exact, 300, 200),
            "exact match"
        );
        let after_tolerance = WindowGeometry {
            x: 304,
            y: 196,
            width: 640,
            height: 480,
        };
        assert!(
            move_took_effect(&before, &after_tolerance, 300, 200),
            "within tolerance"
        );
    }

    #[test]
    fn move_took_effect_rejects_when_read_back_is_far_from_target() {
        // The refusal case: AXSetAttributeValue(AXPosition) reported success but the
        // window is still sitting wherever it started.
        let before = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        let unmoved = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        assert!(
            !move_took_effect(&before, &unmoved, 500, 200),
            "x off by more than tolerance"
        );
        assert!(
            !move_took_effect(&before, &unmoved, 100, 600),
            "y off by more than tolerance"
        );
    }

    #[test]
    fn move_took_effect_rejects_a_total_refusal_even_for_a_small_requested_delta() {
        // Regression test for final-review fix 3: a small (but genuine, > REQUEST_EPSILON_PX)
        // requested delta that's fully refused must not be reported as success just because
        // the unmoved position happens to still land within WINDOW_OP_TOLERANCE_PX of the
        // target — the exact bug this fix closes.
        let before = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        let unmoved = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        // Requested delta is 5px (> REQUEST_EPSILON_PX's 2px) but <= WINDOW_OP_TOLERANCE_PX's
        // 8px, so the old single-tolerance logic silently accepted this as success.
        assert!(!move_took_effect(&before, &unmoved, 105, 200));
    }

    #[test]
    fn move_took_effect_is_true_when_no_real_change_was_requested() {
        // Target is within REQUEST_EPSILON_PX of `before` -- essentially a no-op move, so
        // whatever `after` reads back as is not a refusal to report.
        let before = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        let after = WindowGeometry {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        assert!(move_took_effect(&before, &after, 101, 201));
    }

    #[test]
    fn resize_was_refused_when_size_never_moves() {
        let before = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        let after = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        // Requested a real size change (800x600) but the window is still exactly where it
        // started — the silent-no-op case `window(op)`'s Resize branch must catch.
        assert!(resize_was_refused(&before, &after, 800, 600));
    }

    #[test]
    fn resize_was_refused_is_false_when_nothing_was_requested() {
        // width/height happen to equal `before`'s own size (e.g. a Resize to the current
        // size) — no change was requested, so an unchanged `after` is not a refusal.
        let before = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        let after = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        assert!(!resize_was_refused(&before, &after, 640, 480));
    }

    #[test]
    fn resize_was_refused_is_false_on_a_legitimate_clamp() {
        // macOS clamped to an intermediate size short of the request (e.g. a min-size
        // constraint) rather than ignoring the resize outright — expected behavior, not a
        // refusal; `window(op)` returns this actual geometry rather than erroring.
        let before = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        let after = WindowGeometry {
            x: 0,
            y: 0,
            width: 700,
            height: 480,
        };
        assert!(!resize_was_refused(&before, &after, 1200, 480));
    }

    #[test]
    fn resize_was_refused_is_false_when_the_read_back_matches_the_request() {
        let before = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        let after = WindowGeometry {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        assert!(!resize_was_refused(&before, &after, 800, 600));
    }

    #[test]
    fn resize_was_refused_catches_a_total_refusal_of_a_small_requested_delta() {
        // A small resize request — over REQUEST_EPSILON_PX but under the old
        // WINDOW_OP_TOLERANCE_PX threshold — that is fully refused must be caught. A 5px delta
        // did not used to count as "a change was requested", so a total no-op reported success.
        let before = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        let after = WindowGeometry {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        }; // unmoved
        assert!(resize_was_refused(&before, &after, 645, 480));
    }
}
