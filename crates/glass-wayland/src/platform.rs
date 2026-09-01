use std::cell::Cell;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, BoundDispatch, Deadline, Frame, GlassError, HostPathProtectionMode, KeyEvent,
    Platform, PointerEvent, ProtectedHostPath, Region, Result, SandboxLevel, Stream,
    TEARDOWN_BUDGET, Whose, WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use glass_exec_unix::{Resolved, resolve_path};
use glass_pipe_unix::LineTap;
use glass_proc_linux::{APP_REAP_GRACE, Asked, CLOSE_GRACE, ProcessIdentitySet};
use glass_sandbox_linux::{BwrapStatusPipe, BwrapStatusReader};
use smithay_client_toolkit::delegate_dispatch2;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use tempfile::TempDir;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_pointer::{Axis, ButtonState};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_output, wl_registry, wl_seat, wl_shm};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use std::collections::HashMap;

use crate::clipboard::remaining_timespec;
use crate::command::{LogSink, build_sway_command_with_status, sway_config};
use crate::input::evdev_button;
use crate::swayipc::{Ipc, Window as SwayWindow};

// glass-mcp gives the whole of teardown `glass_core::TEARDOWN_BUDGET` and then exits regardless, on a
// `spawn_blocking` thread that cannot be cancelled — so an ask-then-signal ladder that fills the
// budget would never get to the signal, and sway (plus Xwayland and the app) would outlive glass.
//
// This binds the ladder only. The a11y bus teardown that follows it in the same budget still
// reaps at `REAP_GRACE`, so a helper that ignores SIGTERM can take the whole teardown past the
// budget; that is pre-existing and not what this assertion is about.
const _: () = assert!(
    CLOSE_GRACE.as_millis() + APP_REAP_GRACE.as_millis() < TEARDOWN_BUDGET.as_millis(),
    "the close request + compositor reap must finish inside glass_core::TEARDOWN_BUDGET"
);

const INPUT_SETTLE: Duration = Duration::from_millis(8);

fn clamped_budget(deadline: Deadline, own: Duration, now: Instant) -> (Duration, Whose) {
    deadline.budget(own, now)
}

#[derive(Default)]
struct WaylandDispatch(Cell<bool>);

impl WaylandDispatch {
    fn mark(&self) {
        self.0.set(true);
    }

    fn deadline_error(&self, op: &str) -> GlassError {
        if self.0.get() {
            GlassError::caller_deadline_elapsed(op)
        } else {
            GlassError::deadline_not_started(op)
        }
    }

    fn classify(&self, op: &str, mut error: GlassError) -> GlassError {
        if let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        {
            return GlassError::input_cleanup_failed(
                operation,
                self.classify(op, *primary),
                *cleanup,
            );
        }
        if self.0.get()
            && error.bound_owner() == Some(Whose::Caller)
            && error.bound_dispatch() == Some(BoundDispatch::NotDispatched)
        {
            if let GlassError::Bounded {
                kind,
                whose,
                dispatch,
                message,
                ..
            } = &mut error
            {
                let cleanup_at = [
                    message.find("; cleanup failed"),
                    message.find("; input cleanup failed"),
                ]
                .into_iter()
                .flatten()
                .min();
                let cleanup = cleanup_at
                    .map(|at| message[at..].to_owned())
                    .unwrap_or_default();
                *kind = glass_core::BoundKind::TimedOut;
                *whose = Whose::Caller;
                *dispatch = BoundDispatch::MayHaveDispatched;
                *message = format!(
                    "{op}: the caller deadline elapsed before the operation answered{cleanup}"
                );
                return error;
            }
            return GlassError::caller_deadline_elapsed(op);
        }
        error
    }
}

fn run_wayland_call_by<T>(
    deadline: Deadline,
    op: &str,
    call: impl FnOnce(&WaylandDispatch) -> Result<T>,
) -> Result<T> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started(op));
    }
    let dispatch = WaylandDispatch::default();
    let answer = call(&dispatch).map_err(|error| dispatch.classify(op, error))?;
    if deadline.has_passed() {
        return Err(dispatch.deadline_error(op));
    }
    Ok(answer)
}

fn run_wayland_type_by<S: glass_core::TypeSink>(
    sink: &mut S,
    text: &str,
    dwell: Duration,
    deadline: Deadline,
) -> Result<()> {
    glass_core::run_type_by(sink, text, dwell, deadline)
}

fn input_settle_by(deadline: Deadline) -> Result<()> {
    if deadline.has_passed() {
        return Err(GlassError::caller_deadline_elapsed("input settle"));
    }
    let (sleep_for, _) = clamped_budget(deadline, INPUT_SETTLE, Instant::now());
    std::thread::sleep(sleep_for);
    if deadline.has_passed() {
        return Err(GlassError::caller_deadline_elapsed("input settle"));
    }
    Ok(())
}

fn recovery_needs_settle(recovered: usize) -> bool {
    recovered > 0
}

struct ActiveSession {
    child: Child,
    /// Host PID reported by Bubblewrap for contained launches, or sway's host PID for direct ones.
    ownership_root: u32,
    /// sway's stdout/stderr readers, dropped when the session is torn down. Everything sway
    /// spawns inherits its write ends, so an EOF-only reader parks on a survivor's pipe
    /// (glass#477).
    taps: Vec<LineTap>,
    _runtime_dir: TempDir, // kept alive: the wayland socket lives here
    socket_path: PathBuf,  // path to the sway wayland socket (for clipboard threads)
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    manager: ZwlrScreencopyManagerV1, // captures an output region (cropped to a window)
    output: wl_output::WlOutput,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    ipc: Ipc,
    output_size: (u32, u32), // compositor output extent (for pointer normalization)
    ids: HashMap<String, WindowId>, // foreign-toplevel identifier -> stable WindowId
    next_id: u64,
    recovery: crate::xwayland::Recovery, // re-maps toplevels the compositor lost (Xwayland apps)
    active: Option<String>,              // active window's foreign-toplevel identifier
    active_rect: WindowGeometry,         // active window's output rect (capture/input origin)
    geometry: WindowGeometry,            // active window geometry (session contract)
    time: u32,
    input_poison: Option<String>,
}

/// Linux/Wayland backend (wlroots protocols, per-session headless `sway` compositor).
pub struct WaylandPlatform {
    sway: PathBuf,
    logs: LogSink,
    active: Option<ActiveSession>,
    clipboard_owner: Option<crate::clipboard::ClipboardOwner>,
    dbus: Option<glass_dbus_linux::PrivateBus>,
    protected_host_paths: Vec<ProtectedHostPath>,
}

impl WaylandPlatform {
    pub fn new() -> Result<Self> {
        let sway = resolve_sway()?;
        Ok(Self {
            sway,
            logs: Arc::new(Mutex::new(Vec::new())),
            active: None,
            clipboard_owner: None,
            dbus: None,
            protected_host_paths: Vec::new(),
        })
    }

    /// Tear the session down: ask the app to close, then reap the whole launch.
    ///
    /// The app is asked first because a signal gives a toolkit app no shutdown path at all (see
    /// `glass_proc_linux::CLOSE_GRACE`), so everything it would have flushed on exit is lost and
    /// it can come back reporting a crash.
    ///
    /// The reap covers the app's own processes as well as sway's group. sway calls `setsid` for
    /// every app it `exec`s, so the app is in neither sway's process group nor its session —
    /// signalling that group alone reaches the compositor and nothing else. A display client
    /// usually dies anyway once its compositor goes away, but anything the app forked that is
    /// not a display client would be left running.
    fn kill_session(&mut self) {
        // Tear down the clipboard owner thread before the wayland socket disappears.
        drop(self.clipboard_owner.take());
        if let Some(mut s) = self.active.take() {
            // Snapshot the launch (sway, Xwayland, the app and anything it forked) before any of
            // it exits: once sway is reaped its descendants are reparented to init and can no
            // longer be found from its pid.
            let tree = wayland_host_tree(&s.child, s.ownership_root);
            let app = app_pids(&tree, s.child.id());
            let asked = request_close(&mut s.ipc);
            // Wait on the app's processes, not on sway's window list: an empty list only means
            // the surface is gone, and a toolkit that flushes state after destroying its window
            // would still be mid-shutdown. With no app pid to watch (nothing but the compositor
            // in the tree) fall back to the window list, which is all there is.
            let closed_itself = asked.await_close(CLOSE_GRACE, || {
                if app.is_empty() {
                    s.ipc.windows().is_ok_and(|w| w.is_empty())
                } else {
                    !glass_proc_linux::any_alive(&app)
                }
            });
            // Teardown reports what it asked, not what survived — doctor's deep probe is the
            // caller that reads this (glass#380).
            let _ = glass_proc_linux::reap_launch(
                &mut s.child,
                &tree,
                glass_proc_linux::APP_REAP_GRACE,
            );
            // Reaped first, so each tap's final drain sees what sway wrote on its way out.
            s.taps.clear();
            glass_proc_linux::disclose_teardown(&asked.outcome(closed_itself));
        }
        self.dbus = None;
    }
}

struct PendingWaylandSession {
    child: Child,
    status: Option<BwrapStatusReader>,
    ownership_root: Option<u32>,
}

impl PendingWaylandSession {
    fn poll_status(&mut self) -> Result<()> {
        let Some(status) = self.status.as_mut() else {
            return Ok(());
        };
        match status.poll_child_pid() {
            Ok(Some(pid)) => {
                self.ownership_root = Some(pid);
                self.status = None;
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(error) => Err(GlassError::SandboxUnavailable(format!(
                "could not read Bubblewrap child status: {error}"
            ))),
        }
    }

    fn status_confirmed(&self) -> bool {
        self.status.is_none()
    }
}

fn wayland_host_tree(child: &Child, ownership_root: u32) -> Vec<u32> {
    let mut tree = glass_proc_linux::proc_tree_pids(child.id());
    tree.extend(ProcessIdentitySet::from_host_root(ownership_root).host_pids());
    tree.sort_unstable();
    tree.dedup();
    tree
}

fn reap_pending(pending: &mut PendingWaylandSession) {
    // The status may have been buffered while discovery failed or while sway was exiting. Read it
    // before the authoritative snapshot so its host PID remains a reaping root after reparenting.
    // A malformed status still falls through to reap sway's known host tree.
    let _ = pending.poll_status();
    let mut tree = glass_proc_linux::proc_tree_pids(pending.child.id());
    if let Some(root) = pending.ownership_root {
        tree.extend(ProcessIdentitySet::from_host_root(root).host_pids());
    }
    tree.sort_unstable();
    tree.dedup();
    let _ = glass_proc_linux::reap_launch(&mut pending.child, &tree, glass_proc_linux::REAP_GRACE);
}

fn launch_ready(
    status_confirmed: bool,
    window_discovered: bool,
    deadline: Instant,
    observed_at: Instant,
) -> bool {
    status_confirmed && window_discovered && observed_at < deadline
}

fn launch_deadline_error(
    pending: &mut PendingWaylandSession,
    timeout_ms: u64,
    unrecovered_x11_windows: usize,
) -> GlassError {
    let status_confirmed = pending.status_confirmed();
    reap_pending(pending);
    if !status_confirmed {
        return GlassError::SandboxUnavailable(
            "Bubblewrap did not report a contained child PID".into(),
        );
    }
    if unrecovered_x11_windows > 0 {
        return GlassError::Backend(format!(
            "the app mapped {unrecovered_x11_windows} X11 window(s) the compositor never \
             surfaced; glass re-mapped them and they still did not appear within {timeout_ms}ms. \
             The session's Xwayland may be wedged — retry the launch."
        ));
    }
    GlassError::Timeout(timeout_ms)
}

#[cfg(test)]
impl WaylandPlatform {
    /// The active session's private runtime dir — where sway put both its wayland socket and its
    /// IPC socket. Lets a test observe the session over a connection this backend does not own.
    pub(crate) fn session_runtime_dir(&self) -> Option<&Path> {
        self.active.as_ref()?.socket_path.parent()
    }

    /// The compositor's own pid, for a test that must signal it rather than talk to it.
    pub(crate) fn session_compositor_pid(&self) -> Option<u32> {
        self.active.as_ref().map(|s| s.child.id())
    }
}

/// The X11 window id of each window sway currently reports — what a lost-window cross-check
/// compares the X server's mapped toplevels against. Native Wayland views have no X11 id and are
/// simply absent.
fn x11_ids(wins: &[SwayWindow]) -> Vec<u32> {
    wins.iter().filter_map(|w| w.x11_window).collect()
}

/// How long a launch waits before looking on the X side for a window the compositor lost: half
/// the launch's own budget, so the check and the re-map still fit in the other half, and never
/// less than one [`crate::xwayland::CHECK_INTERVAL`] (a caller's very short timeout would
/// otherwise leave no room to look at all).
fn start_recovery_after(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms / 2).max(crate::xwayland::CHECK_INTERVAL)
}

/// The launched app's own processes: everything in the session's process tree except the
/// compositor itself and the Xwayland it starts. Used as the liveness signal during teardown.
///
/// The Xwayland filter is a guard, not a routine exclusion: sway reparents Xwayland to init, so
/// it is normally outside this tree already (which is why finding it needs the scan in
/// `crate::xwayland::session_display` rather than a walk from here). Keeping the filter costs one
/// `/proc/<pid>/comm` read per process and means teardown cannot start waiting on the
/// compositor's own plumbing if that reparenting ever stops happening.
fn app_pids(tree: &[u32], sway_pid: u32) -> Vec<u32> {
    tree.iter()
        .copied()
        .filter(|&pid| pid != sway_pid && !crate::xwayland::is_xwayland(pid))
        .collect()
}

/// Ask every window in the session to close.
///
/// sway's `kill` is the compositor-side close request — `xdg_toplevel.close` to a native Wayland
/// client, `WM_DELETE_WINDOW` to an Xwayland one that advertises the protocol. Under Wayland the
/// *compositor* owns that ask, so glass goes through sway rather than talking to the client the
/// way the X11 backend does.
///
/// **What glass cannot tell here:** sway acknowledges the *command*, not the client's handling of
/// it, and for an Xwayland client that never opted into `WM_DELETE_WINDOW` wlroots closes the X
/// connection instead of asking. Both look identical from the outside — the window disappears —
/// so on this backend a client that was disconnected rather than asked is reported as a clean
/// close. The X11 backend can distinguish the two because it reads `WM_PROTOCOLS` itself.
///
/// Each window is asked separately rather than as one batched command: sway returns one outcome
/// per command, and a batch collapses them into a single first-failure, which would lose the
/// per-window accounting this returns.
fn request_close(ipc: &mut Ipc) -> Asked {
    let windows = match ipc.windows() {
        Ok(windows) => windows,
        Err(e) => return Asked::blocked(format!("glass could not reach the compositor: {e}")),
    };
    if windows.is_empty() {
        return Asked::none();
    }
    let total = windows.len();
    let asked = windows
        .iter()
        .filter(|w| {
            ipc.run_command(&format!("[con_id={}] kill", w.con_id))
                .is_ok()
        })
        .count();
    Asked::counted(total, asked, |unaskable| {
        format!("the compositor refused the close request for {unaskable} of its {total} window(s)")
    })
}

impl Drop for WaylandPlatform {
    fn drop(&mut self) {
        // Last resort for a `stop_app` that never happened (panicking test, early return), not a
        // guarantee: this asks the app to close, waits up to `CLOSE_GRACE`, reaps the tree and
        // only then drops the private bus. glass-mcp runs it on a detached thread nothing joins,
        // so a process exiting right after the drop kills it partway and orphans sway and the
        // bus (glass#415) — a shorter grace would not help. Call `stop_app`.
        self.kill_session();
    }
}

/// Why no sway was resolved. Two fields: doctor prints cause and fix in separate columns, the
/// launch path joins them into one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoSway {
    pub(crate) cause: String,
    pub(crate) remedy: &'static str,
}

const BUILD_A_SWAY: &str = "build it with https://github.com/fixed-width/sway-build \
     (./build.sh && ./build.sh install), or install a distro sway >=1.12";
const MAKE_IT_RUNNABLE: &str = "chmod +x it, or point GLASS_SWAY at a runnable sway >=1.12";
const SET_GLASS_SWAY: &str = "start glass with a PATH to search, or point GLASS_SWAY at a sway \
     >=1.12; or install the bundle, which is found without a PATH — \
     https://github.com/fixed-width/sway-build (./build.sh && ./build.sh install)";
pub(crate) const CHECK_THAT_SWAY: &str =
    "check that binary, or point GLASS_SWAY at a working sway >=1.12";

impl NoSway {
    fn nothing_qualifies() -> Self {
        NoSway {
            cause: "no sway >=1.12 found".into(),
            remedy: BUILD_A_SWAY,
        }
    }

    /// A bare `sway` and no `$PATH` to look it up in, and no bundle either. The bundle lookup did
    /// run — it reads the glass data dir, not `$PATH` — so a build is still one of the fixes.
    fn no_search_path() -> Self {
        NoSway {
            cause: "no sway >=1.12 found — PATH is unset in glass's environment, and no bundled \
                    sway is installed"
                .into(),
            remedy: SET_GLASS_SWAY,
        }
    }

    /// A sway that is installed and runnable but told glass nothing.
    fn silent(path: &Path, why: &str) -> Self {
        NoSway {
            cause: format!("{}: {why}", path.display()),
            remedy: CHECK_THAT_SWAY,
        }
    }

    fn not_runnable(cause: String) -> Self {
        NoSway {
            cause,
            remedy: MAKE_IT_RUNNABLE,
        }
    }

    /// Cause and fix in one string, for the launch path's single error message.
    fn message(&self) -> String {
        format!("{} — {}", self.cause, self.remedy)
    }
}

/// Find a sway ≥1.12 with no env-var config: PATH (if recent enough) → the glass
/// data dir (where the build tool installs the bundle) → next to this executable.
/// No silent fallback — a clear error if none qualifies.
pub(crate) fn resolve_sway() -> Result<PathBuf> {
    resolve_sway_verdict().map_err(|no| GlassError::Backend(no.message()))
}

/// [`resolve_sway`] keeping the cause and the fix apart, for doctor.
pub(crate) fn resolve_sway_verdict() -> std::result::Result<PathBuf, NoSway> {
    if let Some(overridden) = sway_override(std::env::var_os("GLASS_SWAY")) {
        return overridden;
    }
    // `None` when the environment carries no `$PATH` at all — a walk that never happened, kept
    // apart from one that came back empty (glass#373).
    //
    // The walk stays inline: a `fn` that only splits `$PATH` and delegates has a constant-return
    // mutation nothing can kill on a host with no sway on `$PATH`.
    let walk = std::env::var_os("PATH")
        .map(|path| sway_in_dirs(std::env::split_paths(&path), VERSION_PROBE_BUDGET));
    sway_verdict(walk, || {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        sway_bundle_in(data, exe_dir)
    })
}

/// Pure: the verdict, given what the `$PATH` walk found (`None` when there was no `$PATH` to
/// walk) and what the bundle lookup would find — the testable seam, with no global env and
/// nothing spawned.
///
/// `bundle` is a thunk, not a value: looking for a bundle a `$PATH` hit has already made redundant
/// costs a `current_exe` and a `stat` per candidate on every launch.
fn sway_verdict(
    walk: Option<PathWalk>,
    bundle: impl FnOnce() -> Resolved,
) -> std::result::Result<PathBuf, NoSway> {
    let walk = match walk {
        Some(PathWalk {
            found: Some(sway), ..
        }) => return Ok(sway),
        walked => walked,
    };
    match bundle() {
        Resolved::Found(p) => Ok(p),
        Resolved::NotExecutable(p) => Err(NoSway::not_runnable(format!(
            "{} is not executable",
            p.display()
        ))),
        // A bundled sway glass could not even stat: a permission, not a missing build (glass#474).
        Resolved::Unreadable(p, e) => Err(NoSway {
            cause: format!(
                "the bundled sway at {} could not be looked at ({e}) — it may be installed where \
                 glass cannot read it",
                p.display()
            ),
            remedy: MAKE_IT_RUNNABLE,
        }),
        // `NoSearchPath` cannot come from the bundle lookup, which walks fixed paths; both mean
        // there is no bundle to fall back to.
        Resolved::Absent | Resolved::NoSearchPath => Err(match walk {
            // A silent candidate on PATH outranks "nothing qualifies" — telling the user to build
            // one sends them past the sway they have.
            Some(PathWalk {
                silent: Some(no), ..
            }) => no,
            // A prefix the walk could not even look into outranks "no sway found" (glass#474).
            Some(PathWalk {
                unreadable: Some((p, e)),
                ..
            }) => NoSway {
                cause: format!(
                    "the sway at {} could not be looked at ({e}) — it may be installed where \
                     glass cannot read it",
                    p.display()
                ),
                remedy: MAKE_IT_RUNNABLE,
            },
            Some(_) => NoSway::nothing_qualifies(),
            None => NoSway::no_search_path(),
        }),
    }
}

/// The bundled sway: under the glass data dir, where the build tool installs it, then next to
/// this executable. The testable seam — both roots come from the environment.
///
/// A bundle there but unrunnable is named rather than skipped: a `cp` across machines or an unzip
/// drops the execute bit, and "no sway found, build one" would send the user to rebuild what they
/// already have.
fn sway_bundle_in(data: Option<PathBuf>, exe_dir: Option<PathBuf>) -> Resolved {
    let candidates = [
        data.map(|d| d.join("glass/sway/bin/sway")),
        exe_dir.map(|d| d.join("sway/bin/sway")),
    ];
    let mut first_non_executable = None;
    let mut first_unreadable = None;
    for cand in candidates.into_iter().flatten() {
        match resolve_path(&cand) {
            Resolved::Found(p) => return Resolved::Found(p),
            Resolved::NotExecutable(p) => {
                first_non_executable.get_or_insert(p);
            }
            // A bundle root glass could not stat (glass#474): a later runnable or
            // present-but-unrunnable copy still outranks it.
            Resolved::Unreadable(p, e) => {
                first_unreadable.get_or_insert((p, e));
            }
            // Each candidate is one path, judged on its own: `resolve_path` has no search list
            // to lack, so `NoSearchPath` cannot come from it. Both mean "nothing here".
            Resolved::Absent | Resolved::NoSearchPath => {}
        }
    }
    if let Some(p) = first_non_executable {
        return Resolved::NotExecutable(p);
    }
    first_unreadable
        .map(|(p, e)| Resolved::Unreadable(p, e))
        .unwrap_or(Resolved::Absent)
}

/// What `GLASS_SWAY` decides, or `None` when it is unset or empty and discovery should run.
///
/// An override skips the version gate, and fails closed when it names something glass cannot
/// spawn — whether that is nothing at all or a file without execute permission. Falling back to
/// discovery would chase a version-specific bug in a different binary.
fn sway_override(
    value: Option<std::ffi::OsString>,
) -> Option<std::result::Result<PathBuf, NoSway>> {
    let p = PathBuf::from(value.filter(|s| !s.is_empty())?);
    // `resolve_path`, not `is_executable_file`, so a path glass cannot even stat is named as a
    // permission, not "not executable" (glass#474).
    Some(match resolve_path(&p) {
        Resolved::Found(p) => Ok(p),
        Resolved::NotExecutable(p) => Err(NoSway::not_runnable(format!(
            "GLASS_SWAY={} is not an executable file",
            p.display()
        ))),
        Resolved::Unreadable(p, e) => Err(NoSway {
            cause: format!(
                "GLASS_SWAY={} could not be looked at ({e}) — it may point at a location glass \
                 cannot read",
                p.display()
            ),
            remedy: MAKE_IT_RUNNABLE,
        }),
        // `NoSearchPath` cannot come from `resolve_path`, which judges one path; a directory
        // answers `Absent`, so both keep the old "not an executable file" verdict.
        Resolved::Absent | Resolved::NoSearchPath => Err(NoSway::not_runnable(format!(
            "GLASS_SWAY={} is not an executable file",
            p.display()
        ))),
    })
}

/// How long ONE candidate gets to answer `--version`.
///
/// The budget only has to cover an `exec` on a loaded host. Unbounded, a `sway` that never exits
/// hung `glass_start` and `glass doctor` forever.
///
/// Per candidate, not one deadline across the walk: a first candidate that spent a shared deadline
/// would leave a good sway further along `PATH` unprobed — the failure glass#374 fixed.
pub(crate) const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// What a candidate said when asked for its version.
///
/// Not `Option<String>`: a binary that answered nothing, one that never answered, and one glass
/// could not run at all send a reader to three different remedies.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum VersionAnswer {
    /// It ran and exited; whatever it wrote to stdout, untrimmed and possibly empty. Empty or
    /// unparseable still counts as an answer — see [`sway_in_dirs`].
    Answered(String),
    /// Nothing came back within the budget it carries, so the child was sent SIGKILL. Only the
    /// direct child: anything it forked before wedging outlives the probe.
    TimedOut(Duration),
    /// No answer to be had, and why. The candidate may still have run — one that exited leaving
    /// something it started holding its output pipe lands here too.
    NoReply(String),
}

/// Ask `sway` for its version, under a time bound.
///
/// Shared by discovery and doctor so the two classify a silence the same way. Both production
/// callers pass [`VERSION_PROBE_BUDGET`]; the parameter is for tests.
pub(crate) fn ask_sway_version(sway: &Path, budget: Duration) -> VersionAnswer {
    let mut cmd = std::process::Command::new(sway);
    cmd.arg("--version");
    match glass_core::run_bounded(&mut cmd, budget, "sway:--version") {
        Ok(out) => VersionAnswer::Answered(String::from_utf8_lossy(&out.stdout).into_owned()),
        Err(e) if e.bound() == Some(glass_core::BoundKind::TimedOut) => {
            VersionAnswer::TimedOut(budget)
        }
        Err(e) => VersionAnswer::NoReply(e.to_string()),
    }
}

/// The outcome of walking `PATH`: the sway to use, the first candidate that was there and gave no
/// answer, and the first entry the walk could not look into — `unreadable`, the same from a
/// permission, remembered so an empty walk can report it (glass#474).
#[derive(Debug, Default, PartialEq, Eq)]
struct PathWalk {
    found: Option<PathBuf>,
    silent: Option<NoSway>,
    unreadable: Option<(PathBuf, String)>,
}

/// The first `sway` in `dirs` whose `--version` reports >= 1.12.
///
/// Only an answer ends the walk. A candidate that ran decides the outcome even when the version
/// is too old or unreadable — that means the bundle, never a different sway further along, since
/// `PATH` order is a precedence the user expressed.
///
/// A candidate that gave no answer expressed nothing, so it is stepped over: no execute
/// permission, a spawn that failed outright (`ENOEXEC` for a file that is not a binary, `ETXTBSY`
/// while something else holds it open for writing), or a wait that outstayed `budget`.
fn sway_in_dirs(dirs: impl Iterator<Item = PathBuf>, budget: Duration) -> PathWalk {
    let mut walk = PathWalk::default();
    for dir in dirs {
        let cand = dir.join("sway");
        // `resolve_path`, not `is_executable_file`, so a candidate glass cannot stat is reported
        // as a permission, not skipped as if it were not there (glass#474).
        match resolve_path(&cand) {
            Resolved::Found(_) => {}
            Resolved::NotExecutable(_) => continue,
            Resolved::Unreadable(p, e) => {
                // A later entry may still hold a sway; report the permission only if the walk
                // comes back empty.
                walk.unreadable.get_or_insert((p, e));
                continue;
            }
            Resolved::Absent | Resolved::NoSearchPath => continue,
        }
        let why = match ask_sway_version(&cand, budget) {
            VersionAnswer::Answered(ver) => {
                walk.found = match parse_sway_version(&ver) {
                    Some((maj, min)) if (maj, min) >= (1, 12) => Some(cand),
                    _ => None, // answered, just not usably -> the bundle, not a later sway
                };
                return walk;
            }
            VersionAnswer::TimedOut(budget) => {
                format!("did not answer `--version` within {budget:?}")
            }
            VersionAnswer::NoReply(why) => why,
        };
        // Logged even when a later candidate answers — the step-over costs a whole budget on
        // every launch and is otherwise invisible.
        eprintln!("glass-wayland: skipping {}: {why}", cand.display());
        walk.silent
            .get_or_insert_with(|| NoSway::silent(&cand, &why));
    }
    walk
}

/// Parse `"sway version 1.12-abc (...)"` -> `(1, 12)`.
fn parse_sway_version(s: &str) -> Option<(u32, u32)> {
    let v = s.split_whitespace().nth(2)?; // "1.12-abc"
    let mut nums = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty());
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next()?.parse().ok()?;
    Some((major, minor))
}

/// Pick an output-x one pixel away from `axx` for the focus-reassert nudge.
/// sway only re-evaluates pointer focus on motion, so the intermediate point
/// must be a genuine delta. Nudging left (`axx - 1`) is a no-op at the left
/// edge (`axx == 0`), which silently lost the first click/scroll there — so
/// nudge right instead, clamped to stay on a `w`-wide output.
fn nudge_x(axx: u32, w: u32) -> u32 {
    if axx > 0 {
        axx - 1
    } else {
        (axx + 1).min(w.saturating_sub(1))
    }
}

/// Find sway's `wayland-N` socket in the private runtime dir (sway uses
/// `wayland-1`, not cage's `wayland-0`). Ignores `wayland-N.lock` and `sway-ipc.*`.
pub(crate) fn find_wayland_socket(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        let n = name.to_string_lossy();
        let rest = n.strip_prefix("wayland-")?;
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            Some(e.path())
        } else {
            None
        }
    })
}

/// Mint or fetch the stable `WindowId` for a foreign-toplevel identifier.
fn mint_id(ids: &mut HashMap<String, WindowId>, next: &mut u64, identifier: &str) -> WindowId {
    if let Some(id) = ids.get(identifier) {
        return *id;
    }
    let id = WindowId(*next);
    *next += 1;
    ids.insert(identifier.to_string(), id);
    id
}

/// sway IPC rect (i32) -> `WindowGeometry`.
fn rect_to_geom(r: &crate::swayipc::Rect) -> WindowGeometry {
    WindowGeometry {
        x: r.x,
        y: r.y,
        width: r.width.max(0) as u32,
        height: r.height.max(0) as u32,
    }
}

/// How long one capture may take, from its request to the last event it needs — the format list,
/// the copy, and the `ready` that ends it.
const CAPTURE_BUDGET: Duration = Duration::from_secs(5);

/// Both ends are asserted live too, but only by a test that needs sway: on every other CI leg this
/// is the whole guard.
const _: () = assert!(
    CAPTURE_BUDGET.as_secs() >= 2 && CAPTURE_BUDGET.as_secs() <= 15,
    "the capture budget must leave a loaded compositor time to answer, without holding the session lock for long"
);

/// Half a message: the front arrived and the rest has not. Upstream's own reader returns this as
/// "nothing happened, ask again" rather than a fault, and a split write means a loaded host.
pub(crate) fn is_partial_read(e: &wayland_client::backend::WaylandError) -> bool {
    matches!(e, wayland_client::backend::WaylandError::Io(io)
        if io.kind() == std::io::ErrorKind::WouldBlock)
}

/// A fault raised while talking to the compositor, named for the operation that hit it. Both a
/// broken connection and a request the compositor refused (`wl_display.error`) arrive this way.
fn transport_failed(who: &str, what: &str, e: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("{who}: {what}: {e}"))
}

/// Send what is queued and dispatch what has already arrived, without waiting for more.
fn drain<S>(conn: &Connection, queue: &mut EventQueue<S>, state: &mut S, who: &str) -> Result<()> {
    conn.flush()
        .map_err(|e| transport_failed(who, "flush", e))?;
    queue
        .dispatch_pending(state)
        .map_err(|e| transport_failed(who, "dispatch", e))?;
    Ok(())
}

/// Wait for the compositor to send something, no later than `deadline`, and dispatch it.
///
/// `blocking_dispatch` waits on the socket with no timeout, so a deadline checked after it is not
/// a bound: a compositor that goes quiet holds the caller, and through the session lock every
/// other tool call (glass#383).
///
/// Expiry is not an error here — [`wait_for`] owns the deadline.
///
/// Generic over the queue's state so the bound can be tested with no compositor behind the
/// socket, and so capture and the sync every other request rides on share one implementation.
fn dispatch_until<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Instant,
    who: &str,
) -> Result<()> {
    // `None` means the backend's own queue needs draining — a libwayland condition the pure-Rust
    // backend glass builds never reports.
    let Some(guard) = queue.prepare_read() else {
        return drain(conn, queue, state, who);
    };
    let Some(ts) = remaining_timespec(deadline) else {
        return Ok(());
    };
    // The guard's fd, not the queue's: a queue is a dispatch bucket and has no socket of its own.
    let fd = guard.connection_fd();
    // POSIX reports POLLHUP in `revents` unasked, so a compositor that went away wakes this poll
    // and the read below fails with EPIPE rather than spinning.
    match rustix::event::poll(
        &mut [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )],
        Some(&ts),
    ) {
        Ok(0) => return Ok(()),
        Ok(_) => match guard.read() {
            Ok(_) => {}
            Err(e) if is_partial_read(&e) => {}
            Err(e) => return Err(transport_failed(who, "read", e)),
        },
        // A signal arrived and nothing was lost.
        Err(rustix::io::Errno::INTR) => return Ok(()),
        Err(e) => return Err(transport_failed(who, "poll", e)),
    }
    queue
        .dispatch_pending(state)
        .map_err(|e| transport_failed(who, "dispatch", e))?;
    Ok(())
}

/// Dispatch until `answered` has one, or until `deadline` passes with `expired` to say why not.
///
/// The deadline check lives here rather than in each caller's loop: [`dispatch_until`] returns at
/// once once the budget is spent, so a loop that forgot to check would spin on a core holding the
/// session lock rather than hang.
///
/// An answer in hand is taken before the deadline is judged, so a phase inheriting a spent budget
/// reports what arrived rather than a timeout.
fn wait_for<S, T>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Instant,
    who: &str,
    expired: impl FnOnce(&S) -> GlassError,
    mut answered: impl FnMut(&mut S) -> Option<Result<T>>,
) -> Result<T> {
    loop {
        drain(conn, queue, state, who)?;
        if let Some(answer) = answered(state) {
            return answer;
        }
        if Instant::now() >= deadline {
            return Err(expired(state));
        }
        dispatch_until(conn, queue, state, deadline, who)?;
    }
}

/// How long the compositor gets to answer one request.
///
/// One sync, not one tool call, and a tool call spends it many times over — `glass_type` syncs
/// twice per character, `glass_drag` once per waypoint. A compositor that has stopped answering
/// costs one, since the first failure ends the call; one answering just inside the budget every
/// time can hold the session lock for their sum.
const COMPOSITOR_SYNC_BUDGET: Duration = Duration::from_secs(5);

/// Only the sway-backed tests assert this live: on every other CI leg this is the whole guard.
const _: () = assert!(
    COMPOSITOR_SYNC_BUDGET.as_secs() >= 2 && COMPOSITOR_SYNC_BUDGET.as_secs() <= 15,
    "a sync must have time to answer on a loaded host, without holding the session lock for long"
);

/// What one pass of a discovery loop gives the compositor — short, because such a loop is also
/// watching for what the compositor cannot tell it: that sway exited, that a window needs
/// re-mapping.
const COMPOSITOR_SERVICE_SLICE: Duration = Duration::from_millis(250);

/// A slice a merely-busy compositor answers inside, since one it misses is discarded rather than
/// reported.
const _: () = assert!(
    COMPOSITOR_SERVICE_SLICE.as_millis() >= 50 && COMPOSITOR_SERVICE_SLICE.as_millis() <= 1000,
    "a servicing slice must not become a wait in its own right"
);

/// Set by the compositor's answer to `wl_display.sync`, which is what a roundtrip waits for.
pub(crate) type SyncDone = Arc<std::sync::atomic::AtomicBool>;

/// `EventQueue::roundtrip`, bounded — glass#402.
///
/// The unbounded one loops `blocking_dispatch` until the `done` event sets its flag, so a
/// compositor that has stopped answering holds the caller forever. This asks the same question
/// and gives it `deadline` to answer.
///
fn roundtrip_until_with<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Instant,
    who: &str,
    expired: impl FnOnce(Duration) -> GlassError,
) -> Result<()>
where
    S: Dispatch<wl_callback::WlCallback, SyncDone> + 'static,
{
    // Read before the wait, not in the failure: by then there is none left to report.
    let budget = deadline.saturating_duration_since(Instant::now());
    let done: SyncDone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    conn.display().sync(&queue.handle(), Arc::clone(&done));
    wait_for(
        conn,
        queue,
        state,
        deadline,
        who,
        |_| expired(budget),
        |_| {
            done.load(std::sync::atomic::Ordering::Relaxed)
                .then_some(Ok(()))
        },
    )
}

pub(crate) fn roundtrip_until<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Instant,
    who: &str,
) -> Result<()>
where
    S: Dispatch<wl_callback::WlCallback, SyncDone> + 'static,
{
    roundtrip_until_with(conn, queue, state, deadline, who, |budget| {
        // Name the caller because every request ends in the same sync.
        GlassError::Backend(format!(
            "{who}: the compositor did not answer within {} ms",
            budget.as_millis()
        ))
    })
}

/// Open a wayland connection and collect the compositor's globals, bounded.
///
/// `registry_queue_init` ends in `Connection::roundtrip`, which polls with no timeout, so every
/// bound below it is unreachable: an AF_UNIX connect to a compositor that has stopped answering
/// still succeeds (the kernel completes it into the listen backlog) and the setup then waits
/// forever. Upstream offers no bounded form and `GlobalList` cannot be built here, so the setup
/// runs on its own thread and the socket is shut down under it — the only way to end that poll.
pub(crate) fn connect_bounded<S>(
    socket: &Path,
    budget: Duration,
    who: &str,
) -> Result<(
    Connection,
    wayland_client::globals::GlobalList,
    EventQueue<S>,
)>
where
    S: Dispatch<wl_registry::WlRegistry, GlobalListContents> + Send + 'static,
{
    let stream = UnixStream::connect(socket)
        .map_err(|e| GlassError::Backend(format!("{who}: connect: {e}")))?;
    // The same socket, kept back from the thread: shutting it down is what wakes its poll, and
    // `Both` because a setup blocked writing to a compositor that stopped reading is as stuck.
    let watchdog = stream
        .try_clone()
        .map_err(|e| GlassError::Backend(format!("{who}: connect: {e}")))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let setup = std::thread::spawn(move || {
        let outcome = Connection::from_socket(stream)
            .map_err(|e| format!("wayland connection: {e}"))
            .and_then(|conn| {
                registry_queue_init::<S>(&conn)
                    .map(|(globals, queue)| (conn, globals, queue))
                    .map_err(|e| format!("wayland registry: {e}"))
            });
        let _ = tx.send(outcome);
    });

    match rx.recv_timeout(budget) {
        Ok(outcome) => {
            let _ = setup.join();
            outcome.map_err(|e| GlassError::Backend(format!("{who}: {e}")))
        }
        // The sender went with the thread, so the thread is gone: a panic, not a slow compositor.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let panicked = setup.join().is_err();
            Err(GlassError::Backend(format!(
                "{who}: the wayland setup ended without an answer{}",
                if panicked { " (it panicked)" } else { "" }
            )))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let _ = watchdog.shutdown(std::net::Shutdown::Both);
            // Joined, so a setup still running cannot outlive the call that started it.
            let _ = setup.join();
            Err(GlassError::Backend(format!(
                "{who}: the compositor did not answer within {} ms",
                budget.as_millis()
            )))
        }
    }
}

fn roundtrip_by<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Deadline,
    who: &str,
) -> Result<()>
where
    S: Dispatch<wl_callback::WlCallback, SyncDone> + 'static,
{
    let now = Instant::now();
    let (budget, owner) = clamped_budget(deadline, COMPOSITOR_SYNC_BUDGET, now);
    roundtrip_until_with(
        conn,
        queue,
        state,
        now + budget,
        who,
        move |budget| match owner {
            Whose::Caller => GlassError::caller_deadline_elapsed(who),
            Whose::Callee => GlassError::Backend(format!(
                "{who}: the compositor did not answer within {} ms",
                budget.as_millis()
            )),
        },
    )
}

/// The wayland objects one capture owns, destroyed when it ends however it ends.
///
/// Dropping a proxy destroys nothing, and `ready` is not a destructor — the client is what must
/// destroy a screencopy frame. The scratch in [`State`] is keyed by nothing, so an abandoned
/// frame's late events are taken by the next capture, which then reads a buffer nothing wrote;
/// bounding the wait (glass#383) is what makes that next capture reachable.
struct CaptureObjects {
    frame: ZwlrScreencopyFrameV1,
    buffer: Option<wl_buffer::WlBuffer>,
}

impl Drop for CaptureObjects {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.frame.destroy();
    }
}

/// What the compositor has said about the capture in flight, held apart from the connection so
/// that reading a capture out of it is a plain function, testable without one.
#[derive(Default)]
struct CaptureScratch {
    shm_buffers: Vec<(wl_shm::Format, u32, u32, u32)>, // advertised formats (format, w, h, stride)
    buffer_done: bool,                                 // v3: end of the format advertisement list
    done: Option<Result<()>>,                          // Some(Ok)=ready, Some(Err)=failed
}

impl CaptureScratch {
    /// The buffer to allocate, once the compositor has finished advertising formats.
    ///
    /// `None` while the list is still arriving — the caller's only reason to keep waiting.
    fn advertised(&mut self) -> Option<Result<(wl_shm::Format, u32, u32, u32)>> {
        if self.buffer_done {
            return Some(
                crate::pixels::pick_shm_format(&self.shm_buffers).ok_or_else(|| {
                    GlassError::CaptureFailed("screencopy: no shm format advertised".into())
                }),
            );
        }
        match self.done.take() {
            Some(Err(e)) => Some(Err(e)),
            // Nothing has been asked to be copied yet, so a `ready` is no request of this one's.
            Some(Ok(())) => Some(Err(GlassError::CaptureFailed(
                "screencopy: ready before the buffer list ended".into(),
            ))),
            None => None,
        }
    }

    /// Why the format list never finished — a list that started and stopped is a different fault
    /// from silence, and only it is about the version glass binds.
    fn no_formats(&self) -> String {
        if self.shm_buffers.is_empty() {
            "screencopy: no buffer event".into()
        } else {
            "screencopy: buffer formats advertised, but no buffer_done (v3) to end the list".into()
        }
    }
}

/// SCTK state: registry + output (for the output extent), shm (for capture
/// buffers), and the per-capture wlr-screencopy scratch (reset before each
/// capture). Window enumeration is via sway IPC, not foreign-toplevel.
struct State {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    capture: CaptureScratch,
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

// SCTK 0.21 folded the per-module `delegate_output!`/`delegate_shm!` macros into one
// blanket `delegate_dispatch2!`, which routes every SCTK-owned user-data type. Our own
// `Dispatch<_, ()>` impls below stay hand-written — `()` carries no `Dispatch2` impl,
// so they don't overlap the blanket one.
delegate_dispatch2!(State);
delegate_registry!(State);

// We don't recycle buffers (one pool per capture), so wl_buffer release is a no-op.
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// --- wlr-screencopy (manager has no events; frame events drive a capture) ---
impl Dispatch<wl_callback::WlCallback, SyncDone> for State {
    fn event(
        _: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        done: &SyncDone,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(f),
                width,
                height,
                stride,
            } => {
                state.capture.shm_buffers.push((f, width, height, stride));
            }
            Event::BufferDone => state.capture.buffer_done = true,
            Event::Ready { .. } => state.capture.done = Some(Ok(())),
            Event::Failed => {
                state.capture.done =
                    Some(Err(GlassError::CaptureFailed("screencopy failed".into())))
            }
            _ => {} // Flags, Damage, LinuxDmabuf, etc.
        }
    }
}

// The seat and virtual-pointer proxies carry no events we act on.
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Connect to `socket`, verify globals, bind screencopy + virtual-input managers,
/// read the output extent, and connect sway IPC. Returns everything for a session.
#[expect(
    clippy::type_complexity,
    reason = "one-shot session-setup tuple, destructured immediately by the sole caller"
)]
fn open_session(
    socket: &Path,
    runtime_dir: &Path,
    deadline: Instant,
) -> Result<(
    Connection,
    EventQueue<State>,
    State,
    ZwlrScreencopyManagerV1,
    wl_output::WlOutput,
    ZwlrVirtualPointerV1,
    ZwpVirtualKeyboardV1,
    Ipc,
    (u32, u32),
)> {
    let (conn, globals, mut queue): (_, _, EventQueue<State>) = connect_bounded(
        socket,
        deadline.saturating_duration_since(Instant::now()),
        "session bring-up",
    )?;

    let advertised: Vec<String> = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|g| g.interface)
        .collect();
    let advertised_refs: Vec<&str> = advertised.iter().map(String::as_str).collect();
    crate::globals::verify_globals(&advertised_refs)?;

    let qh = queue.handle();
    let mut state = State {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(|e| GlassError::Backend(format!("bind shm: {e}")))?,
        capture: CaptureScratch::default(),
    };
    // v3 exactly, not 1..=3: capture waits for `buffer_done`, which only v3 sends. The v1/v2
    // branch that waited differently was unreachable — glass only talks to the sway it launched.
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 3..=3, ())
        .map_err(|e| GlassError::Backend(format!("bind screencopy v3: {e}")))?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| GlassError::Backend(format!("bind seat: {e}")))?;
    let vp_manager: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| GlassError::Backend(format!("bind virtual pointer: {e}")))?;
    let vk_manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| GlassError::Backend(format!("bind virtual keyboard: {e}")))?;

    roundtrip_until(&conn, &mut queue, &mut state, deadline, "session bring-up")?;

    let output = state
        .output
        .outputs()
        .next()
        .ok_or_else(|| GlassError::Backend("compositor advertised no output".into()))?;
    let info = state
        .output
        .info(&output)
        .ok_or_else(|| GlassError::Backend("no output info".into()))?;
    let (w, h) = info
        .logical_size
        .or_else(|| info.modes.iter().find(|m| m.current).map(|m| m.dimensions))
        .ok_or_else(|| GlassError::Backend("output has no size".into()))?;
    let output_size = (w as u32, h as u32);
    // Bind the virtual pointer to the output so motion_absolute maps to it.
    let pointer =
        vp_manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());
    let keyboard = vk_manager.create_virtual_keyboard(&seat, &qh, ());

    // The sway IPC socket appears in the private runtime dir alongside the wayland
    // socket; retry briefly in case it lands a moment later.
    let ipc = loop {
        match Ipc::connect(runtime_dir) {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(40)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(e) => return Err(e),
        }
    };

    Ok((
        conn,
        queue,
        state,
        manager,
        output,
        pointer,
        keyboard,
        ipc,
        output_size,
    ))
}

/// Start a reader over one of sway's streams, or reap the session and say why it could not.
///
/// The failed start consumed the pipe, so sway's next write to that stream takes `SIGPIPE`.
///
/// The group rather than the leader, matching every other error path here: sway forks Xwayland
/// into its own group, and a leader-only reap leaves it holding the X display in the global
/// namespace. It does not reach the apps sway `exec`s — those are `setsid`ed out (see
/// `kill_session`) — but sway has not read its config yet, so there are none.
fn tap_or_reap<R: std::io::Read + AsFd + Send + 'static>(
    stream: R,
    tag: Stream,
    name: &str,
    logs: &LogSink,
    child: &mut Child,
) -> Result<LineTap> {
    LineTap::start(stream, tag, name, logs.clone()).map_err(|e| {
        glass_proc_linux::reap_group(child, glass_proc_linux::REAP_GRACE);
        GlassError::AppNotStarted(format!(
            "started sway but could not read its output ({e}); the session was stopped rather \
             than left to write into a pipe nobody drains — free up threads and file descriptors \
             on the host"
        ))
    })
}

/// Spawn one per-session sway+Xwayland, connect, and discover the app's first
/// window — the full compositor bring-up for `start_app`, factored out so it can
/// be retried. On any failure the spawned compositor's process group is reaped, so
/// a caller that retries never leaves an orphaned (or display-colliding) sway or
/// Xwayland behind. `spec`'s build step is the caller's responsibility (it must run
/// once, not per attempt).
fn bring_up_session(
    sway: &Path,
    logs: &LogSink,
    spec: &AppSpec,
    a11y: Option<glass_core::A11yBind>,
    protected_host_paths: &[ProtectedHostPath],
) -> Result<(ActiveSession, WindowGeometry)> {
    let runtime_dir = tempfile::Builder::new()
        .prefix("glass-wl.")
        .tempdir()
        .map_err(GlassError::Io)?;

    let status_pipe = match spec.sandbox {
        SandboxLevel::Off => None,
        SandboxLevel::Default | SandboxLevel::Strict => {
            Some(BwrapStatusPipe::new().map_err(|error| {
                GlassError::SandboxUnavailable(format!(
                    "could not create Bubblewrap status pipe: {error}"
                ))
            })?)
        }
    };
    let status_fd = status_pipe.as_ref().map(BwrapStatusPipe::writer_fd);
    let config = runtime_dir.path().join("sway.cfg");
    std::fs::write(
        &config,
        sway_config(
            spec,
            runtime_dir.path(),
            a11y.map(|a| a.dir),
            status_fd,
            if spec.sandbox == SandboxLevel::Off {
                &[]
            } else {
                protected_host_paths
            },
        )?,
    )
    .map_err(GlassError::Io)?;
    let mut cmd = build_sway_command_with_status(
        sway,
        &config,
        spec,
        runtime_dir.path(),
        a11y.map(|a| a.addr),
        status_fd,
    )?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| GlassError::AppNotStarted(format!("spawn sway: {e}")))?;
    let direct_root = (spec.sandbox == SandboxLevel::Off).then_some(child.id());
    let mut pending = PendingWaylandSession {
        child,
        status: status_pipe.map(BwrapStatusPipe::into_reader),
        ownership_root: direct_root,
    };
    // Declared before the discovery loop below, so each of its `return Err(...)` paths drops the
    // taps *after* its own reap — the order teardown uses, so the final drain sees what sway wrote
    // on the way out. `Stdio::piped()` guarantees both are `Some`; skipping one that is not would
    // silently stop capturing it.
    let stdout = pending.child.stdout.take().expect("stdout was piped");
    let stderr = pending.child.stderr.take().expect("stderr was piped");
    let stdout_tap = match tap_or_reap(
        stdout,
        Stream::Stdout,
        "glass-sway-stdout",
        logs,
        &mut pending.child,
    ) {
        Ok(tap) => tap,
        Err(error) => {
            reap_pending(&mut pending);
            return Err(error);
        }
    };
    let stderr_tap = match tap_or_reap(
        stderr,
        Stream::Stderr,
        "glass-sway-stderr",
        logs,
        &mut pending.child,
    ) {
        Ok(tap) => tap,
        Err(error) => {
            reap_pending(&mut pending);
            return Err(error);
        }
    };
    let taps = vec![stdout_tap, stderr_tap];

    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.max(1));
    let socket = loop {
        if Instant::now() >= deadline {
            return Err(launch_deadline_error(&mut pending, spec.timeout_ms, 0));
        }
        if let Err(error) = pending.poll_status() {
            reap_pending(&mut pending);
            return Err(error);
        }
        if let Some(s) = find_wayland_socket(runtime_dir.path())
            && Instant::now() < deadline
        {
            break s;
        }
        if let Ok(Some(status)) = pending.child.try_wait() {
            // sway exited — but on an *unclean* exit Xwayland, which sway forks into its own
            // group, can outlive it and hold the X display in the global namespace, breaking the
            // next session. (The app sway `exec`s is `setsid`ed out of that group — see
            // `kill_session` — and is covered by the `reap_launch` tree walk there.)
            reap_pending(&mut pending);
            return Err(GlassError::app_exited_during_discovery(
                status.code(),
                spec.sandbox,
            ));
        }
        std::thread::sleep(
            Duration::from_millis(40).min(deadline.saturating_duration_since(Instant::now())),
        );
    };

    if Instant::now() >= deadline {
        return Err(launch_deadline_error(&mut pending, spec.timeout_ms, 0));
    }
    let (conn, mut queue, mut state, manager, output, pointer, keyboard, mut ipc, output_size) =
        match open_session(&socket, runtime_dir.path(), deadline) {
            Ok(v) => v,
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(launch_deadline_error(&mut pending, spec.timeout_ms, 0));
                }
                reap_pending(&mut pending);
                return Err(e);
            }
        };
    if Instant::now() >= deadline {
        return Err(launch_deadline_error(&mut pending, spec.timeout_ms, 0));
    }
    let socket_path = socket;

    // Discover the initially-focused window (the app's first toplevel), so
    // capture/input have an active target before the first list_windows.
    let mut ids: HashMap<String, WindowId> = HashMap::new();
    let mut next_id = 0u64;
    let mut recovery = crate::xwayland::Recovery::new(runtime_dir.path());
    let (active, active_rect) = {
        // An X11 app's only window can reach Xwayland's X server and never reach the compositor
        // (see `crate::xwayland`), and no amount of further waiting brings it — so once the app
        // has had a fair chance to show a window, stop only waiting and go look on the X side.
        //
        // Half the launch budget, because here the two states are hardest to tell apart: a window
        // mapped in X and not yet in the compositor's tree is the normal state mid-handshake, and
        // a slow toolkit under load can sit there for a while. Waiting out half the budget makes
        // an arriving window very unlikely to be mistaken for a lost one, and still leaves the
        // other half to notice, re-map, and see the window appear.
        let start_grace = Instant::now() + start_recovery_after(spec.timeout_ms);
        let mut discovered = None;
        loop {
            if Instant::now() >= deadline {
                let unrecovered = recovery.unrecovered();
                return Err(launch_deadline_error(
                    &mut pending,
                    spec.timeout_ms,
                    unrecovered,
                ));
            }
            if let Err(error) = pending.poll_status() {
                reap_pending(&mut pending);
                return Err(error);
            }
            // A slice per pass, not the launch's whole budget: this loop is also watching for
            // sway exiting and for a window Xwayland lost. The error is dropped because a slice
            // this short is missed by a compositor that is merely loaded.
            let _ = roundtrip_until(
                &conn,
                &mut queue,
                &mut state,
                deadline.min(Instant::now() + COMPOSITOR_SERVICE_SLICE),
                "launch",
            );
            // Distinguish "sway says no windows" from "sway did not answer": an unanswered
            // request is not evidence the app has no windows, and feeding that emptiness to the
            // cross-check below would make every window the app really has look lost.
            let listed = ipc.windows();
            let wins = listed.as_deref().unwrap_or_default();
            if discovered.is_none()
                && let Some(w) = wins.iter().find(|w| w.focused).or_else(|| wins.first())
            {
                mint_id(&mut ids, &mut next_id, &w.identifier);
                discovered = Some((Some(w.identifier.clone()), rect_to_geom(&w.rect)));
            }
            if launch_ready(
                pending.status_confirmed(),
                discovered.is_some(),
                deadline,
                Instant::now(),
            ) && let Some(window) = discovered.take()
            {
                break window;
            }
            let now = Instant::now();
            if now >= start_grace && listed.is_ok() {
                recovery.recover_if_due(now, &x11_ids(wins));
            }
            if let Ok(Some(status)) = pending.child.try_wait() {
                // Reap the whole group (see the socket-wait loop above): an
                // unclean sway exit can orphan Xwayland + the app otherwise.
                reap_pending(&mut pending);
                return Err(GlassError::app_exited_during_discovery(
                    status.code(),
                    spec.sandbox,
                ));
            }
            std::thread::sleep(
                Duration::from_millis(40).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    };
    // The caller's first enumeration must cross-check rather than fall inside the interval this
    // discovery loop already spent.
    recovery.rearm();
    let geometry = active_rect.clone();
    let Some(ownership_root) = pending.ownership_root else {
        reap_pending(&mut pending);
        return Err(GlassError::SandboxUnavailable(
            "Bubblewrap status channel closed without a contained child PID".into(),
        ));
    };
    let session = ActiveSession {
        child: pending.child,
        ownership_root,
        taps,
        _runtime_dir: runtime_dir,
        socket_path,
        conn,
        queue,
        state,
        manager,
        output,
        pointer,
        keyboard,
        ipc,
        output_size,
        ids,
        next_id,
        recovery,
        active,
        active_rect,
        geometry: geometry.clone(),
        time: 0,
        input_poison: None,
    };
    Ok((session, geometry))
}

/// What a gesture has pressed and not yet put back.
///
/// A press is flushed before the wait it may fail in, so the compositor still acts on it, while
/// the gesture unwinds through `?` past every step that would have released it — leaving a button
/// or modifier down for the next tool call to inherit.
///
/// Per gesture, not per settle: the settle that fails is rarely the one that pressed anything (a
/// drag spends its time in `move_to`).
#[derive(Default)]
struct Held {
    button: Option<u32>,
    key: Option<u32>,
    modifiers: bool,
}

impl Held {
    /// Put the seat back without waiting for the compositor, but require the release requests to
    /// reach the Wayland transport.
    fn release(&mut self, s: &mut ActiveSession) -> Result<()> {
        if self.button.is_none() && self.key.is_none() && !self.modifiers {
            return Ok(());
        }
        s.time = s.time.wrapping_add(1);
        let t = s.time;
        if let Some(b) = self.button.take() {
            s.pointer.button(t, b, ButtonState::Released);
            s.pointer.frame();
        }
        if let Some(kc) = self.key.take() {
            s.keyboard.key(t, kc, 0);
        }
        if std::mem::take(&mut self.modifiers) {
            s.keyboard.modifiers(0, 0, 0, 0);
        }
        s.conn
            .flush()
            .map_err(|e| GlassError::Backend(format!("input cleanup flush: {e}")))
    }
}

fn attach_cleanup_failure(primary: GlassError, cleanup: GlassError) -> GlassError {
    GlassError::input_cleanup_failed("releasing Wayland input", primary, cleanup)
}

fn finish_input_cleanup<T>(
    poison: &mut Option<String>,
    primary: Result<T>,
    cleanup: Result<()>,
) -> Result<T> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => {
            poison.get_or_insert_with(|| cleanup.to_string());
            Err(attach_cleanup_failure(primary, cleanup))
        }
        (Ok(_), Err(cleanup)) => {
            poison.get_or_insert_with(|| cleanup.to_string());
            Err(cleanup)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn record_release_failure(poison: &mut Option<String>, result: &Result<()>) {
    if let Err(error) = result {
        poison.get_or_insert_with(|| error.to_string());
    }
}

fn record_after_release(down: bool, poison: &mut Option<String>, result: &Result<()>) {
    match down {
        true => {}
        false => record_release_failure(poison, result),
    }
}

fn confirm_release_settled<T>(held: &mut Option<T>, settled: Result<()>) -> Result<()> {
    settled?;
    *held = None;
    Ok(())
}

fn settle_tap_state<T>(held: &mut Option<T>, state: u32, settled: Result<()>) -> Result<()> {
    match state {
        0 => confirm_release_settled(held, settled),
        _ => settled,
    }
}

fn tap_held_key(state: u32, keycode: u32) -> Option<u32> {
    match state {
        1 => Some(keycode),
        _ => None,
    }
}

fn require_healthy_input(input_poison: Option<&str>) -> Result<()> {
    match input_poison {
        Some(cause) => Err(GlassError::Backend(format!(
            "input state is uncertain after a release cleanup failure ({cause}); restart the session"
        ))),
        None => Ok(()),
    }
}

fn keymap_wire_len(keymap: &str) -> Result<u32> {
    let text_len = u32::try_from(keymap.len()).map_err(|_| {
        GlassError::Backend("Wayland keymap exceeds the protocol size limit".into())
    })?;
    text_len
        .checked_add(1)
        .ok_or_else(|| GlassError::Backend("Wayland keymap exceeds the protocol size limit".into()))
}

fn sync_session_by(s: &mut ActiveSession, who: &str, deadline: Deadline) -> Result<()> {
    roundtrip_by(&s.conn, &mut s.queue, &mut s.state, deadline, who)
}

/// Write the keymap to an unlinked temp file and hand its fd to the compositor,
/// then settle so Xwayland adopts the new mapping before any key events. No
/// unsafe: tempfile gives a normal, mmap-able fd; XKB_V1 format == 1.
fn upload_keymap_by(
    s: &mut ActiveSession,
    kb: &ZwpVirtualKeyboardV1,
    keymap: &str,
    deadline: Deadline,
    dispatch: &WaylandDispatch,
) -> Result<()> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("keymap upload"));
    }
    let wire_len = keymap_wire_len(keymap)?;
    let mut f = tempfile::tempfile().map_err(GlassError::Io)?;
    f.write_all(keymap.as_bytes()).map_err(GlassError::Io)?;
    f.write_all(&[0]).map_err(GlassError::Io)?; // keymap string is NUL-terminated
    f.flush().map_err(GlassError::Io)?;
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("keymap upload"));
    }
    kb.keymap(1, f.as_fd(), wire_len);
    dispatch.mark();
    sync_session_by(s, "keymap upload", deadline)?;
    input_settle_by(deadline)
}

/// Press then release evdev keycode `kc`, bumping the session clock per event and
/// self-committing (roundtrip + settle) after each — so the compositor processes the
/// press/release individually, like the chord sink. A heavy client (e.g. a browser) ignores
/// taps that are merely queued and flushed once at the end.
fn tap_by(
    s: &mut ActiveSession,
    kb: &ZwpVirtualKeyboardV1,
    kc: u32,
    deadline: Deadline,
    dispatch: &WaylandDispatch,
) -> Result<()> {
    let mut held = Held::default();
    for state in [1u32, 0] {
        if deadline.has_passed() {
            let cleanup = held.release(s);
            return finish_input_cleanup(
                &mut s.input_poison,
                Err(GlassError::deadline_not_started("key tap")),
                cleanup,
            );
        }
        s.time = s.time.wrapping_add(1);
        kb.key(s.time, kc, state);
        dispatch.mark();
        held.key = tap_held_key(state, kc);
        if let Err(e) = sync_session_by(s, "key tap", deadline) {
            let cleanup = held.release(s);
            return finish_input_cleanup(&mut s.input_poison, Err(e), cleanup);
        }
        let settled = input_settle_by(deadline);
        let settled = settle_tap_state(&mut held.key, state, settled);
        if let Err(error) = settled {
            let cleanup = held.release(s);
            return finish_input_cleanup(&mut s.input_poison, Err(error), cleanup);
        }
    }
    Ok(())
}

struct WaylandTypeSink<'a> {
    s: &'a mut ActiveSession,
    kb: &'a ZwpVirtualKeyboardV1,
    taps: std::vec::IntoIter<u32>,
    deadline: Deadline,
    dispatch: &'a WaylandDispatch,
}

impl glass_core::TypeSink for WaylandTypeSink<'_> {
    fn character(&mut self, _character: char) -> Result<()> {
        let keycode = self.taps.next().ok_or_else(|| {
            GlassError::Backend("typing plan ended before the requested text".into())
        })?;
        tap_by(self.s, self.kb, keycode, self.deadline, self.dispatch)
    }
}

/// Fail closed: a launch that asked for a sandbox errors rather than running unconfined.
///
/// `probe` is a thunk, not a value: Rust evaluates arguments before the call, so passing
/// `availability()` would fork `bwrap --unshare-user` even for `sandbox:"off"` — the setting that
/// exists for hosts where bubblewrap does not work. glass-x11 keeps its own copy so both stay in
/// the mutation gate's `--package` list.
fn ensure_sandbox_available(
    level: glass_core::SandboxLevel,
    probe: impl FnOnce() -> glass_sandbox_linux::Availability,
) -> Result<()> {
    if level == glass_core::SandboxLevel::Off {
        return Ok(());
    }
    match probe() {
        glass_sandbox_linux::Availability::Ok => Ok(()),
        // The fix travels with the cause: only the probe knows whether bubblewrap is missing,
        // refusing, or wedged. Telling a user to install one that is installed sends them past the
        // mount holding it.
        glass_sandbox_linux::Availability::Unavailable(why) => Err(GlassError::SandboxUnavailable(
            format!("{why}. See `glass-mcp doctor`."),
        )),
    }
}

/// XKB real-modifier mask for a chord's modifiers (standard `include "complete"`
/// order: Shift, Lock, Control, Mod1=Alt, ..., Mod4=Super).
///
/// Shift-free on purpose: `1 << 0` and `1 >> 0` are the same number, so Shift's bit as a shift
/// was a place the code could change with nothing able to notice.
fn modifier_mask(mods: &[glass_core::keys::Modifier]) -> u32 {
    use glass_core::keys::Modifier;
    mods.iter().fold(0, |m, x| {
        m | match x {
            Modifier::Shift => 0b1,
            Modifier::Control => 0b100,
            Modifier::Alt => 0b1000,
            Modifier::Super => 0b100_0000,
        }
    })
}

/// Lets `glass_core::run_drag` drive a Wayland drag through the virtual-pointer
/// protocol. Each method self-commits (`frame` + roundtrip + 8ms settle) and
/// advances the event clock so timestamps stay monotonic across the drag.
struct WaylandDragSink<'a> {
    s: &'a mut ActiveSession,
    dispatch: &'a WaylandDispatch,
    w: u32,
    h: u32,
    ox: i32,
    oy: i32,
    b: u32,
    mask: u32,
    held: Held,
    deadline: Deadline,
}

/// A drag that ends early — `run_drag` propagates a failed settle from any of its waypoints —
/// otherwise leaves the button down.
impl Drop for WaylandDragSink<'_> {
    fn drop(&mut self) {
        let cleanup = self.held.release(self.s);
        if let Err(error) = finish_input_cleanup(&mut self.s.input_poison, Ok(()), cleanup) {
            eprintln!("glass-wayland: drag cleanup failed: {error}");
        }
    }
}

impl WaylandDragSink<'_> {
    fn tick(&mut self) -> u32 {
        self.s.time = self.s.time.wrapping_add(1);
        self.s.time
    }
    fn ax(&self, x: i32) -> u32 {
        (self.ox + x).max(0) as u32
    }
    fn ay(&self, y: i32) -> u32 {
        (self.oy + y).max(0) as u32
    }
    fn settle(&mut self) -> Result<()> {
        sync_session_by(self.s, "input settle", self.deadline)?;
        input_settle_by(self.deadline)
    }
}

impl glass_core::DragSink for WaylandDragSink<'_> {
    fn place(&mut self, x: i32, y: i32) -> Result<()> {
        let vp = self.s.pointer.clone();
        let (w, h) = (self.w, self.h);
        let (axx, ayy) = (self.ax(x), self.ay(y));
        let t = self.tick();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
        self.dispatch.mark();
        self.settle()?;
        let t = self.tick();
        vp.motion_absolute(t, nudge_x(axx, w), ayy, w, h);
        vp.frame();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
        self.dispatch.mark();
        self.settle()
    }
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let vp = self.s.pointer.clone();
        let (w, h) = (self.w, self.h);
        let (axx, ayy) = (self.ax(x), self.ay(y));
        let t = self.tick();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
        self.settle()
    }
    fn button(&mut self, down: bool) -> Result<()> {
        let vp = self.s.pointer.clone();
        let t = self.tick();
        let state = if down {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        vp.button(t, self.b, state);
        vp.frame();
        self.dispatch.mark();
        self.held.button = down.then_some(self.b);
        let result = self.settle();
        record_after_release(down, &mut self.s.input_poison, &result);
        result
    }
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if self.mask == 0 {
            return Ok(());
        }
        let kb = self.s.keyboard.clone();
        if down {
            upload_keymap_by(
                &mut *self.s,
                &kb,
                &crate::keyboard::build_keymap(&[]),
                self.deadline,
                self.dispatch,
            )?;
            kb.modifiers(self.mask, 0, 0, 0);
            self.dispatch.mark();
        } else {
            kb.modifiers(0, 0, 0, 0);
            self.dispatch.mark();
        }
        self.held.modifiers = down;
        // Self-commit so the modifier change reaches the compositor before the
        // press/release that follows it (matches the X11 sink's flush-per-call).
        let result = self.settle();
        record_after_release(down, &mut self.s.input_poison, &result);
        result
    }
}

/// Lets `glass_core::run_chord` drive a Wayland key chord through the virtual keyboard. The keymap
/// (with the chord's key as keycode 1) is uploaded and the modifier mask set in `modifiers(true)`;
/// each method self-commits (roundtrip + 8ms settle) so the modifier is held across the key's frame.
struct WaylandChordSink<'a> {
    s: &'a mut ActiveSession,
    dispatch: &'a WaylandDispatch,
    mask: u32,
    keysym: u32,
    held: Held,
    deadline: Deadline,
}

/// A chord that ends early otherwise leaves its key or its modifier down.
impl Drop for WaylandChordSink<'_> {
    fn drop(&mut self) {
        let cleanup = self.held.release(self.s);
        if let Err(error) = finish_input_cleanup(&mut self.s.input_poison, Ok(()), cleanup) {
            eprintln!("glass-wayland: chord cleanup failed: {error}");
        }
    }
}

impl WaylandChordSink<'_> {
    fn settle(&mut self) -> Result<()> {
        sync_session_by(self.s, "input settle", self.deadline)?;
        input_settle_by(self.deadline)
    }
}

fn chord_holds_modifiers(down: bool, mask: u32) -> bool {
    down && mask != 0
}

impl glass_core::ChordSink for WaylandChordSink<'_> {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        let kb = self.s.keyboard.clone();
        if down {
            // Upload the keymap (chord key = keycode 1) regardless of mask, then set the modifiers.
            upload_keymap_by(
                &mut *self.s,
                &kb,
                &crate::keyboard::build_keymap(&[self.keysym]),
                self.deadline,
                self.dispatch,
            )?;
            if self.mask != 0 {
                kb.modifiers(self.mask, 0, 0, 0);
                self.dispatch.mark();
            }
        } else if self.mask != 0 {
            kb.modifiers(0, 0, 0, 0);
            self.dispatch.mark();
        }
        self.held.modifiers = chord_holds_modifiers(down, self.mask);
        let result = self.settle();
        record_after_release(down, &mut self.s.input_poison, &result);
        result
    }
    fn key(&mut self, down: bool) -> Result<()> {
        let kb = self.s.keyboard.clone();
        self.s.time = self.s.time.wrapping_add(1);
        let t = self.s.time;
        kb.key(t, 1, u32::from(down)); // keycode 1 = the chord's key; 1=pressed, 0=released
        self.dispatch.mark();
        self.held.key = down.then_some(1);
        let result = self.settle();
        record_after_release(down, &mut self.s.input_poison, &result);
        result
    }
}

/// Lets `glass_core::run_scroll` drive a Wayland scroll through the virtual pointer + keyboard. The
/// modifier mask is set in `modifiers(true)` and cleared in `modifiers(false)`; `wheel` positions the
/// pointer (with the focus-reassert nudge, like the drag sink) then emits the vertical and horizontal
/// axis. Each method self-commits (frame + roundtrip + 8ms settle) so the modifier is held across the
/// wheel's frame.
struct WaylandScrollSink<'a> {
    s: &'a mut ActiveSession,
    dispatch: &'a WaylandDispatch,
    w: u32,
    h: u32,
    ox: i32,
    oy: i32,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    mask: u32,
    held: Held,
    deadline: Deadline,
}

/// A scroll that ends early — `wheel` settles three times after the modifier goes down —
/// otherwise leaves that modifier down.
impl Drop for WaylandScrollSink<'_> {
    fn drop(&mut self) {
        let cleanup = self.held.release(self.s);
        if let Err(error) = finish_input_cleanup(&mut self.s.input_poison, Ok(()), cleanup) {
            eprintln!("glass-wayland: scroll cleanup failed: {error}");
        }
    }
}

impl WaylandScrollSink<'_> {
    fn tick(&mut self) -> u32 {
        self.s.time = self.s.time.wrapping_add(1);
        self.s.time
    }
    fn ax(&self, x: i32) -> u32 {
        (self.ox + x).max(0) as u32
    }
    fn ay(&self, y: i32) -> u32 {
        (self.oy + y).max(0) as u32
    }
    fn settle(&mut self) -> Result<()> {
        sync_session_by(self.s, "input settle", self.deadline)?;
        input_settle_by(self.deadline)
    }
}

impl glass_core::ScrollSink for WaylandScrollSink<'_> {
    /// No `mask == 0` guard, unlike the drag sink: `glass_core::run_scroll` returns `wheel()`
    /// directly when there are no modifiers, and every `Modifier` is a non-zero bit.
    fn modifiers(&mut self, down: bool) -> Result<()> {
        let kb = self.s.keyboard.clone();
        if down {
            upload_keymap_by(
                &mut *self.s,
                &kb,
                &crate::keyboard::build_keymap(&[]),
                self.deadline,
                self.dispatch,
            )?;
            kb.modifiers(self.mask, 0, 0, 0);
            self.dispatch.mark();
        } else {
            kb.modifiers(0, 0, 0, 0);
            self.dispatch.mark();
        }
        self.held.modifiers = down;
        let result = self.settle();
        record_after_release(down, &mut self.s.input_poison, &result);
        result
    }
    fn wheel(&mut self) -> Result<()> {
        let vp = self.s.pointer.clone();
        let (w, h) = (self.w, self.h);
        let (axx, ayy) = (self.ax(self.x), self.ay(self.y));
        // Position with the focus-reassert nudge (sway re-evaluates pointer focus only on motion).
        let t = self.tick();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
        self.dispatch.mark();
        self.settle()?;
        let t = self.tick();
        vp.motion_absolute(t, nudge_x(axx, w), ayy, w, h);
        vp.frame();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
        self.settle()?;
        // Emit the wheel (vertical then horizontal) at that point.
        if self.dy != 0 {
            let t = self.tick();
            vp.axis_discrete(t, Axis::VerticalScroll, self.dy as f64 * 15.0, self.dy);
            vp.frame();
        }
        if self.dx != 0 {
            let t = self.tick();
            vp.axis_discrete(t, Axis::HorizontalScroll, self.dx as f64 * 15.0, self.dx);
            vp.frame();
        }
        self.settle()
    }
}

impl Platform for WaylandPlatform {
    fn configure_protected_host_paths(
        &mut self,
        paths: &[ProtectedHostPath],
    ) -> Result<HostPathProtectionMode> {
        glass_sandbox_linux::validate_protected_paths(paths)?;
        self.protected_host_paths = paths.to_vec();
        Ok(HostPathProtectionMode::SandboxRules)
    }

    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        glass_sandbox_linux::validate_protected_paths(&self.protected_host_paths)?;
        ensure_sandbox_available(spec.sandbox, glass_sandbox_linux::availability)?;

        // Run the build step (if any) before the compositor starts. The build is
        // sandboxed (bwrap) when sandbox != Off — same semantics as the X11 backend.
        // Runs once: a retried compositor bring-up must not re-run the build.
        glass_sandbox_linux::run_build(spec)?;

        // Bring up the per-session compositor, retrying a transient failure once. A
        // freshly-spawned headless Xwayland occasionally crashes mid-startup ("failed to read
        // Wayland events: Broken pipe") on the GPU-less CI renderer — after the app has
        // already mapped its window — leaving sway alive but the window never stable in its
        // tree. The crash is rare and independent per spawn, so a fresh compositor makes it
        // reliable. Only transient bring-up failures retry (Timeout / Backend); `bring_up`
        // reaps its own process group, so a retry never races a leftover compositor.
        self.dbus = if spec.a11y {
            Some(glass_dbus_linux::PrivateBus::start().map_err(|e| {
                GlassError::AccessibilityUnavailable(format!(
                    "a11y:true was requested but the private a11y bus could not start: {e}"
                ))
            })?)
        } else {
            None
        };

        const ATTEMPTS: u32 = 2;
        let mut last_err = GlassError::Timeout(spec.timeout_ms);
        for attempt in 0..ATTEMPTS {
            let a11y = self.dbus.as_ref().map(|b| glass_core::A11yBind {
                addr: b.session_bus_address(),
                dir: b.runtime_dir(),
            });
            match bring_up_session(
                &self.sway,
                &self.logs,
                spec,
                a11y,
                &self.protected_host_paths,
            ) {
                Ok((session, geometry)) => {
                    self.active = Some(session);
                    return Ok(geometry);
                }
                Err(e @ (GlassError::Timeout(_) | GlassError::Backend(_)))
                    if attempt + 1 < ATTEMPTS =>
                {
                    last_err = e;
                }
                Err(e) => {
                    self.dbus = None; // reap the private bus on a hard failure
                    return Err(e);
                }
            }
        }
        self.dbus = None; // reap the private bus after exhausted retries
        Err(last_err)
    }

    /// Ignores the deadline — the close-then-reap ladder above is asserted against
    /// `TEARDOWN_BUDGET` instead.
    fn stop_app_by(&mut self, _deadline: glass_core::Deadline) -> Result<()> {
        self.kill_session();
        Ok(())
    }

    fn get_clipboard(&mut self) -> Result<String> {
        let socket = self
            .active
            .as_ref()
            .ok_or(GlassError::NoActiveSession)?
            .socket_path
            .clone();
        crate::clipboard::get(&socket)
    }

    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        let socket = self
            .active
            .as_ref()
            .ok_or(GlassError::NoActiveSession)?
            .socket_path
            .clone();
        // Re-use the existing owner if it is still alive; otherwise re-spawn.
        match &self.clipboard_owner {
            Some(owner) if owner.is_alive() => {
                owner.set_text(text);
                Ok(())
            }
            _ => {
                let owner = crate::clipboard::ClipboardOwner::spawn(socket, text.to_string())?;
                self.clipboard_owner = Some(owner);
                Ok(())
            }
        }
    }

    fn capture_frame_by(&mut self, region: Option<&Region>, deadline: Deadline) -> Result<Frame> {
        run_wayland_call_by(deadline, "capture", |dispatch| {
            let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
            session.state.capture = CaptureScratch::default();
            let qh = session.queue.handle();

            // Capture the selected window from the output framebuffer so static, undamaged content
            // is available without a CPU crop or waiting for a fresh toplevel frame.
            let wr = &session.active_rect;
            let (cx, cy, cw, ch) = match region {
                Some(r) => (wr.x + r.x as i32, wr.y + r.y as i32, r.width, r.height),
                None => (wr.x, wr.y, wr.width, wr.height),
            };
            let frame = session.manager.capture_output_region(
                0,
                &session.output,
                cx,
                cy,
                cw as i32,
                ch as i32,
                &qh,
                (),
            );
            dispatch.mark();
            let mut owned = CaptureObjects {
                frame,
                buffer: None,
            };

            let now = Instant::now();
            let (wait_budget, owner) = clamped_budget(deadline, CAPTURE_BUDGET, now);
            let wait_deadline = now + wait_budget;

            // Wait for the v3 buffer list to finish, then prefer a convertible 32-bit format.
            let (format, w, h, stride) = wait_for(
                &session.conn,
                &mut session.queue,
                &mut session.state,
                wait_deadline,
                "screencopy",
                |s| match owner {
                    Whose::Caller => GlassError::caller_deadline_elapsed("capture"),
                    Whose::Callee => GlassError::CaptureFailed(s.capture.no_formats()),
                },
                |s| s.capture.advertised(),
            )?;
            if deadline.has_passed() {
                return Err(GlassError::caller_deadline_elapsed("capture"));
            }

            // Allocate a matching shm buffer and request the copy.
            let mut pool = RawPool::new((stride * h) as usize, &session.state.shm)
                .map_err(|e| GlassError::CaptureFailed(format!("shm pool: {e}")))?;
            let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, (), &qh);
            owned.frame.copy(&buffer);
            owned.buffer = Some(buffer);

            // Phase 2: dispatch until ready/failed. No live test reaches this call site's bound —
            // only the pure `wait_for` tests stand behind a change here.
            wait_for(
                &session.conn,
                &mut session.queue,
                &mut session.state,
                wait_deadline,
                "screencopy",
                |_| match owner {
                    Whose::Caller => GlassError::caller_deadline_elapsed("capture"),
                    Whose::Callee => GlassError::CaptureFailed(
                        "screencopy: no ready event after the copy request".into(),
                    ),
                },
                |s| s.capture.done.take(),
            )?;

            // The captured buffer already matches the requested region, so no CPU crop.
            let rgba = crate::pixels::to_rgba(pool.mmap(), format, w, h, stride)?;
            Frame::new(w, h, rgba)
        })
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
        run_wayland_call_by(deadline, "pointer input", |dispatch| {
            let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
            require_healthy_input(session.input_poison.as_deref())?;
            session.time = session.time.wrapping_add(1);
            let t = session.time;
            // Pointer motion is absolute over the OUTPUT; map window-relative coords
            // to output coords by the active window's rect origin.
            let (w, h) = session.output_size;
            let (ox, oy) = (session.active_rect.x, session.active_rect.y);
            let ax = |x: i32| (ox + x).max(0) as u32;
            let ay = |y: i32| (oy + y).max(0) as u32;
            let vp = session.pointer.clone();
            let kb = session.keyboard.clone();
            // Flush pending requests and let the compositor + Xwayland process pointer
            // motion (enter/position) before the next event lands.
            let settle = |s: &mut ActiveSession| -> Result<()> {
                sync_session_by(s, "input settle", deadline)?;
                input_settle_by(deadline)
            };
            // sway reevaluates pointer focus only on motion, so settle after the initial move and
            // nudge 1px before the first button or axis event.
            let position = |s: &mut ActiveSession, x: i32, y: i32| -> Result<()> {
                if deadline.has_passed() {
                    return Err(GlassError::deadline_not_started("pointer input"));
                }
                vp.motion_absolute(t, ax(x), ay(y), w, h);
                vp.frame();
                dispatch.mark();
                settle(s)?;
                vp.motion_absolute(t, nudge_x(ax(x), w), ay(y), w, h);
                vp.frame();
                vp.motion_absolute(t, ax(x), ay(y), w, h);
                vp.frame();
                settle(s)
            };
            match *event {
                PointerEvent::Move { x, y } => {
                    position(session, x, y)?;
                }
                PointerEvent::Click {
                    x,
                    y,
                    button,
                    count,
                    ref modifiers,
                } => {
                    position(session, x, y)?;
                    let mask = modifier_mask(modifiers);
                    if mask != 0 {
                        upload_keymap_by(
                            session,
                            &kb,
                            &crate::keyboard::build_keymap(&[]),
                            deadline,
                            dispatch,
                        )?;
                        kb.modifiers(mask, 0, 0, 0);
                        dispatch.mark();
                    }
                    let mut held = Held {
                        modifiers: mask != 0,
                        ..Held::default()
                    };
                    let b = evdev_button(button);
                    let clicks = |session: &mut ActiveSession, held: &mut Held| -> Result<()> {
                        for _ in 0..count {
                            if deadline.has_passed() {
                                return Err(GlassError::deadline_not_started("pointer input"));
                            }
                            vp.button(t, b, ButtonState::Pressed);
                            vp.frame();
                            dispatch.mark();
                            held.button = Some(b);
                            settle(session)?;
                            if deadline.has_passed() {
                                return Err(GlassError::deadline_not_started("pointer input"));
                            }
                            vp.button(t, b, ButtonState::Released);
                            vp.frame();
                            dispatch.mark();
                            confirm_release_settled(&mut held.button, settle(session))?;
                        }
                        Ok(())
                    };
                    let outcome = clicks(session, &mut held);
                    // The same release on both paths: what ends the modifier on a click that worked is
                    // what has to end it on one that did not.
                    let cleanup = held.release(session);
                    finish_input_cleanup(&mut session.input_poison, outcome, cleanup)?;
                }
                PointerEvent::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    button,
                    ref modifiers,
                    duration_ms,
                } => {
                    let gesture =
                        glass_core::DragGesture::plan((from_x, from_y), (to_x, to_y), duration_ms);
                    let mut sink = WaylandDragSink {
                        s: &mut *session,
                        dispatch,
                        w,
                        h,
                        ox,
                        oy,
                        b: evdev_button(button),
                        mask: modifier_mask(modifiers),
                        held: Held::default(),
                        deadline,
                    };
                    glass_core::run_drag_by(&mut sink, &gesture, deadline)?;
                }
                PointerEvent::Scroll {
                    x,
                    y,
                    dx,
                    dy,
                    ref modifiers,
                } => {
                    // Hold modifiers across the wheel frame; see `glass_core::run_scroll`.
                    let mut sink = WaylandScrollSink {
                        s: &mut *session,
                        dispatch,
                        w,
                        h,
                        ox,
                        oy,
                        x,
                        y,
                        dx,
                        dy,
                        mask: modifier_mask(modifiers),
                        held: Held::default(),
                        deadline,
                    };
                    glass_core::run_scroll_by(&mut sink, !modifiers.is_empty(), deadline)?;
                }
                PointerEvent::Gesture { .. } => {
                    return Err(crate::unsupported_multi_touch());
                }
            }
            session
                .conn
                .flush()
                .map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
            Ok(())
        })
    }
    fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        run_wayland_call_by(deadline, "key input", |dispatch| {
            use glass_core::keys::parse_chord;
            let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
            require_healthy_input(session.input_poison.as_deref())?;
            let kb = session.keyboard.clone();
            match event {
                KeyEvent::Text(text) => {
                    // Keep each chunk's keymap fixed so Xwayland clients cannot race a per-character
                    // keymap swap; each tap self-commits to pace heavy clients.
                    let mut characters = text.chars();
                    for chunk in crate::keyboard::plan_type(text) {
                        upload_keymap_by(
                            &mut *session,
                            &kb,
                            &crate::keyboard::build_keymap(&chunk.keysyms),
                            deadline,
                            dispatch,
                        )?;
                        let chunk_text: String =
                            characters.by_ref().take(chunk.taps.len()).collect();
                        let mut sink = WaylandTypeSink {
                            s: &mut *session,
                            kb: &kb,
                            taps: chunk.taps.into_iter(),
                            deadline,
                            dispatch,
                        };
                        run_wayland_type_by(&mut sink, &chunk_text, Duration::ZERO, deadline)?;
                    }
                }
                KeyEvent::Chord(c) => {
                    let (mods, keysym) = parse_chord(c)?; // validates before any traffic
                    let mut sink = WaylandChordSink {
                        s: &mut *session,
                        dispatch,
                        mask: modifier_mask(&mods),
                        keysym,
                        held: Held::default(),
                        deadline,
                    };
                    glass_core::run_chord_by(&mut sink, deadline)?;
                }
            }
            session
                .conn
                .flush()
                .map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
            Ok(())
        })
    }

    fn window_by(&mut self, op: &WindowOp, deadline: Deadline) -> Result<WindowGeometry> {
        run_wayland_call_by(deadline, "Wayland window operation", |dispatch| {
            let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
            let ident = session.active.clone().ok_or(GlassError::WindowNotFound)?;
            dispatch.mark();
            // Window operations target the active floating sway container.
            let con = session
                .ipc
                .windows_by(deadline)?
                .into_iter()
                .find(|w| w.identifier == ident)
                .map(|w| w.con_id)
                .ok_or(GlassError::WindowNotFound)?;
            match *op {
                WindowOp::Geometry => {}
                WindowOp::Focus => session
                    .ipc
                    .run_command_by(&format!("[con_id={con}] focus"), deadline)?,
                WindowOp::Resize { width, height } => session.ipc.run_command_by(
                    &format!("[con_id={con}] resize set width {width} px height {height} px"),
                    deadline,
                )?,
                // Move uses output-absolute coordinates, matching X11 root coordinates.
                WindowOp::Move { x, y } => session.ipc.run_command_by(
                    &format!("[con_id={con}] move absolute position {x} {y}"),
                    deadline,
                )?,
            }
            // Refresh geometry after sway clamps the operation.
            let now = session
                .ipc
                .windows_by(deadline)?
                .into_iter()
                .find(|w| w.identifier == ident)
                .ok_or(GlassError::WindowNotFound)?;
            let geo = rect_to_geom(&now.rect);
            session.active_rect = geo.clone();
            session.geometry = geo.clone();
            Ok(geo)
        })
    }

    fn list_windows_by(&mut self, deadline: Deadline) -> Result<Vec<WindowInfo>> {
        run_wayland_call_by(deadline, "Wayland window list", |dispatch| {
            let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
            dispatch.mark();
            // Refresh foreign-toplevel handles so capture can later find them.
            sync_session_by(session, "window list", deadline)?;
            let mut wins: Vec<SwayWindow> = session.ipc.windows_by(deadline)?;
            // Enumeration exposes Xwayland views missing from sway, so repair them before return.
            let recovered =
                session
                    .recovery
                    .recover_if_due_by(Instant::now(), &x11_ids(&wins), deadline)?;
            if recovery_needs_settle(recovered) {
                let settle = deadline
                    .remaining()
                    .map(|left| left.min(crate::xwayland::REMAP_SETTLE))
                    .unwrap_or(crate::xwayland::REMAP_SETTLE);
                std::thread::sleep(settle);
                if deadline.has_passed() {
                    return Err(GlassError::caller_deadline_elapsed(
                        "Wayland window list remap settle",
                    ));
                }
                wins = session.ipc.windows_by(deadline)?;
            }
            let mut out = Vec::with_capacity(wins.len());
            for w in &wins {
                let id = mint_id(&mut session.ids, &mut session.next_id, &w.identifier);
                out.push(WindowInfo {
                    id,
                    title: w.title.clone(),
                    class: w.class.clone(),
                    geometry: rect_to_geom(&w.rect),
                    active: w.focused,
                });
            }
            Ok(out)
        })
    }

    fn select_window_by(&mut self, id: WindowId, deadline: Deadline) -> Result<WindowGeometry> {
        run_wayland_call_by(deadline, "Wayland window selection", |dispatch| {
            let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
            dispatch.mark();
            let wins = session.ipc.windows_by(deadline)?;
            let target = wins
                .into_iter()
                .find(|w| session.ids.get(&w.identifier) == Some(&id))
                .ok_or(GlassError::WindowNotFound)?;
            session
                .ipc
                .run_command_by(&format!("[con_id={}] focus", target.con_id), deadline)?;
            // Confirm the focus moved (no silent fallback).
            let after = session.ipc.windows_by(deadline)?;
            let now = after
                .iter()
                .find(|w| w.identifier == target.identifier)
                .ok_or(GlassError::WindowNotFound)?;
            if !now.focused {
                return Err(GlassError::Backend("window did not take focus".into()));
            }
            let geo = rect_to_geom(&now.rect);
            session.active = Some(target.identifier);
            session.active_rect = geo.clone();
            session.geometry = geo.clone();
            Ok(geo)
        })
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        std::mem::take(&mut *self.logs.lock().expect("log buffer mutex"))
    }

    /// The app's process subtree. The child we spawn is **sway**, which launches
    /// the app as an `exec` descendant (under a shell, and `bwrap` when
    /// sandboxed), so the real app has a different pid. The a11y reader
    /// correlates the AT-SPI connection pid against this set, so it must include
    /// the descendants — the inherited single-pid default leaves it empty and the
    /// reader can't tell apps apart. Mirrors the X11 backend's `app_pids()`.
    /// (We intentionally don't override `app_pid()`: there is no single
    /// authoritative app pid here — sway's pid isn't the app's.)
    fn app_pids(&self) -> Vec<u32> {
        match &self.active {
            Some(s) => ProcessIdentitySet::from_host_root(s.ownership_root)
                .matching_pids()
                .to_vec(),
            None => Vec::new(),
        }
    }

    fn a11y_bus_addr(&self) -> Option<String> {
        self.dbus.as_ref().map(|b| b.a11y_bus_address().to_string())
    }
}

#[cfg(test)]
mod pure_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::testw::on_a_thread;
    use glass_core::MouseButton;
    use glass_exec_unix::is_executable_file;

    /// The budget the pure waits below are given. `on_a_thread` allows a multiple of it, so a lost
    /// bound fails the test rather than hanging the suite.
    const PURE_WAIT_BUDGET: Duration = Duration::from_millis(300);

    /// Room for a wait that answers at once to be scheduled on a runner busy with the sway-backed
    /// tests, which assert on a fraction of it.
    const PROMPT_WAIT_BUDGET: Duration = Duration::from_secs(1);

    #[derive(Default)]
    struct RecordingTypeSink {
        characters: Vec<char>,
    }

    impl glass_core::TypeSink for RecordingTypeSink {
        fn character(&mut self, character: char) -> Result<()> {
            self.characters.push(character);
            std::thread::sleep(Duration::from_millis(10));
            Ok(())
        }
    }

    #[test]
    fn spent_input_deadline_dispatches_no_backend_events() {
        let mut recorded_events = Vec::new();
        let deadline = glass_core::Deadline::at(Instant::now() - Duration::from_millis(1));

        let error = run_wayland_call_by(deadline, "pointer input", |_| {
            recorded_events.push("motion");
            Ok(())
        })
        .expect_err("spent input must be rejected before dispatch");

        assert!(recorded_events.is_empty());
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn pre_dispatch_caller_deadline_error_stays_not_dispatched() {
        let error = run_wayland_call_by(Deadline::UNBOUNDED, "key input", |_| {
            Err::<(), _>(GlassError::deadline_not_started("keymap upload"))
        })
        .expect_err("the keymap did not reach the compositor");

        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn invalid_pointer_work_is_rejected_before_session_lookup() {
        let mut platform = WaylandPlatform {
            sway: PathBuf::new(),
            logs: Arc::new(Mutex::new(Vec::new())),
            active: None,
            clipboard_owner: None,
            dbus: None,
            protected_host_paths: Vec::new(),
        };
        let events = [
            PointerEvent::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count: 0,
                modifiers: vec![],
            },
            PointerEvent::Click {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                count: u32::MAX,
                modifiers: vec![],
            },
            PointerEvent::Scroll {
                x: 0,
                y: 0,
                dx: i32::MIN,
                dy: i32::MAX,
                modifiers: vec![],
            },
        ];
        for event in events {
            let error = platform
                .send_pointer_by(&event, Deadline::UNBOUNDED)
                .expect_err("invalid pointer work must fail before backend/session work");

            assert!(matches!(error.cause(), GlassError::InvalidPointerInput(_)));
            assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        }
    }

    #[test]
    fn failed_cleanup_flush_preserves_primary_provenance_and_poisons_input() {
        let mut poison = None;
        let primary = Err::<(), _>(GlassError::caller_deadline_elapsed("pointer input"));
        let cleanup = Err(GlassError::Backend("cleanup flush failed".into()));

        let error = finish_input_cleanup(&mut poison, primary, cleanup).unwrap_err();

        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("cleanup flush failed"));
        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("Wayland cleanup failure must remain structured");
        };
        assert_eq!(operation, "releasing Wayland input");
        assert!(matches!(*primary, GlassError::Bounded { .. }));
        assert!(
            matches!(*cleanup, GlassError::Backend(message) if message == "cleanup flush failed")
        );
        assert_eq!(
            poison.as_deref(),
            Some("backend error: cleanup flush failed")
        );
    }

    #[test]
    fn cleanup_failure_survives_outer_dispatch_upgrade() {
        let mut poison = None;
        let primary = Err::<(), _>(GlassError::deadline_not_started("pointer input"));
        let cleanup = Err(GlassError::Backend("cleanup flush failed".into()));
        let error = finish_input_cleanup(&mut poison, primary, cleanup).unwrap_err();
        let dispatch = WaylandDispatch::default();
        dispatch.mark();

        let error = dispatch.classify("pointer input", error);

        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("cleanup flush failed"));
        assert!(matches!(error, GlassError::InputCleanupFailed { .. }));
    }

    #[test]
    fn held_input_is_cleared_only_after_release_settle_succeeds() {
        let mut held = Some(272);
        let error = confirm_release_settled(
            &mut held,
            Err(GlassError::Backend("release settle failed".into())),
        )
        .unwrap_err();
        assert!(error.to_string().contains("release settle failed"));
        assert_eq!(held, Some(272));

        confirm_release_settled(&mut held, Ok(())).unwrap();
        assert_eq!(held, None);
    }

    #[test]
    fn input_deadline_and_health_checks_reject_unusable_input() {
        let error = input_settle_by(Deadline::from_millis(0))
            .expect_err("a spent input settle cannot succeed");
        assert_eq!(error.bound_owner(), Some(Whose::Caller));

        require_healthy_input(None).expect("unpoisoned input is healthy");
        let error = require_healthy_input(Some("release flush failed"))
            .expect_err("poisoned input must remain unusable");
        assert!(error.to_string().contains("release flush failed"));
    }

    #[test]
    fn keymap_and_release_helpers_preserve_protocol_boundaries() {
        assert_eq!(keymap_wire_len("").unwrap(), 1);
        assert_eq!(keymap_wire_len("abc").unwrap(), 4);
        assert_eq!(tap_held_key(1, 272), Some(272));
        assert_eq!(tap_held_key(0, 272), None);

        let mut held = Some(272);
        settle_tap_state(&mut held, 1, Ok(())).expect("press settle");
        assert_eq!(held, Some(272), "a press remains held");
        settle_tap_state(&mut held, 0, Ok(())).expect("release settle");
        assert_eq!(held, None, "a settled release is no longer held");

        let mut held = Some(272);
        let error = settle_tap_state(
            &mut held,
            0,
            Err(GlassError::Backend("release settle failed".into())),
        )
        .expect_err("a failed release settle must remain visible");
        assert!(error.to_string().contains("release settle failed"));
        assert_eq!(held, Some(272), "an unconfirmed release remains held");

        let failed = Err(GlassError::Backend("release settle failed".into()));
        let mut poison = None;
        record_after_release(true, &mut poison, &failed);
        assert_eq!(poison, None, "a failed press is cleaned up by its guard");
        record_after_release(false, &mut poison, &failed);
        assert_eq!(
            poison.as_deref(),
            Some("backend error: release settle failed")
        );
    }

    #[test]
    fn chord_and_recovery_predicates_distinguish_zero_boundaries() {
        assert!(!chord_holds_modifiers(false, 0));
        assert!(!chord_holds_modifiers(false, 4));
        assert!(!chord_holds_modifiers(true, 0));
        assert!(chord_holds_modifiers(true, 4));

        assert!(!recovery_needs_settle(0));
        assert!(recovery_needs_settle(1));
    }

    #[test]
    fn short_typing_deadline_stops_before_all_characters() {
        let requested_text = "abcd";
        let mut sink = RecordingTypeSink::default();

        let error = run_wayland_type_by(
            &mut sink,
            requested_text,
            Duration::ZERO,
            glass_core::Deadline::from_millis(5),
        )
        .expect_err("typing must stop when the shared deadline expires");

        assert!(sink.characters.len() < requested_text.chars().count());
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn capture_returning_after_the_deadline_is_not_success() {
        let capture_error = run_wayland_call_by(
            glass_core::Deadline::from_millis(1),
            "capture",
            |dispatch| {
                dispatch.mark();
                std::thread::sleep(Duration::from_millis(10));
                Ok(())
            },
        )
        .expect_err("a late capture must not return success");

        assert_eq!(capture_error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            capture_error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn caller_deadline_clamps_compositor_wait() {
        let now = Instant::now();
        let caller_remaining = Duration::from_millis(2);

        let (observed_wait, owner) = clamped_budget(
            glass_core::Deadline::at(now + caller_remaining),
            COMPOSITOR_SYNC_BUDGET,
            now,
        );

        assert!(observed_wait <= caller_remaining);
        assert_eq!(owner, glass_core::Whose::Caller);
    }

    #[test]
    fn caller_deadline_clamps_input_settle() {
        let now = Instant::now();
        let caller_remaining = Duration::from_millis(2);

        let (observed_sleep, owner) = clamped_budget(
            glass_core::Deadline::at(now + caller_remaining),
            INPUT_SETTLE,
            now,
        );

        assert!(observed_sleep <= caller_remaining);
        assert_eq!(owner, glass_core::Whose::Caller);
    }

    /// Run [`wait_for`] over a silent socket while `speak` keeps or closes the peer.
    fn wait_over_a_socket(
        budget: Duration,
        speak: impl FnOnce(UnixStream) -> Option<UnixStream>,
    ) -> (std::result::Result<(), GlassError>, Duration) {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let conn = Connection::from_socket(ours).expect("a connection over the socket");
        let mut queue = conn.new_event_queue::<()>();
        let _peer = speak(theirs);
        let started = Instant::now();
        let outcome = wait_for(
            &conn,
            &mut queue,
            &mut (),
            Instant::now() + budget,
            "test",
            |()| GlassError::Backend("nothing arrived".into()),
            |()| None,
        );
        (outcome, started.elapsed())
    }

    /// An open, silent peer: nothing arrives and nothing ends the connection either, which is what
    /// a wedged compositor looks like from this side.
    #[test]
    fn a_wait_gives_up_on_a_peer_that_never_speaks() {
        let (outcome, elapsed) = on_a_thread(
            PURE_WAIT_BUDGET * 20,
            "the wait never came back, so it is not bounded by its deadline",
            || wait_over_a_socket(PURE_WAIT_BUDGET, Some),
        );

        let err = outcome.expect_err("a question nothing answered is a failure");
        assert!(err.to_string().contains("nothing arrived"), "{err}");
        assert!(
            elapsed < PURE_WAIT_BUDGET * 10,
            "the wait outlived its deadline by too much to be bounded by it: {elapsed:?}"
        );
        assert!(
            elapsed >= PURE_WAIT_BUDGET,
            "the deadline was cut short, so a compositor merely slow to answer would be given up \
             on: {elapsed:?}"
        );
    }

    /// The half of the contract silence cannot show: a wait that slept out its deadline instead of
    /// polling the fd would still pass the test above.
    #[test]
    fn a_wait_stops_waiting_as_soon_as_the_peer_speaks() {
        let (outcome, elapsed) = on_a_thread(
            PROMPT_WAIT_BUDGET * 20,
            "the wait did not come back from a peer that spoke",
            || {
                wait_over_a_socket(PROMPT_WAIT_BUDGET, |mut theirs| {
                    // Nonsense, and a whole message of it: what it means is not the point.
                    theirs
                        .write_all(&[0xff; 32])
                        .expect("write to the socketpair");
                    Some(theirs)
                })
            },
        );

        assert!(
            outcome.is_err(),
            "the peer said something unreadable; that is not an answer to the question"
        );
        assert!(
            elapsed < PROMPT_WAIT_BUDGET / 2,
            "the wait sat out its deadline with something already on the socket: {elapsed:?}"
        );
    }

    /// Half a message is not a fault — failing the capture on one would flake a screenshot on the
    /// loaded host where a split write is likeliest.
    #[test]
    fn a_wait_keeps_waiting_through_half_a_message() {
        let (outcome, elapsed) = on_a_thread(
            PURE_WAIT_BUDGET * 20,
            "the wait never came back from a partial message",
            || {
                // Under the 8 bytes a wayland message header takes.
                wait_over_a_socket(PURE_WAIT_BUDGET, |mut theirs| {
                    theirs.write_all(&[0; 4]).expect("write to the socketpair");
                    Some(theirs)
                })
            },
        );

        let err = outcome.expect_err("half a message answers nothing");
        assert!(
            err.to_string().contains("nothing arrived"),
            "a partial message was reported as a read failure: {err}"
        );
        assert!(
            elapsed >= PURE_WAIT_BUDGET,
            "the wait gave up on the rest of the message instead of waiting for it: {elapsed:?}"
        );
    }

    /// Reporting a compositor that exited as a timeout sends the reader looking for a stall that
    /// never happened.
    #[test]
    fn a_wait_reports_a_peer_that_went_away_rather_than_timing_out() {
        let (outcome, elapsed) = on_a_thread(
            PROMPT_WAIT_BUDGET * 20,
            "the wait did not notice the peer had gone",
            || wait_over_a_socket(PROMPT_WAIT_BUDGET, |_| None),
        );

        let err = outcome.expect_err("a connection that ended cannot answer");
        assert!(
            err.to_string().contains("read"),
            "the end of the connection should be reported where it was found: {err}"
        );
        assert!(
            elapsed < PROMPT_WAIT_BUDGET / 2,
            "a closed connection should be noticed at once: {elapsed:?}"
        );
    }

    /// A timeout here would discard an answer the compositor did give, on a budget the phase
    /// before it spent.
    #[test]
    fn a_wait_takes_an_answer_already_in_hand_over_a_spent_budget() {
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");
        let conn = Connection::from_socket(ours).expect("a connection over the socket");
        let mut queue = conn.new_event_queue::<()>();

        let answered = wait_for(
            &conn,
            &mut queue,
            &mut (),
            Instant::now() - Duration::from_secs(1),
            "test",
            |()| GlassError::Backend("nothing arrived".into()),
            |()| Some(Ok(7)),
        );

        assert_eq!(answered.expect("the answer, not the expired budget"), 7);
    }

    /// A state carrying nothing but the sync's `Dispatch` impl, which is all the bound needs and
    /// is constructible with no compositor.
    struct SyncOnly;

    impl Dispatch<wl_callback::WlCallback, SyncDone> for SyncOnly {
        fn event(
            _: &mut Self,
            _: &wl_callback::WlCallback,
            _: wl_callback::Event,
            done: &SyncDone,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// glass#402: `roundtrip` loops `blocking_dispatch` until the compositor answers the sync, so
    /// a quiet one holds every request that ends in one — which is every request.
    #[test]
    fn a_roundtrip_gives_up_on_a_compositor_that_never_answers() {
        let (outcome, elapsed) = on_a_thread(
            PURE_WAIT_BUDGET * 20,
            "the roundtrip never came back, so it is not bounded by its deadline",
            || {
                let (ours, theirs) = UnixStream::pair().expect("socketpair");
                let conn = Connection::from_socket(ours).expect("a connection over the socket");
                let mut queue = conn.new_event_queue::<SyncOnly>();
                let started = Instant::now();
                let outcome = roundtrip_until(
                    &conn,
                    &mut queue,
                    &mut SyncOnly,
                    Instant::now() + PURE_WAIT_BUDGET,
                    "a caller",
                );
                let elapsed = started.elapsed();
                drop(theirs);
                (outcome, elapsed)
            },
        );

        let err = outcome.expect_err("a sync nothing answered is a failure");
        assert!(
            err.to_string().contains("a caller"),
            "the failure should name the request that made it: {err}"
        );
        assert!(
            elapsed < PURE_WAIT_BUDGET * 10,
            "the roundtrip outlived its deadline by too much to be bounded by it: {elapsed:?}"
        );
        assert!(
            elapsed >= PURE_WAIT_BUDGET,
            "the deadline was cut short: {elapsed:?}"
        );
    }

    /// Kept apart from the loops that act on it so the judgement can be checked without a
    /// compositor.
    #[test]
    fn only_a_read_that_lost_nothing_is_asked_again() {
        use wayland_client::backend::WaylandError;
        let partial = WaylandError::Io(std::io::ErrorKind::WouldBlock.into());
        let gone = WaylandError::Io(std::io::ErrorKind::BrokenPipe.into());
        assert!(is_partial_read(&partial), "half a message is not a fault");
        assert!(
            !is_partial_read(&gone),
            "a connection that ended must not be waited on again"
        );
    }

    /// Nothing on a compositor-less CI leg proves the callback is wired to the right queue, and a
    /// `Dispatch` impl that never fired would turn every request into a five-second timeout.
    #[test]
    fn a_roundtrip_returns_when_the_compositor_answers_the_sync() {
        let (ours, mut theirs) = UnixStream::pair().expect("socketpair");
        let conn = Connection::from_socket(ours).expect("a connection over the socket");
        let mut queue = conn.new_event_queue::<SyncOnly>();

        let answer = std::thread::spawn(move || {
            // `wl_callback.done` for object 2: the display is 1 and the sync's callback is the
            // first object this connection creates. After a beat, so it cannot precede that
            // object's creation.
            std::thread::sleep(Duration::from_millis(50));
            let done: [u8; 12] = {
                let mut m = [0u8; 12];
                m[0..4].copy_from_slice(&2u32.to_ne_bytes());
                // Length in the high half, opcode 0 (`done`) in the low.
                m[4..8].copy_from_slice(&(12u32 << 16).to_ne_bytes());
                m
            };
            theirs.write_all(&done).expect("write the done event");
            theirs
        });

        let started = Instant::now();
        let outcome = on_a_thread(
            PROMPT_WAIT_BUDGET * 20,
            "the roundtrip never came back from an answered sync",
            move || {
                roundtrip_until(
                    &conn,
                    &mut queue,
                    &mut SyncOnly,
                    Instant::now() + PROMPT_WAIT_BUDGET,
                    "a caller",
                )
            },
        );
        let elapsed = started.elapsed();
        drop(answer.join().expect("the answering thread"));

        outcome.expect("the compositor answered the sync");
        assert!(
            elapsed < PROMPT_WAIT_BUDGET / 2,
            "the answer was not noticed until the deadline: {elapsed:?}"
        );
    }

    /// A scratch holding one advertised format, as the compositor's `buffer` event leaves it.
    fn advertised_one() -> CaptureScratch {
        CaptureScratch {
            shm_buffers: vec![(wl_shm::Format::Xrgb8888, 40, 30, 160)],
            ..CaptureScratch::default()
        }
    }

    #[test]
    fn a_finished_format_list_yields_the_buffer_to_allocate() {
        let mut scratch = CaptureScratch {
            buffer_done: true,
            ..advertised_one()
        };
        assert_eq!(
            scratch
                .advertised()
                .expect("the list ended")
                .expect("a format"),
            (wl_shm::Format::Xrgb8888, 40, 30, 160)
        );
    }

    /// Nothing glass can convert is a failure rather than a wait: no further event is coming.
    #[test]
    fn a_finished_but_empty_format_list_is_a_failure() {
        let mut scratch = CaptureScratch {
            buffer_done: true,
            ..CaptureScratch::default()
        };
        let err = scratch
            .advertised()
            .expect("the list ended")
            .expect_err("nothing to allocate");
        assert!(
            err.to_string().contains("no shm format advertised"),
            "{err}"
        );
    }

    /// A refusal is the compositor's final word — waiting it out gains nothing.
    #[test]
    fn a_refusal_during_the_format_list_ends_the_wait() {
        let mut scratch = CaptureScratch {
            done: Some(Err(GlassError::CaptureFailed("screencopy failed".into()))),
            ..CaptureScratch::default()
        };
        let err = scratch
            .advertised()
            .expect("a refusal is an answer")
            .expect_err("the refusal");
        assert!(err.to_string().contains("screencopy failed"), "{err}");
    }

    /// Nothing has been asked to be copied yet, so taking a `ready` as this capture's ends the
    /// next phase over a buffer nothing wrote.
    #[test]
    fn a_ready_before_the_format_list_ends_is_a_failure() {
        let mut scratch = CaptureScratch {
            done: Some(Ok(())),
            ..CaptureScratch::default()
        };
        let err = scratch
            .advertised()
            .expect("a ready is an answer, wrong as it is")
            .expect_err("not this capture's");
        assert!(
            err.to_string().contains("ready before the buffer list"),
            "{err}"
        );
    }

    #[test]
    fn an_unfinished_format_list_is_not_yet_an_answer() {
        assert!(advertised_one().advertised().is_none());
    }

    /// Only one of these two faults is about the protocol version glass binds.
    #[test]
    fn a_timeout_names_silence_and_an_unfinished_list_apart() {
        assert_eq!(
            CaptureScratch::default().no_formats(),
            "screencopy: no buffer event"
        );
        assert!(
            advertised_one().no_formats().contains("buffer_done"),
            "a list that started and never ended should say so"
        );
    }

    fn win(identifier: &str, x11: Option<u32>) -> SwayWindow {
        SwayWindow {
            con_id: 1,
            title: None,
            class: None,
            rect: crate::swayipc::Rect {
                x: 1,
                y: 2,
                width: 30,
                height: 40,
            },
            focused: false,
            identifier: identifier.into(),
            x11_window: x11,
        }
    }

    /// A native Wayland view must be absent rather than carry a placeholder id, which could
    /// collide with a real X window in the cross-check.
    #[test]
    fn only_xwayland_views_contribute_an_x11_id() {
        let wins = [win("a", Some(0x40_0001)), win("b", None), win("c", Some(7))];
        assert_eq!(x11_ids(&wins), vec![0x40_0001, 7]);
        assert!(x11_ids(&[win("native", None)]).is_empty());
    }

    /// Teardown waits on the app's own processes. Waiting on the compositor as well would mean
    /// waiting for something that only exits after glass reaps it.
    #[test]
    fn the_apps_processes_are_the_tree_without_the_compositor() {
        let me = std::process::id();
        // 1 is init, which is neither this process nor an Xwayland — a stand-in for the app.
        assert_eq!(app_pids(&[me, 1], me), vec![1]);
        assert!(
            app_pids(&[me], me).is_empty(),
            "a tree holding only the compositor has no app to wait on"
        );
    }

    #[test]
    fn a_sway_rect_becomes_window_geometry() {
        let g = rect_to_geom(&crate::swayipc::Rect {
            x: 12,
            y: 34,
            width: 300,
            height: 200,
        });
        assert_eq!((g.x, g.y, g.width, g.height), (12, 34, 300, 200));
    }

    /// sway reports an i32 rect. A negative extent is not a window one pixel wide the wrong way
    /// round; it clamps to nothing.
    #[test]
    fn a_negative_extent_clamps_to_zero() {
        let g = rect_to_geom(&crate::swayipc::Rect {
            x: -5,
            y: -6,
            width: -1,
            height: -2,
        });
        assert_eq!((g.x, g.y, g.width, g.height), (-5, -6, 0, 0));
    }

    /// Ids are minted once per toplevel and handed back on every later sighting: a caller that
    /// selected a window by id must still reach the same window after the next enumeration.
    #[test]
    fn a_window_id_is_minted_once_and_reused() {
        let mut ids = HashMap::new();
        let mut next = 0u64;
        let first = mint_id(&mut ids, &mut next, "ftid-a");
        let second = mint_id(&mut ids, &mut next, "ftid-b");
        assert_ne!(first, second, "different toplevels get different ids");
        assert_eq!(mint_id(&mut ids, &mut next, "ftid-a"), first, "stable");
        assert_eq!(next, 2, "re-fetching must not consume an id");
    }

    /// Serializes the tests that exec a fixture they just wrote.
    ///
    /// Another thread's `fork` inherits the write fd `fs::write` holds, until that child execs —
    /// so a sibling spawning mid-write fails this exec with `ETXTBSY`, and the lock must cover
    /// the write, not just the exec. Not a product bug: glass never writes a binary it is about
    /// to probe.
    ///
    /// Poison is ignored — one test's panic must not fail its siblings.
    fn one_spawner_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SPAWN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A directory holding an executable `sway` running `script`.
    ///
    /// Every fixture refuses any argument but `--version`: a probe that stopped passing it would
    /// otherwise fail first on a user's machine, where the walk would *launch a compositor* per
    /// candidate and then report no sway installed.
    fn sway_script(script: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("sway");
        let guard = "[ \"$1\" = --version ] || exit 3\n";
        std::fs::write(&bin, format!("#!/bin/sh\n{guard}{script}")).expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        dir
    }

    /// A directory holding a `sway` that answers `--version` with `reply`.
    fn fake_sway(reply: &str) -> tempfile::TempDir {
        sway_script(&format!("echo '{reply}'\n"))
    }

    /// A directory holding an empty `sway` at `mode`. At 0o644 it is a candidate glass may not
    /// execute; at 0o755 one that clears the permission check and then fails to exec (`ENOEXEC`)
    /// — the deterministic twin of the `ETXTBSY` a concurrent write raises.
    fn empty_sway(mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("sway");
        std::fs::write(&bin, b"").expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("chmod");
        dir
    }

    /// How long the hung fixture lives if nothing kills it, in whole seconds: a sub-second
    /// `Duration` here would truncate to `sleep 0`, leaving the timeout tests no hang to bound.
    const HUNG_SWAY_SECS: u64 = 30;
    /// The budget the hung-candidate tests probe under. Wide enough for `sh` to start and record
    /// its pid, narrow enough that [`VERSION_PROBE_BUDGET`] silently replacing it overshoots the
    /// ceiling those tests assert.
    const HUNG_PROBE_BUDGET: Duration = Duration::from_millis(300);
    /// The budget for the walk tests, which probe a *good* fixture under it too — so it has to
    /// cover a whole successful `sh` run on a loaded host, not just the hung candidate's poll.
    const WALK_PROBE_BUDGET: Duration = Duration::from_secs(2);

    /// A directory holding a `sway` that never answers `--version`, and the file it writes its own
    /// pid to.
    ///
    /// `exec`, so that pid stays the sleeping process: a shell that forked its sleep would hand
    /// the probe a child whose death proves nothing about the process actually left behind.
    fn hung_sway() -> (tempfile::TempDir, PathBuf) {
        let dir = sway_script("");
        let pidfile = dir.path().join("pid");
        let bin = dir.path().join("sway");
        let guard = "[ \"$1\" = --version ] || exit 3\n";
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n{guard}echo $$ > {}\nexec sleep {HUNG_SWAY_SECS}\n",
                pidfile.display()
            ),
        )
        .expect("write");
        (dir, pidfile)
    }

    /// [`sway_in_dirs`] over `dirs` at the budget the product uses. A test about a candidate that
    /// never answers passes its own, tighter budget to `sway_in_dirs` directly.
    fn sway_in(dirs: &[&Path]) -> PathWalk {
        sway_in_dirs(dirs.iter().map(|d| d.to_path_buf()), VERSION_PROBE_BUDGET)
    }

    #[test]
    fn a_recent_sway_on_the_path_is_used() {
        let _guard = one_spawner_at_a_time();
        let dir = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        assert_eq!(sway_in(&[dir.path()]).found, Some(dir.path().join("sway")));
    }

    /// Too old, or a version this cannot read, means fall through to the bundle — glass drives
    /// sway through IPC and protocol surface it only has from 1.12.
    #[test]
    fn an_old_or_unreadable_sway_on_the_path_is_not_used() {
        let _guard = one_spawner_at_a_time();
        for reply in ["sway version 1.9", "sway version 1.11-x", "wat"] {
            let dir = fake_sway(reply);
            assert_eq!(sway_in(&[dir.path()]).found, None, "{reply:?}");
        }
    }

    #[test]
    fn a_path_with_no_sway_on_it_finds_nothing() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(sway_in(&[empty.path()]), PathWalk::default());
    }

    /// glass#392: the probe ran `--version` with no bound at all, so a `sway` that never exits
    /// hung `resolve_sway` — and with it `glass_start` — for as long as it stayed up.
    #[test]
    fn a_sway_that_never_answers_is_stepped_over_within_its_budget() {
        let _guard = one_spawner_at_a_time();
        let (hung, _pidfile) = hung_sway();
        let good = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        let started = Instant::now();
        let walk = sway_in_dirs(
            [hung.path().to_path_buf(), good.path().to_path_buf()].into_iter(),
            WALK_PROBE_BUDGET,
        );
        assert_eq!(
            walk.found,
            Some(good.path().join("sway")),
            "a candidate that never answered must be stepped over, not end the walk"
        );
        assert!(
            started.elapsed() < Duration::from_secs(HUNG_SWAY_SECS) / 3,
            "the walk waited out the whole candidate: {:?}",
            started.elapsed()
        );
    }

    /// A walk that ends empty must name the sway the user has: `nothing_qualifies` sends them
    /// past it to build another.
    #[test]
    fn a_walk_that_finds_only_a_silent_sway_names_it() {
        let _guard = one_spawner_at_a_time();
        let (hung, _pidfile) = hung_sway();
        let walk = sway_in_dirs([hung.path().to_path_buf()].into_iter(), HUNG_PROBE_BUDGET);
        assert_eq!(walk.found, None);
        let no = walk.silent.expect("the hung candidate must be recorded");
        assert!(
            no.cause.contains(&hung.path().display().to_string()),
            "{no:?}"
        );
        assert!(no.cause.contains("did not answer"), "{no:?}");
        assert_eq!(no.remedy, CHECK_THAT_SWAY);
    }

    /// A bound that returns while leaving the process behind trades one hang for a leak: every
    /// launch would add another wedged candidate to the machine.
    #[test]
    fn a_probe_that_times_out_kills_the_candidate() {
        let _guard = one_spawner_at_a_time();
        let (hung, pidfile) = hung_sway();
        let started = Instant::now();
        assert_eq!(
            ask_sway_version(&hung.path().join("sway"), HUNG_PROBE_BUDGET),
            VersionAnswer::TimedOut(HUNG_PROBE_BUDGET)
        );
        // Against the budget passed: ignoring the parameter for VERSION_PROBE_BUDGET would still
        // return, just far later.
        assert!(
            started.elapsed() < HUNG_PROBE_BUDGET * 4,
            "the probe outstayed the budget it was given: {:?}",
            started.elapsed()
        );
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("the fixture records its pid before it sleeps")
            .trim()
            .parse()
            .expect("a pid");
        assert!(
            !glass_proc_linux::any_alive(&[pid]),
            "the probe left its candidate ({pid}) running"
        );
    }

    /// Ran-and-said-nothing kept apart from could-not-be-run: the walk ends on the first, an
    /// answer even when empty, and steps over the second.
    #[test]
    fn a_candidate_that_runs_is_distinguished_from_one_that_cannot() {
        let _guard = one_spawner_at_a_time();
        let good = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        assert_eq!(
            ask_sway_version(&good.path().join("sway"), VERSION_PROBE_BUDGET),
            VersionAnswer::Answered("sway version 1.12-abc (Jun 3 2026)\n".into())
        );
        // Not `/bin/true`: GNU coreutils answers `--version` with its own version string.
        let silent = sway_script("exit 0\n");
        assert_eq!(
            ask_sway_version(&silent.path().join("sway"), VERSION_PROBE_BUDGET),
            VersionAnswer::Answered(String::new())
        );
        let cannot_exec = empty_sway(0o755);
        assert!(
            matches!(
                ask_sway_version(&cannot_exec.path().join("sway"), VERSION_PROBE_BUDGET),
                VersionAnswer::NoReply(_)
            ),
            "a candidate that cannot be exec'd must not read as one that answered"
        );
    }

    /// A candidate that exits leaving something it started holding its stdout: glass never reaches
    /// end-of-file, so it has no answer it may act on.
    #[test]
    fn a_candidate_whose_output_is_never_finished_is_not_an_answer() {
        let _guard = one_spawner_at_a_time();
        let leaky = sway_script(&format!(
            "echo 'sway version 1.12-abc'\nsleep {HUNG_SWAY_SECS} &\n"
        ));
        assert!(
            matches!(
                ask_sway_version(&leaky.path().join("sway"), VERSION_PROBE_BUDGET),
                VersionAnswer::NoReply(_)
            ),
            "output glass could not finish reading is not a version it may act on"
        );
    }

    /// An unset or empty override is not a choice; discovery runs.
    #[test]
    fn no_override_leaves_discovery_to_run() {
        assert!(sway_override(None).is_none());
        assert!(sway_override(Some(std::ffi::OsString::new())).is_none());
    }

    #[test]
    fn an_override_naming_a_real_file_is_trusted_without_a_version_check() {
        // Deliberately not a sway: an explicit path skips the version gate.
        let dir = fake_sway("wat");
        let bin = dir.path().join("sway");
        let chosen = sway_override(Some(bin.clone().into_os_string()))
            .expect("a choice was made")
            .expect("a real file is trusted");
        assert_eq!(chosen, bin);
    }

    /// Fail closed. Falling back to discovery would run a different sway than the one named,
    /// which is how a version-specific bug gets chased in the wrong binary.
    #[test]
    fn an_override_naming_nothing_is_an_error_not_a_fallback() {
        let err = sway_override(Some("/nonexistent/sway".into()))
            .expect("a choice was made")
            .expect_err("a named path that is not there must not fall back");
        assert!(err.cause.contains("/nonexistent/sway"), "{err:?}");
    }

    /// glass#374: the override was checked with `is_file()` while the error it produces has always
    /// said "is not an executable file".
    #[test]
    fn an_override_naming_a_non_executable_file_is_an_error() {
        let dir = empty_sway(0o644);
        let bin = dir.path().join("sway");
        let err = sway_override(Some(bin.clone().into_os_string()))
            .expect("a choice was made")
            .expect_err("a file that cannot be run must not be trusted");
        assert!(err.cause.contains(&bin.display().to_string()), "{err:?}");
    }

    /// A non-executable `sway` early on `PATH` used to cost every later `PATH` entry: the
    /// candidate was accepted on `is_file()`, `--version` then failed to spawn, and
    /// `.output().ok()?` ended the walk — so a good distro sway further along was never seen.
    #[test]
    fn a_non_executable_sway_early_on_the_path_does_not_hide_a_later_one() {
        let _guard = one_spawner_at_a_time();
        let broken = empty_sway(0o644);
        let good = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        assert_eq!(
            sway_in(&[broken.path(), good.path()]).found,
            Some(good.path().join("sway")),
            "a sway that cannot be run must be skipped, not treated as the answer"
        );
    }

    /// An empty file at 0o755 clears the permission check and then fails to exec (`ENOEXEC`) —
    /// the deterministic twin of the `ETXTBSY` a concurrent write raises. Ending the walk on
    /// either made glass report no sway on `$PATH` at all.
    #[test]
    fn a_sway_that_fails_to_spawn_does_not_hide_a_later_one() {
        let _guard = one_spawner_at_a_time();
        let unspawnable = empty_sway(0o755);
        let bin = unspawnable.path().join("sway");
        assert!(
            is_executable_file(&bin),
            "the fixture must clear the permission check, or this pins the wrong branch"
        );
        let good = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        assert_eq!(
            sway_in(&[unspawnable.path(), good.path()]).found,
            Some(good.path().join("sway")),
            "a candidate that never ran must be stepped over, not end the walk"
        );
    }

    /// A bundle root holding `glass/sway/bin/sway` (the data-dir layout) or `sway/bin/sway`
    /// (next to the executable) at `mode`.
    fn bundle_at(relative: &str, mode: u32) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let bin = root.path().join(relative);
        std::fs::create_dir_all(bin.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(&bin, b"").expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("chmod");
        root
    }

    #[test]
    fn a_runnable_bundle_in_the_data_dir_is_found() {
        let root = bundle_at("glass/sway/bin/sway", 0o755);
        assert_eq!(
            sway_bundle_in(Some(root.path().to_path_buf()), None),
            Resolved::Found(root.path().join("glass/sway/bin/sway"))
        );
    }

    /// Skipping it silently is no better than accepting it and failing at spawn: the resulting
    /// "build a sway" names the path just skipped.
    #[test]
    fn a_bundle_that_cannot_be_run_is_named_rather_than_skipped() {
        let root = bundle_at("glass/sway/bin/sway", 0o644);
        assert_eq!(
            sway_bundle_in(Some(root.path().to_path_buf()), None),
            Resolved::NotExecutable(root.path().join("glass/sway/bin/sway"))
        );
    }

    #[test]
    fn a_runnable_bundle_beside_the_executable_beats_an_unrunnable_data_dir_one() {
        let data = bundle_at("glass/sway/bin/sway", 0o644);
        let exe = bundle_at("sway/bin/sway", 0o755);
        assert_eq!(
            sway_bundle_in(
                Some(data.path().to_path_buf()),
                Some(exe.path().to_path_buf())
            ),
            Resolved::Found(exe.path().join("sway/bin/sway"))
        );
    }

    #[test]
    fn no_bundle_in_either_root_is_absent() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            sway_bundle_in(
                Some(empty.path().to_path_buf()),
                Some(empty.path().to_path_buf())
            ),
            Resolved::Absent
        );
        assert_eq!(sway_bundle_in(None, None), Resolved::Absent);
    }

    /// glass#373: with no `$PATH` nothing was searched, and "no sway >=1.12 found" then sends the
    /// user to build one — a build that cannot help, because the search list is what is missing.
    /// MCP clients routinely spawn glass-mcp with a stripped environment.
    #[test]
    fn a_sway_that_could_not_be_looked_up_is_not_reported_as_missing() {
        let no = sway_verdict(None, || Resolved::Absent).expect_err("nothing resolved");
        assert!(no.cause.contains("PATH"), "{no:?}");
        assert!(no.remedy.contains("GLASS_SWAY"), "{no:?}");
        assert!(
            no.remedy.contains("PATH to search"),
            "restoring the search list is the fix nothing else names: {no:?}"
        );
        // The bundle lookup reads the glass data dir, not `$PATH`, so it ran and came back empty:
        // installing the bundle really is one of the ways out, and dropping it would be a worse
        // message than the one this replaced.
        assert!(no.remedy.contains("sway-build"), "{no:?}");
    }

    /// The other half of the distinction: a list that was walked and held no sway really is a
    /// host with no sway, and building one is the fix.
    #[test]
    fn a_search_that_turned_up_nothing_still_says_build_one() {
        let no = sway_verdict(Some(PathWalk::default()), || Resolved::Absent)
            .expect_err("nothing resolved");
        assert_eq!(no.cause, "no sway >=1.12 found");
        assert_eq!(no.remedy, BUILD_A_SWAY);
    }

    /// The bundle lookup is a `current_exe` and a `stat` per candidate, and a `$PATH` hit means
    /// the answer is already in hand — passing it as a value would run it regardless.
    #[test]
    fn a_sway_found_on_path_is_taken_without_looking_for_the_bundle() {
        let walk = PathWalk {
            found: Some(PathBuf::from("/usr/bin/sway")),
            silent: None,
            unreadable: None,
        };
        let found = sway_verdict(Some(walk), || {
            panic!("the bundle must not be looked for once PATH has answered")
        });
        assert_eq!(found, Ok(PathBuf::from("/usr/bin/sway")));
    }

    /// The verdict for a walk that could not stat any `$PATH` entry: the permission is named
    /// rather than "no sway found" (glass#474); a candidate that actually ran (silent) still
    /// outranks it.
    #[test]
    fn an_unreadable_path_walk_outranks_nothing_qualifies() {
        let unreadable = Some((
            PathBuf::from("/usr/sway/bin/sway"),
            "Permission denied (os error 13)".to_string(),
        ));
        let no = sway_verdict(
            Some(PathWalk {
                found: None,
                silent: None,
                unreadable,
            }),
            || Resolved::Absent,
        )
        .expect_err("nothing runnable");
        assert!(no.cause.contains("/usr/sway/bin/sway"), "{}", no.message());
        assert!(
            no.cause.contains("could not be looked at"),
            "{}",
            no.message()
        );
        assert_eq!(no.remedy, MAKE_IT_RUNNABLE);

        let no = sway_verdict(
            Some(PathWalk {
                found: None,
                silent: Some(NoSway::silent(
                    Path::new("/usr/bin/sway"),
                    "it ran and said nothing",
                )),
                unreadable: Some((
                    PathBuf::from("/usr/sway/bin/sway"),
                    "Permission denied (os error 13)".to_string(),
                )),
            }),
            || Resolved::Absent,
        )
        .expect_err("nothing runnable");
        assert!(
            no.cause.contains("/usr/bin/sway"),
            "the running sway outranks the permission: {}",
            no.message()
        );
        assert_eq!(no.remedy, CHECK_THAT_SWAY);
    }

    /// A `$PATH` entry glass cannot stat must not silently drop the sway it holds: the walk
    /// records the permission and the verdict reports it (glass#474). Root can traverse a
    /// `0o000` directory, so this needs a non-root host (same guard as the `resolve_bin`
    /// permission test).
    #[test]
    fn a_path_prefix_the_walk_cannot_stat_is_named_not_skipped() {
        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: root can traverse a 0o000 directory, so the EACCES never fires");
            return;
        }
        let _guard = one_spawner_at_a_time();
        let dir = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        let prefix = dir.path().to_path_buf();
        std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let walk = sway_in(&[dir.path()]);
        std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let Some((p, e)) = walk.unreadable else {
            panic!("the walk cannot stat {prefix:?}, so it must record the permission");
        };
        assert_eq!(p, prefix.join("sway"));
        assert!(e.contains("Permission denied"), "{e}");
        assert!(walk.found.is_none());
    }

    /// The launch path gets one string where doctor gets two columns, so it must carry both.
    #[test]
    fn a_failure_message_carries_the_cause_and_the_fix() {
        let msg = NoSway::not_runnable("/opt/sway is not executable".into()).message();
        assert!(msg.contains("/opt/sway is not executable"), "{msg}");
        assert!(msg.contains("chmod +x"), "{msg}");

        let msg = NoSway::nothing_qualifies().message();
        assert!(msg.contains("no sway >=1.12 found"), "{msg}");
        assert!(msg.contains("sway-build"), "{msg}");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn discovery_finds_a_real_sway_on_this_machine() {
        let found = resolve_sway().expect("a discoverable sway");
        assert!(
            is_executable_file(&found),
            "discovery must yield something glass can spawn: {}",
            found.display()
        );
    }

    /// A probe that panics is the only way to assert something is *not* called. Note this drives
    /// the helper, not `start_app`'s call site, which is where an eager argument would bite.
    #[test]
    fn a_launch_with_the_sandbox_off_never_probes_for_bubblewrap() {
        ensure_sandbox_available(glass_core::SandboxLevel::Off, || {
            panic!("sandbox:\"off\" must not fork bwrap")
        })
        .expect("an unconfined launch is always allowed");
    }

    #[test]
    fn a_sandboxed_launch_is_refused_when_bubblewrap_is_unavailable() {
        let err = ensure_sandbox_available(glass_core::SandboxLevel::Default, || {
            glass_sandbox_linux::Availability::Unavailable("no bwrap here".into())
        })
        .expect_err("fail closed rather than launching unconfined");
        assert!(matches!(err, GlassError::SandboxUnavailable(_)), "{err}");
        // The cause and its fix are the probe's to write — glass-sandbox-linux asserts that every
        // one of them offers `sandbox:"off"`.
        assert!(err.to_string().contains("no bwrap here"), "{err}");
        assert!(err.to_string().contains("glass-mcp doctor"), "{err}");
    }

    #[test]
    fn a_sandboxed_launch_proceeds_when_bubblewrap_is_available() {
        ensure_sandbox_available(glass_core::SandboxLevel::Default, || {
            glass_sandbox_linux::Availability::Ok
        })
        .expect("an available sandbox allows the launch");
    }

    /// sway's own socket sits in the same directory as its IPC socket and the lock file.
    #[test]
    fn the_wayland_socket_is_picked_out_of_the_runtime_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        for decoy in ["wayland-1.lock", "sway-ipc.1000.9.sock", "wayland-"] {
            std::fs::write(dir.path().join(decoy), b"").expect("decoy");
        }
        let real = dir.path().join("wayland-1");
        std::fs::write(&real, b"").expect("socket");
        assert_eq!(find_wayland_socket(dir.path()), Some(real));
    }

    #[test]
    fn a_runtime_dir_with_no_wayland_socket_finds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("sway-ipc.1000.9.sock"), b"").expect("decoy");
        assert_eq!(find_wayland_socket(dir.path()), None);
    }

    /// The XKB real-modifier bits, in the order `include "complete"` assigns them.
    #[test]
    fn each_modifier_is_its_own_xkb_bit() {
        use glass_core::keys::Modifier;
        assert_eq!(modifier_mask(&[]), 0);
        assert_eq!(modifier_mask(&[Modifier::Shift]), 0b1);
        assert_eq!(modifier_mask(&[Modifier::Control]), 0b100);
        assert_eq!(modifier_mask(&[Modifier::Alt]), 0b1000);
        assert_eq!(modifier_mask(&[Modifier::Super]), 0b100_0000);
        assert_eq!(
            modifier_mask(&[Modifier::Shift, Modifier::Control, Modifier::Super]),
            0b100_0101,
            "a chord's modifiers are the union of their bits"
        );
        // A repeat discriminates a union from an exclusive-or, which agree on every set of
        // distinct modifiers and so on every other case here.
        assert_eq!(
            modifier_mask(&[Modifier::Shift, Modifier::Shift]),
            0b1,
            "naming a modifier twice still holds it down"
        );
    }
}

#[cfg(test)]
mod session_tests {
    //! The backend against a real compositor. Every method below talks to sway or to the wayland
    //! connection, so there is nothing underneath them to fake; [`crate::testw`] launches a
    //! private session and observes it over a connection the backend does not own.
    use std::collections::VecDeque;
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::testw::{Launch, READY_LINE};
    use x11rb::protocol::xproto::{
        GetInputFocusReply, GetWindowAttributesReply, MapState, QueryTreeReply, Setup, WindowClass,
    };
    use x11rb::x11_utils::Serialize as _;

    const SCRIPTED_X_ROOT: u32 = 0x100;
    const SCRIPTED_X_WINDOW: u32 = 0x200;
    const SCRIPTED_X_WINDOW_2: u32 = 0x201;
    const STALLED_X_REPLY: Duration = Duration::from_secs(1);
    const X_REPLY_DEADLINE: Duration = Duration::from_millis(50);

    fn scripted_x_recovery(
        missing_before: &[u32],
        script: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (crate::xwayland::Recovery, std::thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().expect("scripted X11 socketpair");
        let setup = Setup {
            resource_id_base: 0x0100_0000,
            resource_id_mask: 0x00ff_ffff,
            maximum_request_length: u16::MAX,
            ..Setup::default()
        };
        let recovery = crate::xwayland::Recovery::with_test_connection(
            Path::new("/nonexistent/scripted-xwayland"),
            client,
            setup,
            SCRIPTED_X_ROOT,
            missing_before,
        );
        (recovery, std::thread::spawn(move || script(server)))
    }

    fn scripted_x_recovery_with_clock(
        missing_before: &[u32],
        observations: Vec<Instant>,
        script: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (crate::xwayland::Recovery, std::thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().expect("scripted X11 socketpair");
        let setup = Setup {
            resource_id_base: 0x0100_0000,
            resource_id_mask: 0x00ff_ffff,
            maximum_request_length: u16::MAX,
            ..Setup::default()
        };
        let observations = Arc::new(Mutex::new(VecDeque::from(observations)));
        let clock = {
            let observations = Arc::clone(&observations);
            Arc::new(move || {
                observations
                    .lock()
                    .expect("deadline observation mutex")
                    .pop_front()
                    .expect("unexpected Xwayland deadline observation")
            })
        };
        let recovery = crate::xwayland::Recovery::with_test_connection_and_clock(
            Path::new("/nonexistent/scripted-xwayland"),
            client,
            setup,
            SCRIPTED_X_ROOT,
            missing_before,
            clock,
        );
        (recovery, std::thread::spawn(move || script(server)))
    }

    fn expect_x_request(peer: &mut UnixStream, sequence: &mut u16, opcode: u8) {
        let mut header = [0u8; 4];
        peer.read_exact(&mut header).expect("X11 request header");
        let words = u16::from_ne_bytes([header[2], header[3]]) as usize;
        assert!(words > 0, "the fixture does not accept BigRequests");
        let mut body = vec![0u8; words * 4 - header.len()];
        peer.read_exact(&mut body).expect("X11 request body");
        *sequence += 1;
        assert_eq!(
            header[0], opcode,
            "unexpected X11 request at sequence {sequence}"
        );
    }

    fn answer_query_tree(peer: &mut UnixStream, sequence: u16, children: Vec<u32>) {
        let reply = QueryTreeReply {
            sequence,
            length: children.len() as u32,
            root: SCRIPTED_X_ROOT,
            parent: 0,
            children,
        };
        peer.write_all(&reply.serialize())
            .expect("query-tree reply");
    }

    fn answer_viewable_attributes(peer: &mut UnixStream, sequence: u16) {
        let reply = GetWindowAttributesReply {
            sequence,
            length: 3,
            class: WindowClass::INPUT_OUTPUT,
            map_state: MapState::VIEWABLE,
            override_redirect: false,
            ..GetWindowAttributesReply::default()
        };
        peer.write_all(&reply.serialize())
            .expect("attributes reply");
    }

    fn answer_sync(peer: &mut UnixStream, sequence: u16) {
        let reply = GetInputFocusReply {
            sequence,
            ..GetInputFocusReply::default()
        };
        let mut bytes = reply.serialize().to_vec();
        bytes.resize(32, 0);
        peer.write_all(&bytes).expect("sync reply");
    }

    fn hold_scripted_peer_open() {
        // x11rb drains every packet currently readable before returning one reply. A live peer
        // makes the next non-blocking read report WouldBlock rather than EOF.
        std::thread::sleep(Duration::from_millis(100));
    }

    fn install_recovery(session: &mut crate::testw::Session, recovery: crate::xwayland::Recovery) {
        session
            .platform()
            .active
            .as_mut()
            .expect("a started session")
            .recovery = recovery;
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_blocked_proc_entry_read_cannot_outlive_the_window_list_deadline() {
        let proc = tempfile::tempdir().expect("fake proc root");
        let pid = proc.path().join("4242");
        std::fs::create_dir(&pid).expect("fake pid directory");
        let comm = pid.join("comm");
        let made_fifo = std::process::Command::new("mkfifo")
            .arg(&comm)
            .status()
            .expect("run mkfifo");
        assert!(made_fifo.success(), "mkfifo failed: {made_fifo}");

        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut fifo = std::fs::OpenOptions::new()
                .write(true)
                .open(comm)
                .expect("open blocked comm writer");
            opened_tx.send(()).expect("blocked-read observer");
            release_rx.recv().expect("release blocked comm read");
            fifo.write_all(b"not-Xwayland\n")
                .expect("release blocked comm read");
        });

        let runtime = tempfile::tempdir().expect("runtime directory");
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let call = std::thread::spawn(move || {
            let mut session = Launch::new().start_mapped();
            let recovery =
                crate::xwayland::Recovery::with_test_proc_root(runtime.path(), proc.path());
            install_recovery(&mut session, recovery);
            let started = Instant::now();
            let result = session
                .platform()
                .list_windows_by(Deadline::at(started + X_REPLY_DEADLINE));
            result_tx
                .send((started.elapsed(), result))
                .expect("window-list result receiver");
        });

        opened_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the production discovery read never reached the FIFO");
        let bounded = result_rx.recv_timeout(Duration::from_millis(300));
        release_tx.send(()).expect("release blocked read");
        writer.join().expect("FIFO writer");
        call.join().expect("window-list caller");

        let (elapsed, result) =
            bounded.expect("the blocked proc read outlived its caller deadline");
        let error = result.expect_err("the caller deadline must end blocked discovery");
        assert!(elapsed < Duration::from_millis(250), "{elapsed:?}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert!(
            error.to_string().contains("Xwayland recovery discovery"),
            "{error}"
        );
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn spent_window_list_deadline_sends_no_x_request_and_the_session_remains_usable() {
        let mut session = Launch::new().start_mapped();
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let (recovery, server) = scripted_x_recovery(&[], move |mut peer| {
            peer.set_read_timeout(Some(Duration::from_millis(150)))
                .expect("read timeout");
            let mut first = [0u8; 1];
            let no_request = peer.read(&mut first).is_err_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            });
            observed_tx.send(no_request).expect("observation receiver");
            peer.set_read_timeout(None).expect("clear read timeout");

            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            answer_query_tree(&mut peer, sequence, Vec::new());
            hold_scripted_peer_open();
        });
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::at(Instant::now() - Duration::from_millis(1)))
            .expect_err("an already-spent window-list deadline must reject before dispatch");

        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(
            observed_rx.recv().expect("X11 observation"),
            "the rejected call sent an X11 request"
        );
        let windows = session
            .platform()
            .list_windows_by(Deadline::from_millis(1_000))
            .expect("the next window list should still use the session");
        assert!(!windows.is_empty());
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_late_xwayland_tree_reply_cannot_become_window_list_success() {
        let mut session = Launch::new().start_mapped();
        let (recovery, server) = scripted_x_recovery(&[], |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            std::thread::sleep(STALLED_X_REPLY);
            answer_query_tree(&mut peer, sequence, Vec::new());
            hold_scripted_peer_open();
        });
        install_recovery(&mut session, recovery);

        let started = Instant::now();
        let error = session
            .platform()
            .list_windows_by(Deadline::at(started + X_REPLY_DEADLINE))
            .expect_err("a query reply after the caller deadline is not success");
        let elapsed = started.elapsed();

        assert!(
            elapsed < STALLED_X_REPLY * 3 / 4,
            "the Xwayland query outlived its deadline: {elapsed:?}"
        );
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("Xwayland tree"), "{error}");
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_successful_tree_reply_observed_after_the_deadline_is_rejected() {
        let mut session = Launch::new().start_mapped();
        let expires = Instant::now() + Duration::from_secs(5);
        let (recovery, server) = scripted_x_recovery_with_clock(
            &[],
            vec![expires + Duration::from_millis(1)],
            |mut peer| {
                let mut sequence = 0;
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::QUERY_TREE_REQUEST,
                );
                answer_query_tree(&mut peer, sequence, Vec::new());
                hold_scripted_peer_open();
            },
        );
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::at(expires))
            .expect_err("a completed tree reply observed after expiry is not success");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert!(error.to_string().contains("Xwayland tree query"), "{error}");
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn successful_attributes_observed_after_the_deadline_are_rejected() {
        let mut session = Launch::new().start_mapped();
        let expires = Instant::now() + Duration::from_secs(5);
        let before = expires - Duration::from_millis(1);
        let (recovery, server) = scripted_x_recovery_with_clock(
            &[],
            vec![before, expires + Duration::from_millis(1)],
            |mut peer| {
                let mut sequence = 0;
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::QUERY_TREE_REQUEST,
                );
                answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
                );
                answer_viewable_attributes(&mut peer, sequence);
                hold_scripted_peer_open();
            },
        );
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::at(expires))
            .expect_err("completed attributes observed after expiry are not success");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert!(
            error.to_string().contains("Xwayland window attributes"),
            "{error}"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_timed_out_cached_scan_accepts_its_late_reply_then_performs_a_fresh_recheck() {
        let mut session = Launch::new().start_mapped();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (rechecked_tx, rechecked_rx) = std::sync::mpsc::channel();
        let (recovery, server) = scripted_x_recovery(&[], move |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            release_rx
                .recv()
                .expect("release the first query's late reply");
            answer_query_tree(&mut peer, sequence, Vec::new());

            peer.set_read_timeout(Some(Duration::from_millis(300)))
                .expect("fresh-query observation timeout");
            let mut header = [0u8; 4];
            let rechecked = match peer.read_exact(&mut header) {
                Ok(()) => {
                    let words = u16::from_ne_bytes([header[2], header[3]]) as usize;
                    let mut body = vec![0u8; words * 4 - header.len()];
                    peer.read_exact(&mut body).expect("fresh query body");
                    sequence += 1;
                    assert_eq!(
                        header[0],
                        x11rb::protocol::xproto::QUERY_TREE_REQUEST,
                        "the retry sent a different X11 request"
                    );
                    answer_query_tree(&mut peer, sequence, Vec::new());
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read fresh query: {error}"),
            };
            rechecked_tx.send(rechecked).expect("fresh-query observer");
            hold_scripted_peer_open();
        });
        install_recovery(&mut session, recovery);

        let first = session
            .platform()
            .list_windows_by(Deadline::from_millis(X_REPLY_DEADLINE.as_millis() as u64))
            .expect_err("the first cached scan must time out");
        assert_eq!(first.bound_owner(), Some(Whose::Caller));
        release_tx.send(()).expect("release late query reply");

        let windows = session
            .platform()
            .list_windows_by(Deadline::from_millis(1_000))
            .expect("the retry should complete a fresh X cross-check");
        assert!(!windows.is_empty());
        assert!(
            rechecked_rx.recv().expect("fresh-query observation"),
            "the timed-out read-only scan consumed the successful throttle window"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_stalled_xwayland_attribute_reply_obeys_the_window_list_deadline() {
        let mut session = Launch::new().start_mapped();
        let (recovery, server) = scripted_x_recovery(&[], |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
            );
            std::thread::sleep(STALLED_X_REPLY);
            answer_viewable_attributes(&mut peer, sequence);
            hold_scripted_peer_open();
        });
        install_recovery(&mut session, recovery);

        let started = Instant::now();
        let error = session
            .platform()
            .list_windows_by(Deadline::at(started + X_REPLY_DEADLINE))
            .expect_err("a stalled attribute reply must not hold the window list");
        let elapsed = started.elapsed();

        assert!(
            elapsed < STALLED_X_REPLY * 3 / 4,
            "the Xwayland attribute read outlived its deadline: {elapsed:?}"
        );
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(
            error.to_string().contains("Xwayland window attributes"),
            "{error}"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn an_unmap_confirmation_stall_reports_the_possible_hidden_window_and_sends_no_map() {
        let mut session = Launch::new().start_mapped();
        let (mapped_tx, mapped_rx) = std::sync::mpsc::channel();
        let (recovery, server) = scripted_x_recovery(&[SCRIPTED_X_WINDOW], move |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
            );
            answer_viewable_attributes(&mut peer, sequence);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
            );
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
            );
            std::thread::sleep(STALLED_X_REPLY);
            answer_sync(&mut peer, sequence);
            peer.set_read_timeout(Some(Duration::from_millis(150)))
                .expect("read timeout");
            let mut next = [0u8; 4];
            let no_map = peer.read_exact(&mut next).is_err_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            });
            mapped_tx.send(no_map).expect("map observation receiver");
        });
        install_recovery(&mut session, recovery);

        let started = Instant::now();
        let error = session
            .platform()
            .list_windows_by(Deadline::at(started + X_REPLY_DEADLINE))
            .expect_err("an unconfirmed unmap must be surfaced");
        let elapsed = started.elapsed();

        assert!(elapsed < STALLED_X_REPLY * 3 / 4, "{elapsed:?}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("may now be hidden"), "{error}");
        assert!(
            mapped_rx.recv().expect("map observation"),
            "recovery dispatched a map after its shared deadline"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_remap_confirmation_stall_preserves_partial_side_effect_provenance() {
        let mut session = Launch::new().start_mapped();
        let (retried_tx, retried_rx) = std::sync::mpsc::channel();
        let (recovery, server) = scripted_x_recovery(&[SCRIPTED_X_WINDOW], move |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
            );
            answer_viewable_attributes(&mut peer, sequence);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
            );
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
            );
            answer_sync(&mut peer, sequence);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::MAP_WINDOW_REQUEST,
            );
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
            );
            std::thread::sleep(STALLED_X_REPLY);
            answer_sync(&mut peer, sequence);
            peer.set_read_timeout(Some(Duration::from_millis(150)))
                .expect("read timeout");
            let mut next = [0u8; 4];
            let no_retry = peer.read_exact(&mut next).is_err_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            });
            retried_tx
                .send(no_retry)
                .expect("retry observation receiver");
        });
        install_recovery(&mut session, recovery);

        let started = Instant::now();
        let error = session
            .platform()
            .list_windows_by(Deadline::at(started + X_REPLY_DEADLINE))
            .expect_err("an unconfirmed re-map must be surfaced");
        let elapsed = started.elapsed();

        assert!(elapsed < STALLED_X_REPLY * 3 / 4, "{elapsed:?}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(
            error.to_string().contains("window visibility is uncertain"),
            "{error}"
        );
        assert!(
            retried_rx.recv().expect("retry observation"),
            "recovery retried the map after its shared deadline"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_successful_map_confirmation_observed_after_the_deadline_is_rejected() {
        let mut session = Launch::new().start_mapped();
        let expires = Instant::now() + Duration::from_secs(5);
        let before = expires - Duration::from_millis(1);
        let (recovery, server) = scripted_x_recovery_with_clock(
            &[SCRIPTED_X_WINDOW],
            vec![before, before, expires + Duration::from_millis(1)],
            |mut peer| {
                let mut sequence = 0;
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::QUERY_TREE_REQUEST,
                );
                answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
                );
                answer_viewable_attributes(&mut peer, sequence);
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
                );
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
                );
                answer_sync(&mut peer, sequence);
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::MAP_WINDOW_REQUEST,
                );
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
                );
                answer_sync(&mut peer, sequence);
                hold_scripted_peer_open();
            },
        );
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::at(expires))
            .expect_err("a completed map confirmation observed after expiry is not success");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(
            error.to_string().contains("visibility is uncertain"),
            "{error}"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_transport_failure_after_unmap_is_a_structured_visibility_failure() {
        let mut session = Launch::new().start_mapped();
        let (recovery, server) = scripted_x_recovery(&[SCRIPTED_X_WINDOW], |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
            );
            answer_viewable_attributes(&mut peer, sequence);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
            );
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
            );
        });
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::from_millis(1_000))
            .expect_err("a closed transport after unmap cannot become window-list success");

        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(matches!(error, GlassError::AfterDispatch(_)), "{error:?}");
        assert!(error.to_string().contains("0 window(s)"), "{error}");
        assert!(error.to_string().contains("0x200"), "{error}");
        assert!(error.to_string().contains("may now be hidden"), "{error}");
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_transport_failure_after_map_is_a_structured_visibility_failure() {
        let mut session = Launch::new().start_mapped();
        let (recovery, server) = scripted_x_recovery(&[SCRIPTED_X_WINDOW], |mut peer| {
            let mut sequence = 0;
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::QUERY_TREE_REQUEST,
            );
            answer_query_tree(&mut peer, sequence, vec![SCRIPTED_X_WINDOW]);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
            );
            answer_viewable_attributes(&mut peer, sequence);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
            );
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
            );
            answer_sync(&mut peer, sequence);
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::MAP_WINDOW_REQUEST,
            );
            expect_x_request(
                &mut peer,
                &mut sequence,
                x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
            );
        });
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::from_millis(1_000))
            .expect_err("a closed transport after map cannot become window-list success");

        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(matches!(error, GlassError::AfterDispatch(_)), "{error:?}");
        assert!(error.to_string().contains("0 window(s)"), "{error}");
        assert!(error.to_string().contains("0x200"), "{error}");
        assert!(
            error.to_string().contains("visibility is uncertain"),
            "{error}"
        );
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_later_transport_failure_reports_earlier_successful_remaps() {
        let mut session = Launch::new().start_mapped();
        let (recovery, server) =
            scripted_x_recovery(&[SCRIPTED_X_WINDOW, SCRIPTED_X_WINDOW_2], |mut peer| {
                let mut sequence = 0;
                expect_x_request(
                    &mut peer,
                    &mut sequence,
                    x11rb::protocol::xproto::QUERY_TREE_REQUEST,
                );
                answer_query_tree(
                    &mut peer,
                    sequence,
                    vec![SCRIPTED_X_WINDOW, SCRIPTED_X_WINDOW_2],
                );
                for _ in 0..2 {
                    expect_x_request(
                        &mut peer,
                        &mut sequence,
                        x11rb::protocol::xproto::GET_WINDOW_ATTRIBUTES_REQUEST,
                    );
                    answer_viewable_attributes(&mut peer, sequence);
                }

                for opcode in [
                    x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
                    x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
                ] {
                    expect_x_request(&mut peer, &mut sequence, opcode);
                }
                answer_sync(&mut peer, sequence);
                for opcode in [
                    x11rb::protocol::xproto::MAP_WINDOW_REQUEST,
                    x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
                ] {
                    expect_x_request(&mut peer, &mut sequence, opcode);
                }
                answer_sync(&mut peer, sequence);

                for opcode in [
                    x11rb::protocol::xproto::UNMAP_WINDOW_REQUEST,
                    x11rb::protocol::xproto::GET_INPUT_FOCUS_REQUEST,
                ] {
                    expect_x_request(&mut peer, &mut sequence, opcode);
                }
            });
        install_recovery(&mut session, recovery);

        let error = session
            .platform()
            .list_windows_by(Deadline::from_millis(1_000))
            .expect_err("partial Xwayland recovery must not become window-list success");

        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("1 window(s)"), "{error}");
        assert!(error.to_string().contains("0x201"), "{error}");
        assert!(error.to_string().contains("may now be hidden"), "{error}");
        server.join().expect("scripted X11 server");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_launch_reports_the_window_the_app_actually_mapped() {
        let mut s = Launch::new().windows(&["solo:solo:300x200"]).start();
        let geo = s.platform().window(&WindowOp::Geometry).expect("geometry");
        let wins = s.windows();
        assert_eq!(wins.len(), 1);
        let rect = &wins[0].rect;
        assert_eq!(
            (geo.width, geo.height),
            (rect.width as u32, rect.height as u32),
            "the session contract must match what sway reports"
        );
        assert_eq!((geo.width, geo.height), (300, 200));
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn enumeration_reports_every_window_with_a_stable_id() {
        let mut s = Launch::new()
            .windows(&["one:app-one:200x100", "two:app-two:150x120"])
            .start();
        s.until("both windows to map", |s| s.windows().len() == 2);
        let first = s.platform().list_windows().expect("list");
        assert_eq!(first.len(), 2);
        let titles: Vec<&str> = first.iter().filter_map(|w| w.title.as_deref()).collect();
        assert!(
            titles.contains(&"one") && titles.contains(&"two"),
            "{titles:?}"
        );
        assert_eq!(
            first.iter().filter(|w| w.active).count(),
            1,
            "exactly one window is focused"
        );
        let again = s.platform().list_windows().expect("list again");
        let ids = |ws: &[WindowInfo]| {
            let mut v: Vec<(u64, Option<String>)> =
                ws.iter().map(|w| (w.id.0, w.title.clone())).collect();
            v.sort();
            v
        };
        assert_eq!(ids(&first), ids(&again), "ids must survive re-enumeration");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn selecting_a_window_focuses_it_and_reports_its_geometry() {
        let mut s = Launch::new()
            .windows(&["one:app-one:200x100", "two:app-two:150x120"])
            .start();
        s.until("both windows to map", |s| s.windows().len() == 2);
        let wins = s.platform().list_windows().expect("list");
        let target = wins
            .iter()
            .find(|w| !w.active)
            .expect("one window is not focused");
        let geo = s.platform().select_window(target.id).expect("select");
        assert_eq!(geo, target.geometry);
        assert_eq!(
            s.focused_title().as_deref(),
            target.title.as_deref(),
            "the compositor must actually have moved focus"
        );
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn selecting_a_window_that_is_not_there_reports_it_not_found() {
        let mut s = Launch::new().start();
        let err = s
            .platform()
            .select_window(WindowId(4242))
            .expect_err("no such window");
        assert!(matches!(err, GlassError::WindowNotFound), "{err}");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn moving_and_resizing_change_what_the_compositor_reports() {
        let mut s = Launch::new().start();
        let resized = s
            .platform()
            .window(&WindowOp::Resize {
                width: 260,
                height: 180,
            })
            .expect("resize");
        assert_eq!((resized.width, resized.height), (260, 180));
        let moved = s
            .platform()
            .window(&WindowOp::Move { x: 40, y: 30 })
            .expect("move");
        assert_eq!((moved.x, moved.y), (40, 30));
        let observed = s.windows();
        let rect = &observed[0].rect;
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (40, 30, 260, 180),
            "the reported geometry must be the compositor's, not glass's own idea of it"
        );
    }

    /// The sink is sway's piped stdout and stderr, which the app inherits — so both streams
    /// arrive intermixed, and a line the app printed is the only proof the launch captured it.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_apps_output_reaches_the_log_sink() {
        let mut s = Launch::new().start();
        // `wait_for_log` is the assertion: it panics unless the line arrives.
        s.wait_for_log(READY_LINE);
    }

    /// The a11y reader correlates an AT-SPI connection against this set, so it has to reach past
    /// the compositor to the app: sway's pid is not the app's.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_process_set_reaches_past_the_compositor_to_the_app() {
        let mut s = Launch::new().start();
        let pids = s.platform().app_pids();
        assert!(
            pids.len() >= 2,
            "the compositor alone is not the app's process set: {pids:?}"
        );
        assert!(pids.iter().all(|p| *p > 1), "no placeholder pids: {pids:?}");
    }

    /// No a11y bus was asked for, so there is no address to hand out. Inventing one would send
    /// the reader at the user's own desktop bus.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_session_launched_without_accessibility_has_no_bus_address() {
        let mut s = Launch::new().start();
        assert_eq!(s.platform().a11y_bus_addr(), None);
    }

    /// Coordinates are window-relative at glass's boundary and the backend maps them to the
    /// output. The app is the only witness: Wayland has no way to ask where the pointer is, so
    /// the fixture echoes the surface-local point it was given back through its own stdout.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_pointer_move_arrives_at_the_requested_window_relative_point() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Move { x: 40, y: 30 })
            .expect("move");
        let lines = s.wait_for_log("input: ");
        assert!(
            lines.iter().any(|l| l.ends_with("40 30")),
            "the app should have been pointed at its own (40, 30): {lines:#?}"
        );
    }

    /// A window away from the output origin is the case an origin mix-up survives: with the
    /// window at (0, 0) a window-relative and an output-absolute point are the same number.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_pointer_move_is_relative_to_a_window_that_is_not_at_the_origin() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .window(&WindowOp::Move { x: 100, y: 80 })
            .expect("move the window");
        s.platform()
            .send_pointer(&PointerEvent::Move { x: 25, y: 15 })
            .expect("move the pointer");
        let lines = s.wait_for_log("input: ");
        assert!(
            lines.iter().any(|l| l.ends_with("25 15")),
            "the point must be relative to the window, not the output: {lines:#?}"
        );
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_click_presses_and_releases_the_button_over_the_window() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Click {
                x: 20,
                y: 20,
                button: glass_core::MouseButton::Left,
                count: 1,
                modifiers: vec![],
            })
            .expect("click");
        let lines = s.wait_for_log("input: button");
        let buttons: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("input: button"))
            .collect();
        assert!(
            buttons.iter().any(|l| l.ends_with(" 272 1")),
            "left button pressed: {buttons:?}"
        );
        assert!(
            buttons.iter().any(|l| l.ends_with(" 272 0")),
            "and released: {buttons:?}"
        );
    }

    /// A modified click holds the modifier across the press and releases it after, and a plain
    /// one sends no modifier traffic at all. Both directions, because a single one cannot tell
    /// the guard from its inverse — and an app reads ctrl+click as ctrl+click only if control is
    /// already down when the button arrives.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_click_carries_its_modifiers_and_a_plain_one_sends_none() {
        let mut s = Launch::new().start_mapped();
        let click = |modifiers: Vec<glass_core::keys::Modifier>| PointerEvent::Click {
            x: 20,
            y: 20,
            button: glass_core::MouseButton::Left,
            count: 1,
            modifiers,
        };
        s.platform().send_pointer(&click(vec![])).expect("plain");
        let plain = s.wait_for_log(" 272 0");
        assert!(
            !plain.iter().any(|l| l.contains("input: mods")),
            "a plain click must send no modifier traffic: {plain:#?}"
        );
        s.platform()
            .send_pointer(&click(vec![glass_core::keys::Modifier::Control]))
            .expect("modified");
        let modified = s.wait_for_log("mods 0");
        let at = |needle: &str| modified.iter().position(|l| l.contains(needle));
        let (down, press, up) = (
            at("mods 4").expect("control down"),
            at(" 272 1").expect("pressed"),
            at("mods 0").expect("control released"),
        );
        assert!(
            down < press && press < up,
            "control must be held across the click: {modified:#?}"
        );
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_scroll_reaches_the_window_as_an_axis_event() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Scroll {
                x: 20,
                y: 20,
                dx: 0,
                dy: -2,
                modifiers: vec![],
            })
            .expect("scroll");
        let lines = s.wait_for_log("input: axis");
        let axis = lines
            .iter()
            .find(|l| l.contains("input: axis"))
            .expect("a vertical wheel");
        // `axis 0` is the vertical axis; the value is the wheel delta scaled to surface units.
        // Asserting the number, not just that one arrived: a delta computed by adding or
        // dividing instead of scaling still produces an axis event, just the wrong distance.
        let value: f64 = axis
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no axis value in {axis:?}"));
        assert!(axis.contains(" 0 "), "the vertical axis: {axis}");
        assert!(
            (value - -30.0).abs() < 0.01,
            "two notches up is -30 surface units, got {value} in {axis:?}"
        );
        // The scroll sink keeps its own clock, and a compositor drops an event whose time did
        // not move — so a stuck clock here loses the wheel, not just its ordering.
        let times = crate::testw::event_times(&lines);
        assert!(
            times.last() > times.first(),
            "the clock must advance across the scroll: {times:?}"
        );
    }

    /// Once another client takes the selection this owner's thread is done, and a second write
    /// has to start a fresh one. Updating the dead thread's text instead leaves the other
    /// client's value on the clipboard while glass reports the write as done.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn writing_the_clipboard_after_losing_it_starts_a_new_owner() {
        let mut s = Launch::new().start();
        s.platform().set_clipboard("ours").expect("set");
        assert_eq!(s.platform().get_clipboard().expect("get"), "ours");
        // A second client takes the selection out from under the backend's owner.
        let socket = s.wayland_socket();
        let thief =
            crate::clipboard::ClipboardOwner::spawn(socket, "theirs".into()).expect("spawn");
        s.until("the backend's owner to be cancelled", |s| {
            s.platform().get_clipboard().expect("get") == "theirs"
        });
        s.platform().set_clipboard("ours again").expect("re-set");
        assert_eq!(
            s.platform().get_clipboard().expect("get"),
            "ours again",
            "a write after losing the selection must take it back"
        );
        drop(thief);
    }

    /// The compositor drops a pointer event whose timestamp did not move, so the session clock
    /// has to advance per event. A stuck clock looks like nothing at all from the outside — the
    /// events are sent, and simply do not arrive.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_event_clock_advances_across_a_drag() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Drag {
                from_x: 20,
                from_y: 20,
                to_x: 80,
                to_y: 60,
                button: glass_core::MouseButton::Left,
                modifiers: vec![],
                duration_ms: 40,
            })
            .expect("drag");
        let lines = s.wait_for_log("input: button t");
        let times = crate::testw::event_times(&lines);
        assert!(times.len() >= 3, "several events: {lines:#?}");
        assert!(
            times.windows(2).all(|w| w[1] >= w[0]),
            "the clock must never go backwards: {times:?}"
        );
        assert!(
            times.last() > times.first(),
            "and must actually advance: {times:?}"
        );
    }

    /// A launch that runs out of attempts reaps the private bus it brought up. Leaving it running
    /// leaks a dbus-daemon per failed launch, and leaves `a11y_bus_addr` handing out the address
    /// of a session that no longer exists.
    ///
    /// Not a test of the retry guard, though it looks like one: the bus is reaped whether or not
    /// the last attempt takes the retry arm, so relaxing that guard leaves this passing.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_launch_that_runs_out_of_attempts_reaps_its_private_bus() {
        let mut platform = WaylandPlatform::new().expect("sway");
        let spec = Launch::new()
            .windows(&[])
            .with_a11y()
            .timeout_ms(700)
            .spec();
        platform
            .start_app(&spec)
            .expect_err("an app with no window cannot start");
        assert_eq!(
            platform.a11y_bus_addr(),
            None,
            "the private bus outlived the launch that started it"
        );
    }

    /// A launch that asked for accessibility gets a private bus, and its address is what the
    /// reader connects to. Answering `None` sends the reader at the user's own desktop bus.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_session_launched_with_accessibility_hands_out_its_private_bus() {
        let mut s = Launch::new().with_a11y().start();
        let addr = s
            .platform()
            .a11y_bus_addr()
            .expect("an a11y launch has a bus");
        assert!(addr.contains("unix:"), "{addr}");
        assert_ne!(
            Some(addr.as_str()),
            std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
            "the session's own bus, not the developer's"
        );
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_gesture_is_refused_by_this_backend() {
        let mut s = Launch::new().start();
        let err = s
            .platform()
            .send_pointer(&PointerEvent::Gesture {
                pointers: vec![],
                duration_ms: 10,
            })
            .expect_err("no multi-touch on a desktop compositor");
        assert!(err.to_string().contains("multi_touch"), "{err}");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn typed_text_reaches_the_window_as_key_events() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_key(&KeyEvent::Text("hi".into()))
            .expect("type");
        let lines = s.wait_for_log("input: key");
        let presses = lines.iter().filter(|l| l.ends_with(" 1")).count();
        assert!(presses >= 2, "one press per character: {lines:#?}");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_chord_holds_its_modifier_across_the_key() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_key(&KeyEvent::Chord("ctrl+a".into()))
            .expect("chord");
        // An app reads ctrl+a as ctrl+a only if control is already down when the key arrives, so
        // the ordering is the claim: down, key, released.
        let lines = s.wait_for_log("input: mods 0");
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no {needle:?} in {lines:#?}"))
        };
        let (down, key, up) = (at("input: mods 4"), at("input: key"), at("input: mods 0"));
        assert!(
            down < key && key < up,
            "control must be held before the key and released after it: {lines:#?}"
        );
    }

    /// The fixture fills its surface with one known colour, so this checks pixels rather than
    /// only dimensions.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_capture_reads_the_active_windows_own_pixels() {
        let mut s = Launch::new().windows(&["cap:cap:200x160"]).start_mapped();
        let frame = s.platform().capture_frame(None).expect("capture");
        assert_eq!((frame.width, frame.height), (200, 160));
        let px = &frame.pixels[..4];
        assert_eq!(
            (px[0], px[1], px[2]),
            crate::testw::window_fill_rgb(0),
            "the window's own fill colour, not the compositor's background"
        );
        assert_eq!(px[3], 255, "opaque");
    }

    /// How long a suspended compositor stays suspended if nothing resumes it. Both budget
    /// brackets cap at 15s, so anything bounded by either is over first.
    const SUSPENSION: Duration = Duration::from_secs(30);

    /// SIGSTOPs a process until dropped: a compositor holding its connection open and answering
    /// nothing, which no sway IPC command will do.
    ///
    /// A stuck capture cannot be failed from the test thread — it is the test thread — so the
    /// resume also runs from a watchdog after [`SUSPENSION`], without which a lost bound wedges
    /// the whole suite.
    struct Suspended {
        resume: Option<std::sync::mpsc::Sender<()>>,
        watchdog: Option<std::thread::JoinHandle<()>>,
    }

    impl Suspended {
        fn process(pid: u32) -> Suspended {
            let pid = rustix::process::Pid::from_raw(pid as i32).expect("a non-zero pid");
            rustix::process::kill_process(pid, rustix::process::Signal::STOP).expect("SIGSTOP");
            let (resume, wait) = std::sync::mpsc::channel();
            let watchdog = std::thread::spawn(move || {
                // Either end of the wait resumes it: the drop below, or the timeout. The pid
                // cannot have been recycled by then — the session holds it as an unreaped child
                // until after `Drop` joins this thread.
                let _ = wait.recv_timeout(SUSPENSION);
                rustix::process::kill_process(pid, rustix::process::Signal::CONT)
                    .expect("SIGCONT the suspended compositor");
            });
            wait_until_stopped(pid);
            Suspended {
                resume: Some(resume),
                watchdog: Some(watchdog),
            }
        }
    }

    impl Drop for Suspended {
        fn drop(&mut self) {
            drop(self.resume.take());
            // Joined, so the compositor is running again before the teardown that follows asks it
            // to close. A watchdog that panicked left it stopped — worth the test, but not worth
            // aborting the process during someone else's unwind.
            if let Some(w) = self.watchdog.take() {
                let joined = w.join();
                if !std::thread::panicking() {
                    joined.expect("the watchdog resumed the compositor");
                }
            }
        }
    }

    /// Spin until the kernel reports the process stopped. `kill` returns once the signal is
    /// pending, so a compositor on another core can still service the request that follows.
    fn wait_until_stopped(pid: rustix::process::Pid) {
        let stat = format!("/proc/{}/stat", pid.as_raw_nonzero());
        for _ in 0..1000 {
            // Field 3, after a parenthesised comm field that may itself contain spaces and
            // parens — hence the split at the last ')'.
            let read = std::fs::read_to_string(&stat).expect("the compositor's /proc entry");
            let after_comm = read.rsplit_once(')').expect("a comm field").1;
            if after_comm.split_whitespace().next() == Some("T") {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the compositor never reached the stopped state");
    }

    /// glass#383: the deadline was checked only after `blocking_dispatch` returned, and that
    /// returns only when an event arrives — so a compositor that went quiet held the capture, and
    /// with it the session lock.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_capture_gives_up_on_a_compositor_that_stops_answering() {
        let mut s = Launch::new().start_mapped();
        let pid = s
            .platform()
            .session_compositor_pid()
            .expect("a started session has a compositor");
        // Declared after the session, so it resumes the compositor before teardown needs it.
        let _suspended = Suspended::process(pid);

        let started = Instant::now();
        // Not `expect_err`: on the failure this test exists to catch the capture succeeds, and
        // `Frame`'s `Debug` prints a whole window of pixels as the reason.
        let Err(err) = s.platform().capture_frame(None) else {
            panic!("the capture was not bounded: it waited the compositor out and then succeeded");
        };
        let elapsed = started.elapsed();

        assert!(
            elapsed < SUSPENSION * 2 / 3,
            "the capture waited the compositor out rather than bounding it: {elapsed:?}"
        );
        // The other end of the bracket: the fast capture tests cannot tell a 5s budget from a 5ms
        // one, so a budget cut to nothing would leave them green.
        assert!(
            elapsed >= Duration::from_secs(2),
            "the capture budget is no longer generous enough for a compositor under load: \
             {elapsed:?}"
        );
        assert!(
            err.to_string().contains("no buffer event"),
            "the capture should report what never arrived: {err}"
        );
    }

    /// The window a gesture fails in — between a press and the settle after it — cannot be hit on
    /// purpose, but the guard that puts the seat back is a `Drop`, which can be driven directly
    /// against a healthy compositor.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_gesture_dropped_after_a_press_releases_the_button() {
        use glass_core::DragSink as _;

        let mut s = Launch::new().start_mapped();
        {
            let session = s.platform().active.as_mut().expect("a started session");
            let (w, h) = session.output_size;
            let (ox, oy) = (session.active_rect.x, session.active_rect.y);
            let dispatch = WaylandDispatch::default();
            let mut sink = WaylandDragSink {
                s: session,
                dispatch: &dispatch,
                w,
                h,
                ox,
                oy,
                b: evdev_button(glass_core::MouseButton::Left),
                mask: 0,
                held: Held::default(),
                deadline: Deadline::UNBOUNDED,
            };
            // Placed first, as `run_drag` does: sway routes a button to the surface its pointer
            // is over, and re-evaluates that only on motion.
            sink.place(10, 10).expect("the placement");
            sink.button(true).expect("the press");
            // The sink drops here, as it does when `run_drag` propagates a failure.
        }

        // `272 0` is BTN_LEFT released, so the wait is the assertion — a gesture that left the
        // button down never logs it, and the harness fails rather than hangs.
        s.wait_for_log(" 272 0");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn dropped_chord_and_scroll_sinks_release_everything_they_hold() {
        use glass_core::{ChordSink as _, ScrollSink as _};

        let mut s = Launch::new().start_mapped();
        {
            let session = s.platform().active.as_mut().expect("a started session");
            let dispatch = WaylandDispatch::default();
            let mut sink = WaylandChordSink {
                s: session,
                dispatch: &dispatch,
                mask: modifier_mask(&[glass_core::keys::Modifier::Control]),
                keysym: 'a' as u32,
                held: Held::default(),
                deadline: Deadline::UNBOUNDED,
            };
            sink.modifiers(true).expect("hold chord modifier");
            sink.key(true).expect("hold chord key");
        }
        let chord = s.wait_for_log("input: mods 0");
        assert!(
            chord
                .iter()
                .any(|line| line.contains("input: key") && line.ends_with(" 0")),
            "dropping the chord released its key: {chord:#?}"
        );
        s.platform().drain_logs();

        {
            let session = s.platform().active.as_mut().expect("a started session");
            let dispatch = WaylandDispatch::default();
            let (w, h) = session.output_size;
            let (ox, oy) = (session.active_rect.x, session.active_rect.y);
            let mut sink = WaylandScrollSink {
                s: session,
                dispatch: &dispatch,
                w,
                h,
                ox,
                oy,
                x: 10,
                y: 10,
                dx: 0,
                dy: -1,
                mask: modifier_mask(&[glass_core::keys::Modifier::Control]),
                held: Held::default(),
                deadline: Deadline::UNBOUNDED,
            };
            sink.modifiers(true).expect("hold scroll modifier");
        }
        let scroll = s.wait_for_log("input: mods 0");
        assert!(
            scroll.iter().any(|line| line.ends_with("input: mods 4")),
            "the scroll modifier was held before drop: {scroll:#?}"
        );

        let session = s.platform().active.as_mut().expect("a started session");
        let dispatch = WaylandDispatch::default();
        let mut sink = WaylandChordSink {
            s: session,
            dispatch: &dispatch,
            mask: 0,
            keysym: 'a' as u32,
            held: Held::default(),
            deadline: Deadline::from_millis(0),
        };
        sink.settle()
            .expect_err("a chord settle must honor its caller deadline");
    }

    /// glass#402: waiting for the sync every request ends in was unbounded, so `glass_click` on a
    /// quiet compositor held the session lock that glass#383 had just stopped holding.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn input_gives_up_on_a_compositor_that_stops_answering() {
        let mut s = Launch::new().start_mapped();
        let pid = s
            .platform()
            .session_compositor_pid()
            .expect("a started session has a compositor");
        let _suspended = Suspended::process(pid);

        let started = Instant::now();
        let err = s
            .platform()
            .send_pointer(&PointerEvent::Move { x: 10, y: 10 })
            .expect_err("a suspended compositor cannot answer");
        let elapsed = started.elapsed();

        assert!(
            elapsed < SUSPENSION * 2 / 3,
            "the input waited the compositor out rather than bounding it: {elapsed:?}"
        );
        // The other end of the bracket: healthy syncs answer in microseconds, so a budget cut to
        // nothing would leave the suite green.
        assert!(
            elapsed >= Duration::from_secs(2),
            "the sync budget is no longer generous enough for a compositor under load: \
             {elapsed:?}"
        );
        assert!(
            err.to_string().contains("input settle"),
            "the failure should name the request that made it: {err}"
        );
    }

    /// Each path settles through its own site, so a bound reverted at one alone would leave
    /// `glass_type` or `glass_key` hanging with the rest green.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_keyboard_paths_give_up_on_a_compositor_that_stops_answering() {
        let mut s = Launch::new().start_mapped();
        let pid = s
            .platform()
            .session_compositor_pid()
            .expect("a started session has a compositor");
        let _suspended = Suspended::process(pid);

        let started = Instant::now();
        for key in [KeyEvent::Text("a".into()), KeyEvent::Chord("ctrl+a".into())] {
            let err = s
                .platform()
                .send_key(&key)
                .expect_err("a suspended compositor cannot answer");
            // Both paths upload a keymap before they press anything, so that is the settle they
            // fail in.
            assert!(
                err.to_string().contains("keymap upload"),
                "the failure should name the request that made it: {err}"
            );
        }
        assert!(
            started.elapsed() < SUSPENSION * 2 / 3,
            "a keyboard path waited the compositor out: {:?}",
            started.elapsed()
        );
    }

    /// As above for the pointer gestures, which settle through their own sinks.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_pointer_gestures_give_up_on_a_compositor_that_stops_answering() {
        let mut s = Launch::new().start_mapped();
        let pid = s
            .platform()
            .session_compositor_pid()
            .expect("a started session has a compositor");
        let _suspended = Suspended::process(pid);

        let started = Instant::now();
        let gestures = [
            PointerEvent::Drag {
                from_x: 10,
                from_y: 10,
                to_x: 40,
                to_y: 40,
                button: glass_core::MouseButton::Left,
                modifiers: Vec::new(),
                duration_ms: 50,
            },
            PointerEvent::Scroll {
                x: 10,
                y: 10,
                dx: 0,
                dy: -1,
                modifiers: Vec::new(),
            },
        ];
        for gesture in gestures {
            let err = s
                .platform()
                .send_pointer(&gesture)
                .expect_err("a suspended compositor cannot answer");
            assert!(
                err.to_string().contains("input settle"),
                "the failure should name the request that made it: {err}"
            );
        }
        assert!(
            started.elapsed() < SUSPENSION * 2 / 3,
            "a pointer gesture waited the compositor out: {:?}",
            started.elapsed()
        );
    }

    /// Enumeration reaches the compositor over the wayland connection before it asks sway IPC,
    /// which has a read timeout of its own — so the bound this needs is the wayland one.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn enumeration_gives_up_on_a_compositor_that_stops_answering() {
        let mut s = Launch::new().start_mapped();
        let pid = s
            .platform()
            .session_compositor_pid()
            .expect("a started session has a compositor");
        let _suspended = Suspended::process(pid);

        let started = Instant::now();
        let err = s
            .platform()
            .list_windows()
            .expect_err("a suspended compositor cannot answer");
        let elapsed = started.elapsed();

        assert!(
            elapsed < SUSPENSION * 2 / 3,
            "the enumeration waited the compositor out rather than bounding it: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(2),
            "the sync budget is no longer generous enough for a compositor under load: \
             {elapsed:?}"
        );
        assert!(
            err.to_string().contains("window list"),
            "the failure should name the request that made it: {err}"
        );
    }

    /// An undestroyed buffer keeps the compositor's mapping of that frame's memory resident, and
    /// a `glass_wait_stable` loop adds one per capture. Only a copy that timed out owns a buffer,
    /// so no test reaching this through `capture_frame` can cover it.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_objects_a_capture_owns_are_destroyed_with_it() {
        use wayland_client::Proxy as _;

        let mut s = Launch::new().start();
        let session = s.platform().active.as_mut().expect("a started session");
        let qh = session.queue.handle();
        let frame = session
            .manager
            .capture_output_region(0, &session.output, 0, 0, 8, 8, &qh, ());
        let mut pool = RawPool::new(8 * 4 * 8, &session.state.shm).expect("shm pool");
        let buffer = pool.create_buffer(0, 8, 8, 8 * 4, wl_shm::Format::Xrgb8888, (), &qh);
        // Clones of the same objects: a proxy reports the object's state, not its own.
        let (frame_after, buffer_after) = (frame.clone(), buffer.clone());

        drop(CaptureObjects {
            frame,
            buffer: Some(buffer),
        });

        assert!(!frame_after.is_alive(), "the frame outlived its capture");
        assert!(!buffer_after.is_alive(), "the buffer outlived its capture");
    }

    /// The frame a bounded-out capture abandons is still the compositor's to answer, into a
    /// scratch keyed by nothing. The two regions differ in size so that a stolen `buffer` event is
    /// a visible fault rather than a coincidence.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_capture_that_follows_an_abandoned_one_reads_its_own_frame() {
        let mut s = Launch::new().windows(&["cap:cap:200x160"]).start_mapped();
        let pid = s
            .platform()
            .session_compositor_pid()
            .expect("a started session has a compositor");

        let region = |width, height| Region {
            x: 0,
            y: 0,
            width,
            height,
        };
        {
            let _suspended = Suspended::process(pid);
            s.platform()
                .capture_frame(Some(&region(50, 40)))
                .expect_err("a suspended compositor cannot answer a capture");
        }

        // The compositor is running again and still owes the abandoned frame its buffer events.
        let frame = s
            .platform()
            .capture_frame(Some(&region(100, 80)))
            .expect("the compositor is answering again");
        assert_eq!(
            (frame.width, frame.height),
            (100, 80),
            "the region this capture asked for, not the one before it"
        );
        let px = &frame.pixels[..4];
        assert_eq!(
            (px[0], px[1], px[2]),
            crate::testw::window_fill_rgb(0),
            "the window's own pixels, not a buffer nothing wrote"
        );
    }

    /// A refused capture must surface as an error: a caller cannot tell a blank frame from a
    /// black window.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_capture_the_compositor_refuses_is_reported_as_a_failure() {
        let mut s = Launch::new().start_mapped();
        // Far outside the 1280x800 output, so there is nothing for screencopy to copy.
        let err = s
            .platform()
            .capture_frame(Some(&Region {
                x: 100_000,
                y: 100_000,
                width: 64,
                height: 64,
            }))
            .expect_err("a region off the output cannot be captured");
        // The message, not the variant: without the `Failed` arm this reports a timeout, which is
        // the same `CaptureFailed` and a different fault.
        assert!(
            err.to_string().contains("screencopy failed"),
            "the refusal should be reported as one, not as a timeout: {err}"
        );
    }

    /// A region is window-relative too, and it is cropped at the source: the compositor is asked
    /// for exactly that rectangle of the output.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_capture_region_is_relative_to_the_window() {
        let mut s = Launch::new().windows(&["cap:cap:200x160"]).start_mapped();
        s.platform()
            .window(&WindowOp::Move { x: 60, y: 40 })
            .expect("move");
        let frame = s
            .platform()
            .capture_frame(Some(&Region {
                x: 10,
                y: 10,
                width: 50,
                height: 40,
            }))
            .expect("capture");
        assert_eq!((frame.width, frame.height), (50, 40));
        let px = &frame.pixels[..4];
        assert_eq!(
            (px[0], px[1], px[2]),
            crate::testw::window_fill_rgb(0),
            "10px into a moved window is still inside it"
        );
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_clipboard_round_trips_through_the_compositor() {
        let mut s = Launch::new().start();
        s.platform().set_clipboard("glass wayland").expect("set");
        assert_eq!(s.platform().get_clipboard().expect("get"), "glass wayland");
    }

    /// A re-set that started a second owner without stopping the first would race it for the
    /// selection.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn writing_the_clipboard_twice_leaves_the_second_value() {
        let mut s = Launch::new().start();
        s.platform().set_clipboard("first").expect("set");
        s.platform().set_clipboard("second").expect("re-set");
        assert_eq!(s.platform().get_clipboard().expect("get"), "second");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_session_with_nothing_on_the_clipboard_reads_empty() {
        let mut s = Launch::new().start();
        assert_eq!(s.platform().get_clipboard().expect("get"), "");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_drag_presses_moves_and_releases_over_the_window() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Drag {
                from_x: 20,
                from_y: 20,
                to_x: 90,
                to_y: 70,
                button: glass_core::MouseButton::Left,
                modifiers: vec![],
                duration_ms: 40,
            })
            .expect("drag");
        let lines = s.wait_for_log("272 0");
        let order: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("input: button") || l.contains("input: motion"))
            .collect();
        let down = order
            .iter()
            .position(|l| l.contains("272 1"))
            .unwrap_or_else(|| panic!("no press in {order:#?}"));
        let up = order
            .iter()
            .position(|l| l.contains("272 0"))
            .unwrap_or_else(|| panic!("no release in {order:#?}"));
        assert!(down < up, "pressed before released: {order:?}");
        assert!(
            order[down..up].iter().any(|l| l.contains("motion")),
            "the pointer must move while the button is held: {order:?}"
        );
        assert!(
            lines.iter().any(|l| l.ends_with("90 70")),
            "and end where it was asked to: {lines:#?}"
        );
    }

    /// `glass_core::run_drag` calls `modifiers()` on every drag, so unlike the scroll sink the
    /// drag sink's `mask == 0` guard is live. Inverting it makes a *modified* drag skip its
    /// modifiers, which is what this catches.
    ///
    /// The other direction — a plain drag sending no modifier traffic — is not asserted and
    /// cannot be. The compositor emits an unsolicited `modifiers` event when the window takes
    /// keyboard focus, indistinguishable from one the sink sent, and whether it lands before or
    /// after the ready line depends on how fast the machine is.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_modified_drag_carries_its_modifiers() {
        let mut s = Launch::new().start_mapped();
        let drag = |modifiers: Vec<glass_core::keys::Modifier>| PointerEvent::Drag {
            from_x: 20,
            from_y: 20,
            to_x: 70,
            to_y: 50,
            button: glass_core::MouseButton::Left,
            modifiers,
            duration_ms: 30,
        };
        s.platform()
            .send_pointer(&drag(vec![glass_core::keys::Modifier::Control]))
            .expect("modified drag");
        let modified = s.wait_for_log("mods 0");
        assert!(
            modified.iter().any(|l| l.ends_with("mods 4")),
            "control down for a ctrl-drag: {modified:#?}"
        );
    }

    /// The horizontal axis, which every other scroll test leaves at zero — a separate branch with
    /// its own scale.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_horizontal_scroll_reaches_the_window_on_the_other_axis() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Scroll {
                x: 20,
                y: 20,
                dx: 3,
                dy: 0,
                modifiers: vec![],
            })
            .expect("scroll");
        let lines = s.wait_for_log("input: axis");
        let axis = lines
            .iter()
            .find(|l| l.contains("input: axis"))
            .expect("an axis event");
        assert!(axis.contains(" 1 "), "the horizontal axis: {axis}");
        let value: f64 = axis
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no axis value in {axis:?}"));
        assert!(
            (value - 45.0).abs() < 0.01,
            "three notches right is 45 surface units, got {value} in {axis:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("input: axis 0 ")),
            "a purely horizontal scroll must not emit a vertical axis: {lines:#?}"
        );
    }

    /// An app reads ctrl+scroll as zoom only if control is down when the axis arrives.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_modified_scroll_holds_the_modifier_and_releases_it() {
        let mut s = Launch::new().start_mapped();
        s.platform()
            .send_pointer(&PointerEvent::Scroll {
                x: 20,
                y: 20,
                dx: 0,
                dy: -1,
                modifiers: vec![glass_core::keys::Modifier::Control],
            })
            .expect("scroll");
        // Waits for the *release*, which the sink emits after the wheel — waiting for the axis
        // would assert on a line that had not arrived yet.
        let lines = s.wait_for_log("mods 0");
        let mods: Vec<&String> = lines.iter().filter(|l| l.contains("input: mods")).collect();
        assert!(
            mods.iter().any(|l| l.ends_with("mods 4")),
            "control down (XKB bit 2): {lines:#?}"
        );
        assert!(
            mods.last().is_some_and(|l| l.ends_with("mods 0")),
            "and released again: {mods:?}"
        );
    }

    /// An unmodified scroll sends no modifier traffic. `glass_core::run_scroll` is what skips it
    /// — this pins the wiring end to end, not the sink.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn an_unmodified_scroll_does_not_touch_the_modifiers() {
        let mut s = Launch::new().start_mapped();
        let before = s
            .platform()
            .drain_logs()
            .into_iter()
            .filter(|(_, l)| l.contains("input: mods"))
            .count();
        s.platform()
            .send_pointer(&PointerEvent::Scroll {
                x: 20,
                y: 20,
                dx: 0,
                dy: -1,
                modifiers: vec![],
            })
            .expect("scroll");
        let lines = s.wait_for_log("input: axis");
        assert_eq!(
            lines.iter().filter(|l| l.contains("input: mods")).count(),
            0,
            "no modifier traffic for an unmodified scroll (before: {before}): {lines:#?}"
        );
    }

    /// An app that never maps a window fails the launch with a timeout rather than hanging.
    ///
    /// It does NOT show the bring-up was retried, and nothing does. Both attempts return the same
    /// `Timeout`, and elapsed time cannot separate them because one attempt already spends the
    /// budget twice — once for the socket, once for a window. See #382.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_launch_that_finds_no_window_times_out() {
        let mut platform = WaylandPlatform::new().expect("sway");
        let spec = Launch::new().windows(&[]).timeout_ms(1500).spec();
        let err = platform
            .start_app(&spec)
            .expect_err("an app with no window cannot start");
        assert!(matches!(err, GlassError::Timeout(_)), "{err}");
    }

    /// Teardown has to happen even when nobody called `stop_app` — a panicking test or an early
    /// return would otherwise leak sway, its Xwayland and the app.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn dropping_the_backend_reaps_a_session_that_was_never_stopped() {
        let mut s = Launch::new().start();
        let pids = s.platform().app_pids();
        assert!(!pids.is_empty());
        drop(s);
        assert!(
            !glass_proc_linux::any_alive(&pids),
            "the compositor subtree outlived the backend: {pids:?}"
        );
    }

    /// After stopping there is no compositor to talk to, and the backend must say so rather than
    /// answer from what it last saw.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn stopping_ends_the_session() {
        let mut s = Launch::new().start();
        s.platform().stop_app().expect("stop");
        let err = s.platform().list_windows().expect_err("no session");
        assert!(matches!(err, GlassError::NoActiveSession), "{err}");
    }

    /// Teardown *asks* before it signals. Both routes end with the app gone, so its own shutdown
    /// path is the only witness — and a signalled app never reaches it, losing whatever it would
    /// have flushed.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_cooperative_app_is_asked_to_close_and_runs_its_own_shutdown() {
        let mut s = Launch::new().start_mapped();
        s.platform().stop_app().expect("stop");
        // Bounded rather than a single drain: the readers are separate threads, and the line is
        // still in flight when `stop_app` returns.
        s.wait_for_log(crate::testw::CLOSING_LINE);
    }

    /// An app with no shutdown path still has to be gone afterwards: the ask is followed by a
    /// signal, and the reap covers the compositor's whole group.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn an_app_that_ignores_the_close_request_is_still_reaped() {
        let mut s = Launch::new().ignoring_close().start();
        let pids = s.platform().app_pids();
        assert!(!pids.is_empty());
        s.platform().stop_app().expect("stop");
        assert!(
            !glass_proc_linux::any_alive(&pids),
            "the launch outlived its teardown: {pids:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BwrapStatusPipe, PendingWaylandSession, launch_ready, nudge_x, parse_sway_version,
        reap_pending, start_recovery_after,
    };
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    struct PendingCleanup<'a>(Option<&'a mut PendingWaylandSession>);

    impl PendingCleanup<'_> {
        fn reap(&mut self) {
            reap_pending(self.0.as_deref_mut().expect("armed cleanup guard"));
        }

        fn disarm(&mut self) {
            self.0 = None;
        }
    }

    impl Drop for PendingCleanup<'_> {
        fn drop(&mut self) {
            if let Some(pending) = self.0.as_deref_mut() {
                reap_pending(pending);
            }
        }
    }

    #[test]
    fn reap_pending_reaps_a_late_reported_separate_session_tree() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let target_pid = dir.path().join("target-pid");
        let child = Command::new("sh")
            .arg("-c")
            .arg("setsid sh -c 'sleep 30 & wait' & echo $! > \"$1\"")
            .arg("sh")
            .arg(&target_pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn compositor fixture");
        let pipe = BwrapStatusPipe::new().expect("status pipe");
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/self/fd/{}", pipe.writer_fd()))
            .expect("duplicate status writer");
        let target = (0..100)
            .find_map(|_| {
                std::fs::read_to_string(&target_pid)
                    .ok()
                    .and_then(|pid| pid.trim().parse().ok())
                    .or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        None
                    })
            })
            .expect("separate-session target pid");
        writer
            .write_all(format!("{{\"child-pid\":{target}}}\n").as_bytes())
            .expect("buffer status");
        drop(writer);
        let mut pending = PendingWaylandSession {
            child,
            status: Some(pipe.into_reader()),
            ownership_root: None,
        };
        let target_tree = glass_proc_linux::proc_tree_pids(target);
        assert!(
            target_tree.len() > 1,
            "target fixture must have a descendant: {target_tree:?}"
        );
        let mut cleanup = PendingCleanup(Some(&mut pending));

        cleanup.reap();
        cleanup.disarm();

        assert!(
            !glass_proc_linux::any_alive(&target_tree),
            "late-reported target tree survived cleanup: {target_tree:?}"
        );
    }

    #[test]
    fn launch_ready_accepts_status_and_window_just_before_deadline() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1);

        assert!(launch_ready(
            true,
            true,
            deadline,
            deadline - std::time::Duration::from_nanos(1)
        ));
    }

    #[test]
    fn launch_ready_rejects_status_and_window_observed_at_deadline() {
        let deadline = std::time::Instant::now();

        assert!(!launch_ready(true, true, deadline, deadline));
    }

    #[test]
    fn launch_ready_rejects_a_missing_status_or_window() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1);
        let observed_at = deadline - std::time::Duration::from_nanos(1);

        assert!(!launch_ready(false, true, deadline, observed_at));
        assert!(!launch_ready(true, false, deadline, observed_at));
    }

    /// A launch spends half its budget waiting for the compositor before suspecting a window was
    /// lost, so a slow app that is merely still starting is not interfered with.
    #[test]
    fn a_launch_waits_out_half_its_budget_before_looking_for_a_lost_window() {
        assert_eq!(
            start_recovery_after(10_000),
            std::time::Duration::from_millis(5_000)
        );
    }

    /// A caller's very short timeout would leave no room to look at all; one check interval is
    /// the floor, so recovery still gets one chance.
    #[test]
    fn a_short_launch_budget_still_leaves_room_for_one_check() {
        assert_eq!(
            start_recovery_after(200),
            crate::xwayland::CHECK_INTERVAL,
            "the grace must never fall below one check interval"
        );
    }

    #[test]
    fn parse_sway_version_handles_real_and_garbage() {
        assert_eq!(
            parse_sway_version("sway version 1.12-8886939 (Jun 3 2026)"),
            Some((1, 12))
        );
        assert_eq!(parse_sway_version("sway version 1.9"), Some((1, 9)));
        assert_eq!(parse_sway_version("not a version"), None);
        assert!((1u32, 12u32) >= (1, 12) && (1u32, 9u32) < (1, 12));
    }

    #[test]
    fn nudge_x_always_differs_from_target() {
        // Interior: nudge one pixel left.
        assert_eq!(nudge_x(5, 100), 4);
        assert_eq!(nudge_x(1, 100), 0);
        // Right edge stays on-output and still differs.
        assert_eq!(nudge_x(99, 100), 98);
        // Left edge (output x==0): must NOT be a no-op — nudge right instead.
        assert_eq!(nudge_x(0, 100), 1);
        // The core regression property: on any real (>=2px wide) output the
        // nudge is always a genuine motion delta, so sway re-evaluates focus.
        for w in 2..=64u32 {
            for x in 0..w {
                assert_ne!(nudge_x(x, w), x, "no-op nudge at x={x}, w={w}");
                assert!(nudge_x(x, w) < w, "nudge off-output at x={x}, w={w}");
            }
        }
    }
}
