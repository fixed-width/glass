use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region, Result, Stream,
    WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use smithay_client_toolkit::delegate_output;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::delegate_shm;
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

use crate::command::{build_sway_command, sway_config, LogSink};
use crate::input::evdev_button;
use crate::swayipc::{Ipc, Window as SwayWindow};

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
    active: Option<String>,      // active window's foreign-toplevel identifier
    active_rect: WindowGeometry, // active window's output rect (capture/input origin)
    geometry: WindowGeometry,    // active window geometry (session contract)
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

    fn kill_session(&mut self) {
        // Tear down the clipboard owner thread before the wayland socket disappears.
        if let Some(owner) = self.clipboard_owner.take() {
            owner.stop();
        }
        if let Some(mut s) = self.active.take() {
            glass_proc_linux::reap_group(&mut s.child, glass_proc_linux::REAP_GRACE);
        }
        self.dbus = None;
    }
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
    // Explicit override wins and is trusted (skips the PATH version gate). Fail closed if it
    // is not an executable file rather than silently falling back to discovery.
    if let Some(p) = std::env::var_os("GLASS_SWAY").filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(GlassError::Backend(format!(
                "GLASS_SWAY={} is not an executable file",
                p.display()
            )))
        };
    }
    if let Some(p) = sway_on_path_if_recent() {
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
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("sway/bin/sway");
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    Err(GlassError::Backend(
        "no sway >=1.12 found. Build it with https://github.com/fixed-width/sway-build (./build.sh && ./build.sh install), \
         or install a distro sway >=1.12."
            .into(),
    ))
}

/// The first `sway` on `PATH`, but only if `sway --version` reports >= 1.12.
fn sway_on_path_if_recent() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
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
fn find_wayland_socket(dir: &Path) -> Option<PathBuf> {
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

delegate_output!(State);
delegate_registry!(State);
delegate_shm!(State);

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
            return Err(GlassError::AppExited(status.code()));
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
    let (active, active_rect) = {
        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.max(1));
        loop {
            let _ = queue.roundtrip(&mut state); // keep the wayland queue serviced
            let wins = ipc.windows().unwrap_or_default();
            if let Some(w) = wins.iter().find(|w| w.focused).or_else(|| wins.first()) {
                mint_id(&mut ids, &mut next_id, &w.identifier);
                break (Some(w.identifier.clone()), rect_to_geom(&w.rect));
            }
            if let Ok(Some(status)) = child.try_wait() {
                // Reap the whole group (see the socket-wait loop above): an
                // unclean sway exit can orphan Xwayland + the app otherwise.
                glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
                return Err(GlassError::AppExited(status.code()));
            }
            if Instant::now() >= deadline {
                glass_proc_linux::reap_group(&mut child, glass_proc_linux::REAP_GRACE);
                return Err(GlassError::Timeout(spec.timeout_ms));
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    };
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

/// `TypeSink` for Wayland: types each character by uploading a one-key keymap (the char's
/// keysym at keycode 1) and tapping it, self-committed per key — exactly the chord sink's
/// shape. A heavy client (e.g. a browser) ignores keys tapped under a multi-key keymap it
/// hasn't adopted, or flushed only once at the end. See glass_core::run_type.
struct WaylandTypeSink<'a> {
    s: &'a mut ActiveSession,
    kb: ZwpVirtualKeyboardV1,
}

impl glass_core::TypeSink for WaylandTypeSink<'_> {
    fn character(&mut self, c: char) -> Result<()> {
        let ks = glass_core::keys::keysym_for_text(c);
        upload_keymap(
            &mut *self.s,
            &self.kb,
            &crate::keyboard::build_keymap(&[ks]),
        )?;
        tap(&mut *self.s, &self.kb, 1)
    }
}

/// XKB real-modifier mask for a chord's modifiers (standard `include "complete"`
/// order: Shift, Lock, Control, Mod1=Alt, ..., Mod4=Super).
fn modifier_mask(mods: &[glass_core::keys::Modifier]) -> u32 {
    use glass_core::keys::Modifier;
    mods.iter().fold(0, |m, x| {
        m | match x {
            Modifier::Shift => 1 << 0,
            Modifier::Control => 1 << 2,
            Modifier::Alt => 1 << 3,
            Modifier::Super => 1 << 6,
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
        // Fail-closed: if a sandbox was requested but bwrap is unavailable, error
        // immediately rather than launching unconfined.
        if spec.sandbox != glass_core::SandboxLevel::Off {
            if let glass_sandbox_linux::Availability::Unavailable(why) =
                glass_sandbox_linux::availability()
            {
                return Err(GlassError::SandboxUnavailable(format!(
                    "{why}. Install bubblewrap / enable unprivileged user namespaces, or pass \
                     sandbox:\"off\" (GLASS_SANDBOX=off) to run unconfined. See `glass-mcp doctor`."
                )));
            }
        }

        // Run the build step (if any) before the compositor starts. The build is
        // sandboxed (bwrap) when sandbox != Off — same semantics as the X11 backend.
        // Runs once: a retried compositor bring-up must not re-run the build.
        glass_sandbox_linux::run_build(spec)?;

        // Bring up the per-session compositor, retrying a transient failure once.
        // A freshly-spawned headless Xwayland occasionally crashes mid-startup
        // ("failed to read Wayland events: Broken pipe") on the GPU-less CI renderer
        // — after the app has already mapped its window — leaving sway alive but the
        // window never stable in its tree, so discovery times out. The crash is rare
        // and independent per spawn, so re-spawning a fresh compositor makes it
        // reliable. Only transient bring-up failures retry (Timeout / Backend); a
        // genuine app exit or a config/sandbox error fails immediately. `bring_up`
        // reaps its own sway+Xwayland process group on failure, so a retry never
        // races a leftover compositor.
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
                return Err(GlassError::Unsupported(
                    "multi-touch gestures are only supported on the android backend".into(),
                ));
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
                // Per-character, self-committed typing (a 1-key keymap + tap, roundtripped
                // per key) so a heavy client receives a long string instead of dropping a
                // batch — see glass_core::run_type and WaylandTypeSink. The per-key roundtrip
                // is the pacing, so no extra inter-character dwell is needed.
                let mut sink = WaylandTypeSink {
                    s: &mut *session,
                    kb,
                };
                glass_core::run_type(&mut sink, text, std::time::Duration::ZERO)?;
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
        let wins: Vec<SwayWindow> = session.ipc.windows()?;
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
mod tests {
    use super::{nudge_x, parse_sway_version};

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
