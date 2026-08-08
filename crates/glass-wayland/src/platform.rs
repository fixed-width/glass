use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region, Result, Stream,
    TEARDOWN_BUDGET, WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use glass_proc_linux::{APP_REAP_GRACE, Asked, CLOSE_GRACE};
use smithay_client_toolkit::delegate_dispatch2;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use tempfile::TempDir;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_pointer::{Axis, ButtonState};
use wayland_client::protocol::{wl_buffer, wl_output, wl_seat, wl_shm};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use std::collections::HashMap;

use crate::command::{LogSink, build_sway_command, sway_config};
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

struct ActiveSession {
    child: Child,
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
}

/// Linux/Wayland backend (wlroots protocols, per-session headless `sway` compositor).
pub struct WaylandPlatform {
    sway: PathBuf,
    logs: LogSink,
    active: Option<ActiveSession>,
    clipboard_owner: Option<crate::clipboard::ClipboardOwner>,
    dbus: Option<glass_dbus_linux::PrivateBus>,
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
            let tree = glass_proc_linux::proc_tree_pids(s.child.id());
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
            glass_proc_linux::reap_launch(&mut s.child, &tree, glass_proc_linux::APP_REAP_GRACE);
            glass_proc_linux::disclose_teardown(&asked.outcome(closed_itself));
        }
        self.dbus = None;
    }
}

#[cfg(test)]
impl WaylandPlatform {
    /// The active session's private runtime dir — where sway put both its wayland socket and its
    /// IPC socket. Lets a test observe the session over a connection this backend does not own.
    pub(crate) fn session_runtime_dir(&self) -> Option<&Path> {
        self.active.as_ref()?.socket_path.parent()
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
        // Tear down the compositor subtree even if stop_app was never called
        // (panicking test, early return), so we never leak sway + Xwayland + app.
        self.kill_session();
    }
}

/// Find a sway ≥1.12 with no env-var config: PATH (if recent enough) → the glass
/// data dir (where the build tool installs the bundle) → next to this executable.
/// No silent fallback — a clear error if none qualifies.
pub(crate) fn resolve_sway() -> Result<PathBuf> {
    if let Some(overridden) = sway_override(std::env::var_os("GLASS_SWAY")) {
        return overridden;
    }
    // Inlined rather than wrapped: a `fn` that only splits PATH and delegates has a
    // constant-return mutation nothing can kill on a host where the true answer is that constant
    // — here, any machine with no sway on PATH.
    if let Some(p) =
        std::env::var_os("PATH").and_then(|path| sway_in_dirs(std::env::split_paths(&path)))
    {
        return Ok(p);
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(d) = data {
        let cand = d.join("glass/sway/bin/sway");
        if cand.is_file() {
            return Ok(cand);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let cand = dir.join("sway/bin/sway");
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(GlassError::Backend(
        "no sway >=1.12 found. Build it with https://github.com/fixed-width/sway-build (./build.sh && ./build.sh install), \
         or install a distro sway >=1.12."
            .into(),
    ))
}

/// What `GLASS_SWAY` decides, or `None` when it is unset or empty and discovery should run.
///
/// An explicit override wins and is trusted — it skips the version gate. It fails closed when it
/// names something that is not a file, rather than falling back to discovery: a caller who named
/// a path wants *that* sway, and silently running a different one is how a version-specific bug
/// gets chased in the wrong binary.
fn sway_override(value: Option<std::ffi::OsString>) -> Option<Result<PathBuf>> {
    let p = PathBuf::from(value.filter(|s| !s.is_empty())?);
    Some(if p.is_file() {
        Ok(p)
    } else {
        Err(GlassError::Backend(format!(
            "GLASS_SWAY={} is not an executable file",
            p.display()
        )))
    })
}

/// The first `sway` in `dirs` whose `--version` reports >= 1.12.
///
/// Only the first `sway` found is considered. A too-old or unparseable one is not skipped over in
/// favour of a later directory: `PATH` order is the user's own precedence, and walking past their
/// choice to run a different sway is the same silent substitution [`sway_override`] refuses.
fn sway_in_dirs(dirs: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    for dir in dirs {
        let cand = dir.join("sway");
        if !cand.is_file() {
            continue;
        }
        let out = std::process::Command::new(&cand)
            .arg("--version")
            .output()
            .ok()?;
        let ver = String::from_utf8_lossy(&out.stdout);
        return match parse_sway_version(&ver) {
            Some((maj, min)) if (maj, min) >= (1, 12) => Some(cand),
            _ => None, // a sway is on PATH but too old/unparseable -> use the bundle
        };
    }
    None
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

/// SCTK state: registry + output (for the output extent), shm (for capture
/// buffers), and the per-capture wlr-screencopy scratch (reset before each
/// capture). Window enumeration is via sway IPC, not foreign-toplevel.
struct State {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    shm_buffers: Vec<(wl_shm::Format, u32, u32, u32)>, // advertised formats (format, w, h, stride)
    buffer_done: bool,                                 // v3: end of the format advertisement list
    capture_done: Option<Result<()>>,                  // Some(Ok)=ready, Some(Err)=failed
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
                state.shm_buffers.push((f, width, height, stride));
            }
            Event::BufferDone => state.buffer_done = true,
            Event::Ready { .. } => state.capture_done = Some(Ok(())),
            Event::Failed => {
                state.capture_done =
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
    let stream = UnixStream::connect(socket)
        .map_err(|e| GlassError::Backend(format!("connect to wayland socket: {e}")))?;
    let conn = Connection::from_socket(stream)
        .map_err(|e| GlassError::Backend(format!("wayland connection: {e}")))?;
    let (globals, mut queue): (_, EventQueue<State>) = registry_queue_init(&conn)
        .map_err(|e| GlassError::Backend(format!("wayland registry: {e}")))?;

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
        shm_buffers: Vec::new(),
        buffer_done: false,
        capture_done: None,
    };
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|e| GlassError::Backend(format!("bind screencopy: {e}")))?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| GlassError::Backend(format!("bind seat: {e}")))?;
    let vp_manager: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| GlassError::Backend(format!("bind virtual pointer: {e}")))?;
    let vk_manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| GlassError::Backend(format!("bind virtual keyboard: {e}")))?;

    queue
        .roundtrip(&mut state)
        .map_err(|e| GlassError::Backend(format!("wayland roundtrip: {e}")))?;

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
    let deadline = Instant::now() + Duration::from_millis(2000);
    let ipc = loop {
        match Ipc::connect(runtime_dir) {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(40)),
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
) -> Result<(ActiveSession, WindowGeometry)> {
    let runtime_dir = tempfile::Builder::new()
        .prefix("glass-wl.")
        .tempdir()
        .map_err(GlassError::Io)?;

    let config = runtime_dir.path().join("sway.cfg");
    std::fs::write(
        &config,
        sway_config(spec, runtime_dir.path(), a11y.map(|a| a.dir)),
    )
    .map_err(GlassError::Io)?;
    let mut cmd = build_sway_command(
        sway,
        &config,
        spec,
        runtime_dir.path(),
        a11y.map(|a| a.addr),
    );
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| GlassError::AppNotStarted(format!("spawn sway: {e}")))?;
    if let Some(out) = child.stdout.take() {
        glass_proc_linux::spawn_reader(out, Stream::Stdout, logs.clone());
    }
    if let Some(err) = child.stderr.take() {
        glass_proc_linux::spawn_reader(err, Stream::Stderr, logs.clone());
    }

    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.max(1));
    let socket = loop {
        if let Some(s) = find_wayland_socket(runtime_dir.path()) {
            break s;
        }
        if let Ok(Some(status)) = child.try_wait() {
            // sway exited — but on an *unclean* exit its group children
            // (Xwayland + the exec'd app) can outlive it. Reap the whole
            // group, not just the leader, or a leaked Xwayland holds the X
            // display in the global namespace and breaks the next session.
            glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
            return Err(GlassError::app_exited_during_discovery(
                status.code(),
                spec.sandbox,
            ));
        }
        if Instant::now() >= deadline {
            glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
            return Err(GlassError::Timeout(spec.timeout_ms));
        }
        std::thread::sleep(Duration::from_millis(40));
    };

    let (conn, mut queue, mut state, manager, output, pointer, keyboard, mut ipc, output_size) =
        match open_session(&socket, runtime_dir.path()) {
            Ok(v) => v,
            Err(e) => {
                glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
                return Err(e);
            }
        };
    let socket_path = socket;

    // Discover the initially-focused window (the app's first toplevel), so
    // capture/input have an active target before the first list_windows.
    let mut ids: HashMap<String, WindowId> = HashMap::new();
    let mut next_id = 0u64;
    let mut recovery = crate::xwayland::Recovery::new(runtime_dir.path());
    let (active, active_rect) = {
        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.max(1));
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
        loop {
            let _ = queue.roundtrip(&mut state); // keep the wayland queue serviced
            // Distinguish "sway says no windows" from "sway did not answer": an unanswered
            // request is not evidence the app has no windows, and feeding that emptiness to the
            // cross-check below would make every window the app really has look lost.
            let listed = ipc.windows();
            let wins = listed.as_deref().unwrap_or_default();
            if let Some(w) = wins.iter().find(|w| w.focused).or_else(|| wins.first()) {
                mint_id(&mut ids, &mut next_id, &w.identifier);
                break (Some(w.identifier.clone()), rect_to_geom(&w.rect));
            }
            let now = Instant::now();
            if now >= start_grace && listed.is_ok() {
                recovery.recover_if_due(now, &x11_ids(wins));
            }
            if let Ok(Some(status)) = child.try_wait() {
                // Reap the whole group (see the socket-wait loop above): an
                // unclean sway exit can orphan Xwayland + the app otherwise.
                glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
                return Err(GlassError::app_exited_during_discovery(
                    status.code(),
                    spec.sandbox,
                ));
            }
            if Instant::now() >= deadline {
                glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
                // Say what glass saw. A launch that gives up after re-mapping a window the app
                // really had is a different problem from an app that never opened one, and a
                // bare timeout would send the reader looking at the app.
                let unrecovered = recovery.unrecovered();
                if unrecovered > 0 {
                    return Err(GlassError::Backend(format!(
                        "the app mapped {unrecovered} X11 window(s) the compositor never \
                         surfaced; glass re-mapped them and they still did not appear within \
                         {}ms. The session's Xwayland may be wedged — retry the launch.",
                        spec.timeout_ms
                    )));
                }
                return Err(GlassError::Timeout(spec.timeout_ms));
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    };
    // The caller's first enumeration must cross-check rather than fall inside the interval this
    // discovery loop already spent.
    recovery.rearm();
    let geometry = active_rect.clone();
    let session = ActiveSession {
        child,
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
    };
    Ok((session, geometry))
}

/// Write the keymap to an unlinked temp file and hand its fd to the compositor,
/// then settle so Xwayland adopts the new mapping before any key events. No
/// unsafe: tempfile gives a normal, mmap-able fd; XKB_V1 format == 1.
fn upload_keymap(s: &mut ActiveSession, kb: &ZwpVirtualKeyboardV1, keymap: &str) -> Result<()> {
    let mut f = tempfile::tempfile().map_err(GlassError::Io)?;
    f.write_all(keymap.as_bytes()).map_err(GlassError::Io)?;
    f.write_all(&[0]).map_err(GlassError::Io)?; // keymap string is NUL-terminated
    f.flush().map_err(GlassError::Io)?;
    kb.keymap(1, f.as_fd(), keymap.len() as u32 + 1);
    s.queue
        .roundtrip(&mut s.state)
        .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
    std::thread::sleep(Duration::from_millis(8));
    Ok(())
}

/// Press then release evdev keycode `kc`, bumping the session clock per event and
/// self-committing (roundtrip + settle) after each — so the compositor processes the
/// press/release individually, like the chord sink. A heavy client (e.g. a browser) ignores
/// taps that are merely queued and flushed once at the end.
fn tap(s: &mut ActiveSession, kb: &ZwpVirtualKeyboardV1, kc: u32) -> Result<()> {
    for state in [1u32, 0] {
        s.time = s.time.wrapping_add(1);
        kb.key(s.time, kc, state);
        s.queue
            .roundtrip(&mut s.state)
            .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
        std::thread::sleep(Duration::from_millis(8));
    }
    Ok(())
}

/// Fail closed: a launch that asked for a sandbox errors rather than running unconfined.
///
/// `probe` is a thunk, not a value. Rust evaluates arguments before the call, so passing
/// `availability()` directly would fork `bwrap --unshare-user` on *every* launch — including
/// `sandbox:"off"`, the one setting that exists for machines where bubblewrap does not work.
/// glass-x11 has the same helper for the same reason; keeping a copy in each backend keeps both
/// inside the mutation gate's `--package` list.
fn ensure_sandbox_available(
    level: glass_core::SandboxLevel,
    probe: impl FnOnce() -> glass_sandbox_linux::Availability,
) -> Result<()> {
    if level == glass_core::SandboxLevel::Off {
        return Ok(());
    }
    match probe() {
        glass_sandbox_linux::Availability::Ok => Ok(()),
        glass_sandbox_linux::Availability::Unavailable(why) => {
            Err(GlassError::SandboxUnavailable(format!(
                "{why}. Install bubblewrap / enable unprivileged user namespaces, or pass \
                 sandbox:\"off\" (GLASS_SANDBOX=off) to run unconfined. See `glass-mcp doctor`."
            )))
        }
    }
}

/// XKB real-modifier mask for a chord's modifiers (standard `include "complete"`
/// order: Shift, Lock, Control, Mod1=Alt, ..., Mod4=Super).
///
/// The bits are written out rather than shifted. `1 << 0` and `1 >> 0` are the same number, so a
/// shift here is a place the code can be changed without any test being able to notice.
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
    w: u32,
    h: u32,
    ox: i32,
    oy: i32,
    b: u32,
    mask: u32,
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
        self.s
            .queue
            .roundtrip(&mut self.s.state)
            .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
        std::thread::sleep(Duration::from_millis(8));
        Ok(())
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
        self.settle()?;
        let t = self.tick();
        vp.motion_absolute(t, nudge_x(axx, w), ayy, w, h);
        vp.frame();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
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
        self.settle()
    }
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if self.mask == 0 {
            return Ok(());
        }
        let kb = self.s.keyboard.clone();
        if down {
            upload_keymap(&mut *self.s, &kb, &crate::keyboard::build_keymap(&[]))?;
            kb.modifiers(self.mask, 0, 0, 0);
        } else {
            kb.modifiers(0, 0, 0, 0);
        }
        // Self-commit so the modifier change reaches the compositor before the
        // press/release that follows it (matches the X11 sink's flush-per-call).
        self.settle()
    }
}

/// Lets `glass_core::run_chord` drive a Wayland key chord through the virtual keyboard. The keymap
/// (with the chord's key as keycode 1) is uploaded and the modifier mask set in `modifiers(true)`;
/// each method self-commits (roundtrip + 8ms settle) so the modifier is held across the key's frame.
struct WaylandChordSink<'a> {
    s: &'a mut ActiveSession,
    mask: u32,
    keysym: u32,
}

impl WaylandChordSink<'_> {
    fn settle(&mut self) -> Result<()> {
        self.s
            .queue
            .roundtrip(&mut self.s.state)
            .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
        std::thread::sleep(Duration::from_millis(8));
        Ok(())
    }
}

impl glass_core::ChordSink for WaylandChordSink<'_> {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        let kb = self.s.keyboard.clone();
        if down {
            // Upload the keymap (chord key = keycode 1) regardless of mask, then set the modifiers.
            upload_keymap(
                &mut *self.s,
                &kb,
                &crate::keyboard::build_keymap(&[self.keysym]),
            )?;
            if self.mask != 0 {
                kb.modifiers(self.mask, 0, 0, 0);
            }
        } else if self.mask != 0 {
            kb.modifiers(0, 0, 0, 0);
        }
        self.settle()
    }
    fn key(&mut self, down: bool) -> Result<()> {
        let kb = self.s.keyboard.clone();
        self.s.time = self.s.time.wrapping_add(1);
        kb.key(self.s.time, 1, u32::from(down)); // keycode 1 = the chord's key; 1=pressed, 0=released
        self.settle()
    }
}

/// Lets `glass_core::run_scroll` drive a Wayland scroll through the virtual pointer + keyboard. The
/// modifier mask is set in `modifiers(true)` and cleared in `modifiers(false)`; `wheel` positions the
/// pointer (with the focus-reassert nudge, like the drag sink) then emits the vertical and horizontal
/// axis. Each method self-commits (frame + roundtrip + 8ms settle) so the modifier is held across the
/// wheel's frame.
struct WaylandScrollSink<'a> {
    s: &'a mut ActiveSession,
    w: u32,
    h: u32,
    ox: i32,
    oy: i32,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    mask: u32,
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
        self.s
            .queue
            .roundtrip(&mut self.s.state)
            .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
        std::thread::sleep(Duration::from_millis(8));
        Ok(())
    }
}

impl glass_core::ScrollSink for WaylandScrollSink<'_> {
    fn modifiers(&mut self, down: bool) -> Result<()> {
        if self.mask == 0 {
            return Ok(());
        }
        let kb = self.s.keyboard.clone();
        if down {
            upload_keymap(&mut *self.s, &kb, &crate::keyboard::build_keymap(&[]))?;
            kb.modifiers(self.mask, 0, 0, 0);
        } else {
            kb.modifiers(0, 0, 0, 0);
        }
        self.settle()
    }
    fn wheel(&mut self) -> Result<()> {
        let vp = self.s.pointer.clone();
        let (w, h) = (self.w, self.h);
        let (axx, ayy) = (self.ax(self.x), self.ay(self.y));
        // Position with the focus-reassert nudge (sway re-evaluates pointer focus only on motion).
        let t = self.tick();
        vp.motion_absolute(t, axx, ayy, w, h);
        vp.frame();
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
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
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
            match bring_up_session(&self.sway, &self.logs, spec, a11y) {
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

    fn stop_app(&mut self) -> Result<()> {
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

    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
        session.state.shm_buffers.clear();
        session.state.buffer_done = false;
        session.state.capture_done = None;
        let qh = session.queue.handle();

        // Map the (window-relative) request to OUTPUT coordinates by the active
        // window's rect, then have the compositor copy exactly that region. The
        // selected window is raised on `select_window`, so the output framebuffer
        // shows it on top; cropping at the source needs no CPU work and reads the
        // existing framebuffer (robust for static, undamaged windows — unlike
        // per-toplevel ext-image-copy-capture, which stalls until a fresh frame).
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

        let deadline = Instant::now() + Duration::from_millis(5000);

        // Phase 1: dispatch until the compositor has advertised its buffer formats, then pick
        // one we can convert (preferring 32-bit). v3 marks the end of the format list with
        // `buffer_done`; v1/v2 advertise a single format and never send it, so there we proceed
        // as soon as one arrives.
        let manager_v3 = session.manager.version() >= 3;
        let (format, w, h, stride) = loop {
            session
                .queue
                .blocking_dispatch(&mut session.state)
                .map_err(|e| GlassError::CaptureFailed(format!("dispatch: {e}")))?;
            let advertised = if manager_v3 {
                session.state.buffer_done
            } else {
                !session.state.shm_buffers.is_empty()
            };
            if advertised {
                break crate::pixels::pick_shm_format(&session.state.shm_buffers).ok_or_else(
                    || GlassError::CaptureFailed("screencopy: no shm format advertised".into()),
                )?;
            }
            if let Some(Err(e)) = session.state.capture_done.take() {
                return Err(e);
            }
            if Instant::now() >= deadline {
                return Err(GlassError::CaptureFailed(
                    "screencopy: no buffer event".into(),
                ));
            }
        };

        // Allocate a matching shm buffer and request the copy.
        let mut pool = RawPool::new((stride * h) as usize, &session.state.shm)
            .map_err(|e| GlassError::CaptureFailed(format!("shm pool: {e}")))?;
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, (), &qh);
        frame.copy(&buffer);

        // Phase 2: dispatch until ready/failed.
        loop {
            session
                .queue
                .blocking_dispatch(&mut session.state)
                .map_err(|e| GlassError::CaptureFailed(format!("dispatch: {e}")))?;
            if let Some(done) = session.state.capture_done.take() {
                done?;
                break;
            }
            if Instant::now() >= deadline {
                return Err(GlassError::CaptureFailed("screencopy timed out".into()));
            }
        }

        // The captured buffer already matches the requested region, so no CPU crop.
        let rgba = crate::pixels::to_rgba(pool.mmap(), format, w, h, stride)?;
        Frame::new(w, h, rgba)
    }

    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
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
        let settle = |q: &mut EventQueue<State>, s: &mut State| -> Result<()> {
            q.roundtrip(s)
                .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
            std::thread::sleep(Duration::from_millis(8));
            Ok(())
        };
        // Position the pointer at a window-relative point so the *next* button/axis
        // routes to the window under it. sway (re)evaluates pointer focus only on
        // motion, never on elapsed time: a surface that maps and settles under a
        // now-stationary cursor never receives `enter`, and a one-shot button/axis
        // sent to it is then silently dropped. So move there, let the surface settle,
        // then re-assert with a 1px delta to force a fresh focus evaluation now that
        // it is ready. Without this, fast back-to-back launch+click on a loaded host
        // intermittently loses the very first click/scroll (the Wayland flake).
        let position = |q: &mut EventQueue<State>, s: &mut State, x: i32, y: i32| -> Result<()> {
            vp.motion_absolute(t, ax(x), ay(y), w, h);
            vp.frame();
            settle(q, s)?;
            vp.motion_absolute(t, nudge_x(ax(x), w), ay(y), w, h);
            vp.frame();
            vp.motion_absolute(t, ax(x), ay(y), w, h);
            vp.frame();
            settle(q, s)
        };
        match *event {
            PointerEvent::Move { x, y } => {
                position(&mut session.queue, &mut session.state, x, y)?;
            }
            PointerEvent::Click {
                x,
                y,
                button,
                count,
                ref modifiers,
            } => {
                position(&mut session.queue, &mut session.state, x, y)?;
                let mask = modifier_mask(modifiers);
                if mask != 0 {
                    upload_keymap(session, &kb, &crate::keyboard::build_keymap(&[]))?;
                    kb.modifiers(mask, 0, 0, 0);
                }
                let b = evdev_button(button);
                for _ in 0..count.max(1) {
                    vp.button(t, b, ButtonState::Pressed);
                    vp.frame();
                    settle(&mut session.queue, &mut session.state)?;
                    vp.button(t, b, ButtonState::Released);
                    vp.frame();
                    settle(&mut session.queue, &mut session.state)?;
                }
                if mask != 0 {
                    kb.modifiers(0, 0, 0, 0);
                }
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
                    w,
                    h,
                    ox,
                    oy,
                    b: evdev_button(button),
                    mask: modifier_mask(modifiers),
                };
                glass_core::run_drag(&mut sink, &gesture)?;
            }
            PointerEvent::Scroll {
                x,
                y,
                dx,
                dy,
                ref modifiers,
            } => {
                // Shared, frame-aware sequencing: hold the modifier across the wheel's frame instead
                // of bursting modifier+wheel+release into one — see glass_core::run_scroll.
                let mut sink = WaylandScrollSink {
                    s: &mut *session,
                    w,
                    h,
                    ox,
                    oy,
                    x,
                    y,
                    dx,
                    dy,
                    mask: modifier_mask(modifiers),
                };
                glass_core::run_scroll(&mut sink, !modifiers.is_empty())?;
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
    }
    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        use glass_core::keys::parse_chord;
        let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
        let kb = session.keyboard.clone();
        match event {
            KeyEvent::Text(text) => {
                // Upload one keymap per chunk (each distinct keysym at its own keycode), then
                // tap the planned keycode for every character. The keymap stays fixed while a
                // chunk's key events are delivered, so a client that resolves keysyms lazily
                // (e.g. an X11 app under Xwayland querying the keymap per press) can't read a
                // neighbouring character by racing a mid-string keymap swap — the flake that a
                // fresh keymap on the *same* keycode per character exhibited under load. Each
                // tap self-commits (roundtrip + settle), which also paces a heavy client; the
                // one keymap upload per chunk replaces one upload per character. See
                // crate::keyboard::plan_type.
                for chunk in crate::keyboard::plan_type(text) {
                    upload_keymap(
                        &mut *session,
                        &kb,
                        &crate::keyboard::build_keymap(&chunk.keysyms),
                    )?;
                    for kc in chunk.taps {
                        tap(&mut *session, &kb, kc)?;
                    }
                }
            }
            KeyEvent::Chord(c) => {
                let (mods, keysym) = parse_chord(c)?; // validates before any traffic
                let mut sink = WaylandChordSink {
                    s: &mut *session,
                    mask: modifier_mask(&mods),
                    keysym,
                };
                glass_core::run_chord(&mut sink)?;
            }
        }
        session
            .conn
            .flush()
            .map_err(|e| GlassError::Backend(format!("flush: {e}")))?;
        Ok(())
    }

    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
        let ident = session.active.clone().ok_or(GlassError::WindowNotFound)?;
        // All window ops act on the active window's sway container. Windows are
        // floating (see sway_config), so resize/move behave like a normal WM.
        let con = session
            .ipc
            .windows()?
            .into_iter()
            .find(|w| w.identifier == ident)
            .map(|w| w.con_id)
            .ok_or(GlassError::WindowNotFound)?;
        match *op {
            WindowOp::Geometry => {}
            WindowOp::Focus => session.ipc.run_command(&format!("[con_id={con}] focus"))?,
            WindowOp::Resize { width, height } => session.ipc.run_command(&format!(
                "[con_id={con}] resize set width {width} px height {height} px"
            ))?,
            // Move's (x, y) is an output-absolute origin, matching the X11 backend
            // (root coordinates); the headless output is at (0, 0).
            WindowOp::Move { x, y } => session
                .ipc
                .run_command(&format!("[con_id={con}] move absolute position {x} {y}"))?,
        }
        // Re-read the resulting rect (sway may clamp) and refresh the session
        // contract — active_rect drives the capture crop and pointer offset.
        let now = session
            .ipc
            .windows()?
            .into_iter()
            .find(|w| w.identifier == ident)
            .ok_or(GlassError::WindowNotFound)?;
        let geo = rect_to_geom(&now.rect);
        session.active_rect = geo.clone();
        session.geometry = geo.clone();
        Ok(geo)
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
        // Refresh foreign-toplevel handles so capture can later find them.
        session
            .queue
            .roundtrip(&mut session.state)
            .map_err(|e| GlassError::Backend(format!("roundtrip: {e}")))?;
        let mut wins: Vec<SwayWindow> = session.ipc.windows()?;
        // A window the app mapped can be missing here through no fault of the app (see
        // `crate::xwayland`). Enumerating is where that shows up, so it is where glass repairs
        // it — otherwise the caller is told the app has fewer windows than it does.
        if session
            .recovery
            .recover_if_due(Instant::now(), &x11_ids(&wins))
            > 0
        {
            std::thread::sleep(crate::xwayland::REMAP_SETTLE);
            wins = session.ipc.windows()?;
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
    }

    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        let session = self.active.as_mut().ok_or(GlassError::NoActiveSession)?;
        let wins = session.ipc.windows()?;
        let target = wins
            .into_iter()
            .find(|w| session.ids.get(&w.identifier) == Some(&id))
            .ok_or(GlassError::WindowNotFound)?;
        session
            .ipc
            .run_command(&format!("[con_id={}] focus", target.con_id))?;
        // Confirm the focus moved (no silent fallback).
        let after = session.ipc.windows()?;
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
            Some(s) => glass_proc_linux::proc_tree_pids(s.child.id()),
            None => Vec::new(),
        }
    }

    fn a11y_bus_addr(&self) -> Option<String> {
        self.dbus.as_ref().map(|b| b.a11y_bus_address().to_string())
    }
}

#[cfg(test)]
mod pure_tests {
    use super::*;

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

    /// The cross-check compares the X server's toplevels against these, so a native Wayland view
    /// must be absent rather than present as some placeholder id that could collide with a real
    /// X window.
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

    /// A directory holding a `sway` that answers `--version` with `reply`.
    fn fake_sway(reply: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("sway");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{reply}'\n")).expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        dir
    }

    #[test]
    fn a_recent_sway_on_the_path_is_used() {
        let dir = fake_sway("sway version 1.12-abc (Jun 3 2026)");
        assert_eq!(
            sway_in_dirs([dir.path().to_path_buf()].into_iter()),
            Some(dir.path().join("sway"))
        );
    }

    /// Too old, or a version this cannot read, means fall through to the bundle — glass drives
    /// sway through IPC and protocol surface it only has from 1.12.
    #[test]
    fn an_old_or_unreadable_sway_on_the_path_is_not_used() {
        for reply in ["sway version 1.9", "sway version 1.11-x", "wat"] {
            let dir = fake_sway(reply);
            assert_eq!(
                sway_in_dirs([dir.path().to_path_buf()].into_iter()),
                None,
                "{reply:?}"
            );
        }
    }

    #[test]
    fn a_path_with_no_sway_on_it_finds_nothing() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(sway_in_dirs([empty.path().to_path_buf()].into_iter()), None);
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
        assert!(err.to_string().contains("/nonexistent/sway"), "{err}");
    }

    #[test]
    fn discovery_finds_a_real_sway_on_this_machine() {
        let found = resolve_sway().expect("a discoverable sway");
        assert!(found.is_file(), "{}", found.display());
    }

    /// The only way to assert something is *not* called: a probe that panics if it is. Testing
    /// the extracted function alone proves nothing about the call site, which is where an eager
    /// argument would change the behaviour.
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
        assert!(err.to_string().contains("no bwrap here"), "{err}");
        assert!(err.to_string().contains("sandbox:\"off\""), "{err}");
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

    /// The XKB real-modifier bits, in the order `include "complete"` assigns them. Each is a
    /// distinct bit: an `&`/`^` fold or a shift the wrong way would collapse or move them.
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
    use super::*;
    use crate::testw::{Launch, READY_LINE};

    #[test]
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
    fn selecting_a_window_that_is_not_there_reports_it_not_found() {
        let mut s = Launch::new().start();
        let err = s
            .platform()
            .select_window(WindowId(4242))
            .expect_err("no such window");
        assert!(matches!(err, GlassError::WindowNotFound), "{err}");
    }

    #[test]
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

    /// Logs are the app's stdout and stderr as the launch captured them, not the compositor's.
    #[test]
    fn the_apps_output_reaches_the_log_sink() {
        let mut s = Launch::new().start();
        let lines = s.wait_for_log(READY_LINE);
        assert!(lines.iter().any(|l| l.contains(READY_LINE)), "{lines:#?}");
    }

    /// The a11y reader correlates an AT-SPI connection against this set, so it has to reach past
    /// the compositor to the app: sway's pid is not the app's.
    #[test]
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
    fn a_session_launched_without_accessibility_has_no_bus_address() {
        let mut s = Launch::new().start();
        assert_eq!(s.platform().a11y_bus_addr(), None);
    }

    /// Coordinates are window-relative at glass's boundary and the backend maps them to the
    /// output. The app is the only witness: Wayland has no way to ask where the pointer is, so
    /// the fixture echoes the surface-local point it was given back through its own stdout.
    #[test]
    fn a_pointer_move_arrives_at_the_requested_window_relative_point() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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
    fn a_pointer_move_is_relative_to_a_window_that_is_not_at_the_origin() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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
    fn a_click_presses_and_releases_the_button_over_the_window() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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
            buttons.iter().any(|l| l.contains("272 1")),
            "left button pressed: {buttons:?}"
        );
        assert!(
            buttons.iter().any(|l| l.contains("272 0")),
            "and released: {buttons:?}"
        );
    }

    #[test]
    fn a_scroll_reaches_the_window_as_an_axis_event() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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
        let times: Vec<u32> = lines
            .iter()
            .filter_map(|l| l.split_whitespace().find(|w| w.starts_with('t')))
            .filter_map(|t| t[1..].parse().ok())
            .collect();
        assert!(
            times.last() > times.first(),
            "the clock must advance across the scroll: {times:?}"
        );
    }

    /// Once another client takes the selection this owner's thread is done, and a second write
    /// has to start a fresh one. Updating the dead thread's text instead leaves the other
    /// client's value on the clipboard while glass reports the write as done.
    #[test]
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
    fn the_event_clock_advances_across_a_drag() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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
        let times: Vec<u32> = lines
            .iter()
            .filter_map(|l| l.split_whitespace().find(|w| w.starts_with('t')))
            .filter_map(|t| t[1..].parse().ok())
            .collect();
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
    fn typed_text_reaches_the_window_as_key_events() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
        s.platform()
            .send_key(&KeyEvent::Text("hi".into()))
            .expect("type");
        let lines = s.wait_for_log("input: key");
        let presses = lines.iter().filter(|l| l.ends_with(" 1")).count();
        assert!(presses >= 2, "one press per character: {lines:#?}");
    }

    #[test]
    fn a_chord_holds_its_modifier_across_the_key() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
        s.platform()
            .send_key(&KeyEvent::Chord("ctrl+a".into()))
            .expect("chord");
        let lines = s.wait_for_log("input: key");
        assert!(
            lines.iter().any(|l| l.contains("input: mods 4")),
            "control held (XKB bit 2): {lines:#?}"
        );
        assert!(lines.iter().any(|l| l.contains("input: key")), "{lines:#?}");
    }

    /// The fixture fills its surface with one known colour, so the capture can be checked pixel
    /// for pixel rather than only for its dimensions.
    #[test]
    fn a_capture_reads_the_active_windows_own_pixels() {
        let mut s = Launch::new().windows(&["cap:cap:200x160"]).start();
        s.wait_for_log(READY_LINE);
        let frame = s.platform().capture_frame(None).expect("capture");
        assert_eq!((frame.width, frame.height), (200, 160));
        let px = &frame.pixels[..4];
        assert_eq!(
            (px[0], px[1], px[2]),
            (0x33, 0x11, 0x22),
            "the window's own fill colour, not the compositor's background"
        );
        assert_eq!(px[3], 255, "opaque");
    }

    /// A region is window-relative too, and it is cropped at the source: the compositor is asked
    /// for exactly that rectangle of the output.
    #[test]
    fn a_capture_region_is_relative_to_the_window() {
        let mut s = Launch::new().windows(&["cap:cap:200x160"]).start();
        s.wait_for_log(READY_LINE);
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
            (0x33, 0x11, 0x22),
            "10px into a moved window is still inside it"
        );
    }

    #[test]
    fn the_clipboard_round_trips_through_the_compositor() {
        let mut s = Launch::new().start();
        s.platform().set_clipboard("glass wayland").expect("set");
        assert_eq!(s.platform().get_clipboard().expect("get"), "glass wayland");
    }

    /// A second write replaces the first. The owner is a live thread serving the selection, so a
    /// re-set that started a second owner without stopping the first would race.
    #[test]
    fn writing_the_clipboard_twice_leaves_the_second_value() {
        let mut s = Launch::new().start();
        s.platform().set_clipboard("first").expect("set");
        s.platform().set_clipboard("second").expect("re-set");
        assert_eq!(s.platform().get_clipboard().expect("get"), "second");
    }

    #[test]
    fn a_session_with_nothing_on_the_clipboard_reads_empty() {
        let mut s = Launch::new().start();
        assert_eq!(s.platform().get_clipboard().expect("get"), "");
    }

    #[test]
    fn a_drag_presses_moves_and_releases_over_the_window() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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

    /// A modifier is held across the wheel, not sent alongside it: an app reads ctrl+scroll as
    /// zoom only if control is down when the axis arrives.
    #[test]
    fn a_modified_scroll_holds_the_modifier_and_releases_it() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
        s.platform()
            .send_pointer(&PointerEvent::Scroll {
                x: 20,
                y: 20,
                dx: 0,
                dy: -1,
                modifiers: vec![glass_core::keys::Modifier::Control],
            })
            .expect("scroll");
        let lines = s.wait_for_log("input: axis");
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

    /// An unmodified scroll must not upload a keymap or touch the modifier state at all — that
    /// is what the `mask == 0` short circuit is for.
    #[test]
    fn an_unmodified_scroll_does_not_touch_the_modifiers() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
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

    /// An app that never maps a window is retried before the launch gives up: the compositor
    /// bring-up is the flaky part, and one attempt would surface that as the app's fault.
    #[test]
    fn a_launch_that_finds_no_window_is_retried_before_it_gives_up() {
        let mut platform = WaylandPlatform::new().expect("sway");
        let spec = Launch::new().windows(&[]).timeout_ms(700).spec();
        let start = Instant::now();
        let err = platform
            .start_app(&spec)
            .expect_err("an app with no window cannot start");
        let elapsed = start.elapsed();
        assert!(matches!(err, GlassError::Timeout(_)), "{err}");
        assert!(
            elapsed >= Duration::from_millis(1400),
            "one 700ms budget was spent, not two — the launch was not retried ({elapsed:?})"
        );
    }

    /// Teardown has to happen even when nobody called `stop_app` — a panicking test or an early
    /// return would otherwise leak sway, its Xwayland and the app.
    #[test]
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

    /// Stopping ends the session: what follows has no compositor to talk to and must say so
    /// rather than answering from whatever the backend last saw.
    #[test]
    fn stopping_ends_the_session() {
        let mut s = Launch::new().start();
        s.platform().stop_app().expect("stop");
        let err = s.platform().list_windows().expect_err("no session");
        assert!(matches!(err, GlassError::NoActiveSession), "{err}");
    }

    /// Teardown *asks* the app to close before it signals anything. Both routes end with the app
    /// gone, so the end state cannot tell them apart — the app's own shutdown path is the only
    /// witness, and a signalled app never reaches it. This is the difference between an app that
    /// flushes its state on the way out and one that reports a crash on its next launch.
    #[test]
    fn a_cooperative_app_is_asked_to_close_and_runs_its_own_shutdown() {
        let mut s = Launch::new().start();
        s.wait_for_log(READY_LINE);
        s.platform().stop_app().expect("stop");
        let said: Vec<String> = s
            .platform()
            .drain_logs()
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert!(
            said.iter().any(|l| l.contains(crate::testw::CLOSING_LINE)),
            "the app was signalled, not asked: {said:#?}"
        );
    }

    /// An app with no shutdown path still has to be gone afterwards: the ask is followed by a
    /// signal, and the reap covers the compositor's whole group.
    #[test]
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
    use super::{nudge_x, parse_sway_version, start_recovery_after};

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
