//! Wayland clipboard get/set via the `wlr-data-control` protocol.
//!
//! `get` reads the compositor's current selection via a temporary
//! `ZwlrDataControlDeviceV1` on the session's existing connection.
//! `set` installs a `ZwlrDataControlSourceV1` on a dedicated serving thread
//! that owns its own wayland connection to the same sway socket, mirroring
//! the X11 owner-thread pattern.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Cap on how long `get()` waits for the selection owner to finish writing the
/// transfer pipe. The owner is an arbitrary external app, so a stuck/slow one
/// must not hang the server (the X11 backend guards the analogous read with 1s).
const CLIP_READ_TIMEOUT: Duration = Duration::from_secs(2);

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_seat;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use glass_core::{GlassError, Result};

use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry};
use wayland_client::protocol::{wl_buffer, wl_output};

// ---- MIME types we offer / accept ----
pub const MIME_UTF8: &str = "text/plain;charset=utf-8";
pub const MIME_PLAIN: &str = "text/plain";
pub const MIME_UTF8_STR: &str = "UTF8_STRING";

/// Choose the best MIME from an advertised list.
/// Preference: `text/plain;charset=utf-8` > `text/plain` > `UTF8_STRING`.
fn pick_mime(mimes: &[String]) -> Option<&'static str> {
    [MIME_UTF8, MIME_PLAIN, MIME_UTF8_STR]
        .iter()
        .find(|&&preferred| mimes.iter().any(|m| m == preferred))
        .copied()
}

// ---------------------------------------------------------------------------
// Minimal State for the clipboard-specific connections
// ---------------------------------------------------------------------------

/// Minimal SCTK state for a clipboard-only Wayland connection.
/// We need registry + output + shm to satisfy the delegate macros; we don't
/// actually use screencopy or input here.
struct ClipState {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    /// Mime types being advertised on the pending offer (before `selection` arrives).
    pending_mimes: Vec<String>,
    /// The current selection offer (if any) + its mime types.
    selection: Option<(ZwlrDataControlOfferV1, Vec<String>)>,
}

impl ProvidesRegistryState for ClipState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

impl OutputHandler for ClipState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for ClipState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_dispatch2!(ClipState);
delegate_registry!(ClipState);

// wl_buffer is required by the shm path.
impl Dispatch<wl_buffer::WlBuffer, ()> for ClipState {
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

impl Dispatch<wl_seat::WlSeat, ()> for ClipState {
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

// Manager has no events.
impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The device's `data_offer` event introduces a new offer object; the `selection`
/// event then tells us which offer is the current selection.
impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                // A new offer object is being introduced; its `offer` events follow
                // immediately. Destroy any stale pending offer.
                state.pending_mimes.clear();
                // The offer proxy will dispatch Offer events that accumulate into
                // pending_mimes via Dispatch<ZwlrDataControlOfferV1>.
                let _ = id;
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                match id {
                    Some(offer) => {
                        let mimes = std::mem::take(&mut state.pending_mimes);
                        state.selection = Some((offer, mimes));
                    }
                    None => {
                        // No selection currently held.
                        state.selection = None;
                    }
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {}
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {}
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClipState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

/// The offer's `offer` event (note: same name as the request) advertises each
/// MIME type the current selection can be transferred as.
impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.pending_mimes.push(mime_type);
        }
    }
}

// ---------------------------------------------------------------------------
// Module-level types needed to avoid re-using ClipState module-level macros
// ---------------------------------------------------------------------------

// We need a separate State for the serving thread that handles `Send` properly.
struct ServeState {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    /// The shared text to serve on `Send` events.
    text: Arc<Mutex<String>>,
    /// Set to true when the source is `Cancelled`.
    cancelled: bool,
}

impl ProvidesRegistryState for ServeState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

impl OutputHandler for ServeState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for ServeState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_dispatch2!(ServeState);
delegate_registry!(ServeState);

impl Dispatch<wl_buffer::WlBuffer, ()> for ServeState {
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

impl Dispatch<wl_seat::WlSeat, ()> for ServeState {
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

impl Dispatch<ZwlrDataControlManagerV1, ()> for ServeState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ServeState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_device_v1::Event::Finished = event {
            state.cancelled = true;
        }
    }

    wayland_client::event_created_child!(ServeState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ServeState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ServeState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type: _, fd } => {
                // Write the current text to the fd and close it.
                let text = state.text.lock().expect("clipboard text mutex").clone();
                // fd is OwnedFd; wrap in File and write (file closes on drop).
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(text.as_bytes());
                // file drops here, closing the write end.
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.cancelled = true;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// `get` — read the current Wayland clipboard selection
// ---------------------------------------------------------------------------

/// Read the current clipboard selection as text.
/// Opens a fresh connection to `socket` to avoid conflicting with the session's
/// event queue, does a roundtrip to collect the selection offer, then reads the
/// pipe transfer.
pub fn get(socket: &Path) -> Result<String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| GlassError::Backend(format!("clipboard get: connect: {e}")))?;
    let conn = Connection::from_socket(stream)
        .map_err(|e| GlassError::Backend(format!("clipboard get: wayland connection: {e}")))?;

    let (globals, mut queue): (_, EventQueue<ClipState>) = registry_queue_init(&conn)
        .map_err(|e| GlassError::Backend(format!("clipboard get: registry: {e}")))?;

    let qh = queue.handle();
    let mut state = ClipState {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh)
            .map_err(|e| GlassError::Backend(format!("clipboard get: shm: {e}")))?,
        pending_mimes: Vec::new(),
        selection: None,
    };

    let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| {
        GlassError::Backend(format!("clipboard get: bind data-control manager: {e}"))
    })?;

    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| GlassError::Backend(format!("clipboard get: bind seat: {e}")))?;

    // get_data_device triggers `data_offer` + `offer*` + `selection` events.
    let _device = manager.get_data_device(&seat, &qh, ());

    // Roundtrip: collect the initial selection advertisement.
    queue
        .roundtrip(&mut state)
        .map_err(|e| GlassError::Backend(format!("clipboard get: roundtrip: {e}")))?;

    // One more roundtrip to make sure pending_mimes have been accumulated.
    queue
        .roundtrip(&mut state)
        .map_err(|e| GlassError::Backend(format!("clipboard get: roundtrip2: {e}")))?;

    let (offer, mimes) = match state.selection.take() {
        Some(v) => v,
        None => return Ok(String::new()), // no clipboard content
    };

    let mime = match pick_mime(&mimes) {
        Some(m) => m.to_string(),
        None => {
            offer.destroy();
            return Ok(String::new()); // no text MIME offered
        }
    };

    // Create a pipe: offer writes to write_end, we read from read_end.
    let (read_end, write_end) = rustix::pipe::pipe()
        .map_err(|e| GlassError::Backend(format!("clipboard get: pipe: {e}")))?;

    offer.receive(mime, write_end.as_fd());
    drop(write_end); // close write end BEFORE roundtrip so the source can EOF us

    conn.flush()
        .map_err(|e| GlassError::Backend(format!("clipboard get: flush: {e}")))?;

    // Roundtrip so the compositor can service our receive request and tell the
    // source to write data. The source's write happens in a separate connection
    // (either ours or another client), so we just need to let the compositor
    // route the fd.
    queue
        .roundtrip(&mut state)
        .map_err(|e| GlassError::Backend(format!("clipboard get: roundtrip3: {e}")))?;

    // Read to EOF from the read end, bounded by a deadline so a misbehaving
    // selection owner can't hang us forever (see CLIP_READ_TIMEOUT).
    let buf = read_to_eof_bounded(read_end, CLIP_READ_TIMEOUT)?;

    offer.destroy();

    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Whether a failed read should be retried — a signal arrived and nothing was lost. Retrying
/// anything else spins to the deadline and reports a timeout, hiding the real failure.
fn is_retryable(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Interrupted
}

/// Read `fd` to EOF, but give up after `timeout`. The selection owner is an
/// arbitrary external app; one that opens the transfer but never finishes
/// writing (or never closes its write end) would otherwise block this read —
/// and, via the session-wide Glass lock, every other tool call. Poll the fd
/// with the remaining deadline and read when ready; we are the pipe's sole
/// reader, so a poll-ready read never blocks. Returns `Timeout` on expiry.
fn read_to_eof_bounded(fd: OwnedFd, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut file = std::fs::File::from(fd);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(GlassError::Timeout(timeout.as_millis() as u64));
        }
        let remaining = deadline - now;
        let ts = rustix::event::Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as i64,
        };
        // The PollFd borrows `file` only for this statement, freeing it before
        // the read below.
        let ready = rustix::event::poll(
            &mut [rustix::event::PollFd::new(
                &file,
                rustix::event::PollFlags::IN,
            )],
            Some(&ts),
        );
        match ready {
            Ok(0) => return Err(GlassError::Timeout(timeout.as_millis() as u64)),
            Ok(_) => match file.read(&mut chunk) {
                Ok(0) => return Ok(buf), // EOF
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if is_retryable(&e) => continue,
                Err(e) => {
                    return Err(GlassError::Backend(format!(
                        "clipboard get: read pipe: {e}"
                    )));
                }
            },
            // The poll's half of the rule `is_retryable` names for the read: a signal arrived
            // and nothing was lost. Same judgement, different error type.
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(GlassError::Backend(format!("clipboard get: poll: {e}"))),
        }
    }
}

/// Cap on how long the serving thread spends handing one paste to a requesting app. Its own
/// constant rather than a reuse of [`CLIP_READ_TIMEOUT`]: that bounds an app glass reads from,
/// this bounds an app glass writes to, and moving one should not move the other.
// The `Send` handler wiring lands separately; until then only the tests below call this.
#[cfg_attr(not(test), allow(dead_code))]
const CLIP_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a failed write should be retried — nothing was lost and the pipe may take more. A
/// signal arrived (`Interrupted`), or the pipe filled between the poll and the write
/// (`WouldBlock`, which non-blocking mode reports instead of parking). `WouldBlock` is retryable
/// here and not in [`is_retryable`] for exactly that reason: only this side sets `O_NONBLOCK`.
#[cfg_attr(not(test), allow(dead_code))]
fn is_write_retryable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
    )
}

/// Write `buf` to `fd`, but give up after `timeout`. The mirror of [`read_to_eof_bounded`] on the
/// serving side: the receiver is an arbitrary external app, and one that opens the transfer but
/// never reads blocks this write as soon as `buf` passes the pipe buffer (64 KiB by default).
/// That block lands on the owner thread inside `dispatch_pending`, so its loop never reaches the
/// stop check — and `ClipboardOwner::drop` joins a thread that can no longer return.
///
/// The fd is made non-blocking first. `POLLOUT` promises only that one byte fits, while a
/// blocking `write` of more than `PIPE_BUF` waits for the whole payload: polling a blocking fd
/// would leave the same unbounded wait one syscall further down.
#[cfg_attr(not(test), allow(dead_code))]
fn write_all_bounded(fd: OwnedFd, buf: &[u8], timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let flags = rustix::fs::fcntl_getfl(&fd)
        .map_err(|e| GlassError::Backend(format!("clipboard serve: fcntl_getfl: {e}")))?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| GlassError::Backend(format!("clipboard serve: fcntl_setfl: {e}")))?;

    let mut file = std::fs::File::from(fd);
    let total = buf.len();
    let mut written = 0usize;
    while written < total {
        let now = Instant::now();
        if now >= deadline {
            return Err(GlassError::Timeout(timeout.as_millis() as u64));
        }
        let remaining = deadline - now;
        let ts = rustix::event::Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as i64,
        };
        // The PollFd borrows `file` only for this statement, freeing it before the write below.
        let ready = rustix::event::poll(
            &mut [rustix::event::PollFd::new(
                &file,
                rustix::event::PollFlags::OUT,
            )],
            Some(&ts),
        );
        match ready {
            Ok(0) => return Err(GlassError::Timeout(timeout.as_millis() as u64)),
            Ok(_) => match file.write(&buf[written..]) {
                Ok(0) => {
                    return Err(GlassError::Backend(format!(
                        "clipboard serve: paste transfer stalled at {written} of {total} bytes"
                    )));
                }
                Ok(n) => written += n,
                Err(e) if is_write_retryable(&e) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Err(GlassError::Backend(format!(
                        "clipboard serve: paste receiver closed the transfer after {written} of {total} bytes"
                    )));
                }
                Err(e) => {
                    return Err(GlassError::Backend(format!(
                        "clipboard serve: write pipe: {e} after {written} of {total} bytes"
                    )));
                }
            },
            // The poll's half of the rule `is_write_retryable` names for the write.
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(GlassError::Backend(format!("clipboard serve: poll: {e}"))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `ClipboardOwner` — serving thread for `set`
// ---------------------------------------------------------------------------

/// A serving thread that owns a `ZwlrDataControlSourceV1`, offering the
/// clipboard text to any app that requests a paste. Mirrors the X11
/// `ClipboardOwner` pattern.
pub struct ClipboardOwner {
    text: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Readiness state signalled from the serving thread back to `spawn`.
enum ReadyState {
    Pending,
    Ok,
    Err(String),
}

impl ClipboardOwner {
    /// Spawn the serving thread.
    ///
    /// Opens its own Wayland connection to `socket`, binds the data-control
    /// manager + seat, creates a source offering the text, calls
    /// `set_selection`, flushes, and does at least one roundtrip so the
    /// compositor registers the selection — then signals ready before
    /// returning. Returns `Err` if the setup fails or times out (2 s).
    pub fn spawn(socket: PathBuf, initial_text: String) -> Result<Self> {
        let text = Arc::new(Mutex::new(initial_text));
        let stop = Arc::new(AtomicBool::new(false));
        // Signalled by the thread once the selection is registered (or it fails).
        let ready: Arc<(Mutex<ReadyState>, Condvar)> =
            Arc::new((Mutex::new(ReadyState::Pending), Condvar::new()));

        let text_thread = Arc::clone(&text);
        let stop_thread = Arc::clone(&stop);
        let ready_thread = Arc::clone(&ready);

        let handle = std::thread::Builder::new()
            .name("glass-wl-clip-owner".into())
            .spawn(move || {
                if let Err(e) = serve_loop(socket, text_thread, stop_thread, ready_thread) {
                    eprintln!("glass-wayland: clipboard owner thread error: {e}");
                }
            })
            .map_err(GlassError::Io)?;

        // Block until the thread signals that it has called set_selection +
        // roundtripped (or encountered an error), with a 2 s timeout.
        let (lock, cvar) = &*ready;
        let result = cvar
            .wait_timeout_while(
                lock.lock().expect("clipboard ready mutex"),
                Duration::from_secs(2),
                |s| matches!(s, ReadyState::Pending),
            )
            .unwrap();

        if result.1.timed_out() {
            // The thread is still running but didn't signal in time; stop it.
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
            return Err(GlassError::Backend(
                "clipboard set: timed out waiting for selection registration".into(),
            ));
        }

        match &*result.0 {
            ReadyState::Ok | ReadyState::Pending /* unreachable but safe */ => {}
            ReadyState::Err(msg) => {
                let _ = handle.join();
                return Err(GlassError::Backend(format!("clipboard set: {msg}")));
            }
        }
        drop(result);

        Ok(Self {
            text,
            stop,
            handle: Some(handle),
        })
    }

    /// Update the text being served. If the thread already lost ownership
    /// (`Cancelled`), this is a no-op (caller should re-spawn).
    pub fn set_text(&self, text: &str) {
        *self.text.lock().expect("clipboard text mutex") = text.to_string();
    }

    /// True if the serving thread is still running (hasn't stopped itself due
    /// to a `Cancelled` event or error).
    pub fn is_alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
}

impl Drop for ClipboardOwner {
    /// Do not add back an inherent `stop(self)` alongside this: taking `self` by value runs the
    /// drop glue anyway, so the two had identical bodies and emptying `stop` changed nothing.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Signal the ready condvar. Helper to reduce repetition.
fn signal_ready(ready: &Arc<(Mutex<ReadyState>, Condvar)>, state: ReadyState) {
    let (lock, cvar) = &**ready;
    *lock.lock().expect("clipboard ready mutex") = state;
    cvar.notify_one();
}

/// The serving thread body: connect, set up the source, pump until stopped.
fn serve_loop(
    socket: PathBuf,
    text: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    ready: Arc<(Mutex<ReadyState>, Condvar)>,
) -> Result<()> {
    let stream = UnixStream::connect(&socket).map_err(|e| {
        let msg = format!("connect: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;
    let conn = Connection::from_socket(stream).map_err(|e| {
        let msg = format!("wayland connection: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;

    let (globals, mut queue): (_, EventQueue<ServeState>) =
        registry_queue_init(&conn).map_err(|e| {
            let msg = format!("registry: {e}");
            signal_ready(&ready, ReadyState::Err(msg.clone()));
            GlassError::Backend(format!("clipboard serve: {msg}"))
        })?;

    let qh = queue.handle();
    let shm = Shm::bind(&globals, &qh).map_err(|e| {
        let msg = format!("shm: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;
    let mut state = ServeState {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm,
        text,
        cancelled: false,
    };

    let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| {
        let msg = format!("bind manager: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;

    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=8, ()).map_err(|e| {
        let msg = format!("bind seat: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;

    let source = manager.create_data_source(&qh, ());
    source.offer(MIME_UTF8.to_string());
    source.offer(MIME_PLAIN.to_string());

    let device = manager.get_data_device(&seat, &qh, ());
    device.set_selection(Some(&source));

    conn.flush().map_err(|e| {
        let msg = format!("flush: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;

    // Roundtrip so the compositor registers the selection before we return to
    // the caller. This is what makes set_clipboard synchronous: spawn() blocks
    // on the ready signal below, which is only sent after this roundtrip.
    queue.roundtrip(&mut state).map_err(|e| {
        let msg = format!("initial roundtrip: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        GlassError::Backend(format!("clipboard serve: {msg}"))
    })?;

    // Selection is now registered with the compositor — unblock spawn().
    signal_ready(&ready, ReadyState::Ok);

    // Pump the queue until cancelled or externally stopped.
    // Use `prepare_read` + `poll` with a short timeout so we can honour the
    // stop flag without blocking indefinitely.
    use std::os::fd::AsFd as _;
    loop {
        // One stop check per iteration, after `dispatch_pending` where a `Cancelled` becomes
        // visible. A second copy at the top made both unkillable — a mutation to either was
        // masked by the other still breaking.

        // Flush any pending outgoing requests.
        if let Err(e) = conn.flush() {
            eprintln!("glass-wayland: clipboard serve: flush: {e}");
            break;
        }

        // Dispatch whatever is already in the queue (non-blocking).
        match queue.dispatch_pending(&mut state) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("glass-wayland: clipboard serve: dispatch_pending: {e}");
                break;
            }
        }

        if stop.load(Ordering::Relaxed) || state.cancelled {
            break;
        }

        // Prepare to read new events from the socket. If the queue already has
        // pending events, `prepare_read` returns `None`; in that case loop
        // straight back to dispatch_pending without waiting on the fd.
        let guard = match queue.prepare_read() {
            Some(g) => g,
            None => continue, // unread events still in queue
        };

        // Poll the connection fd with a short timeout so we can check `stop`
        // between polls without parking the thread forever (50 ms).
        let fd = conn.as_fd();
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        match rustix::event::poll(
            &mut [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )],
            Some(&timeout),
        ) {
            Ok(n) if n > 0 => {
                // Data available: read into the queue.
                if let Err(e) = guard.read() {
                    eprintln!("glass-wayland: clipboard serve: read: {e}");
                    break;
                }
            }
            Ok(_) => {
                // Timeout — loop back and check the stop flag.
                drop(guard);
            }
            Err(e) => {
                eprintln!("glass-wayland: clipboard serve: poll: {e}");
                drop(guard);
                break;
            }
        }
    }

    // Signal that this owner has stopped so the platform can re-spawn if needed.
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CLIP_READ_TIMEOUT, CLIP_WRITE_TIMEOUT, ClipboardOwner, get, is_retryable,
        read_to_eof_bounded, write_all_bounded,
    };
    use crate::testw::Launch;
    use glass_core::GlassError;
    use std::io::Write;

    /// The serving thread is the clipboard — it holds the selection while it runs, and a pasting
    /// app reads from it over a pipe. Nothing under that is fakeable, so these use a real one.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn an_owner_serves_its_text_and_reports_itself_alive() {
        let s = Launch::new().start();
        let socket = s.wayland_socket();
        let owner = ClipboardOwner::spawn(socket.clone(), "served".into()).expect("spawn");
        assert!(owner.is_alive(), "a freshly spawned owner is serving");
        assert_eq!(get(&socket).expect("get"), "served");
    }

    /// Another client taking the selection stops this thread. Reporting itself alive afterwards
    /// makes `set_clipboard` update a thread serving nothing, and the next paste reads the other
    /// client's text.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn an_owner_that_lost_the_selection_stops_reporting_itself_alive() {
        let s = Launch::new().start();
        let socket = s.wayland_socket();
        let first = ClipboardOwner::spawn(socket.clone(), "mine".into()).expect("spawn");
        let _second = ClipboardOwner::spawn(socket.clone(), "theirs".into()).expect("spawn");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while first.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !first.is_alive(),
            "an owner the compositor cancelled is not still serving"
        );
        assert_eq!(get(&socket).expect("get"), "theirs");
    }

    /// Updating in place is what lets a second write reuse the thread. If `set_text` did nothing
    /// the old value would keep being served, with nothing to say it had gone stale.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn updating_an_owners_text_changes_what_a_paste_reads() {
        let s = Launch::new().start();
        let socket = s.wayland_socket();
        let owner = ClipboardOwner::spawn(socket.clone(), "before".into()).expect("spawn");
        assert_eq!(get(&socket).expect("get"), "before");
        owner.set_text("after");
        assert_eq!(get(&socket).expect("get"), "after");
    }

    /// Dropping an owner has to *return*. `Drop` joins the serving thread, so a loop that stops
    /// noticing the stop flag does not fail a test — it wedges it, and takes teardown with it.
    /// Bounded from another thread, because a blocking drop cannot time itself.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn dropping_an_owner_does_not_block() {
        let s = Launch::new().start();
        let owner = ClipboardOwner::spawn(s.wayland_socket(), "held".into()).expect("spawn");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(owner);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("dropping an owner must stop its thread, not wait on it forever");
    }

    /// Dropping gives up the selection. A thread that kept running would keep answering pastes
    /// after glass believed the clipboard was released — and would still be holding the wayland
    /// socket when the session tore down around it.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn dropping_an_owner_gives_up_the_selection() {
        let s = Launch::new().start();
        let socket = s.wayland_socket();
        let owner = ClipboardOwner::spawn(socket.clone(), "transient".into()).expect("spawn");
        assert_eq!(get(&socket).expect("get"), "transient");
        drop(owner);
        assert_eq!(get(&socket).expect("get"), "");
    }

    /// A compositor that is gone cannot be served. Reporting success would leave the caller
    /// believing the clipboard holds text no app can ever read.
    #[test]
    fn an_owner_cannot_be_spawned_against_a_socket_that_is_not_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = match ClipboardOwner::spawn(dir.path().join("wayland-9"), "text".into()) {
            Ok(_) => panic!("a socket with no compositor behind it must not read as served"),
            Err(e) => e,
        };
        assert!(matches!(err, GlassError::Backend(_)), "{err}");
    }

    /// A signal arriving mid-read costs nothing and the read resumes. Every other failure is a
    /// failure: retrying one spins out the deadline and reports a timeout, which sends the reader
    /// looking for a slow app instead of at the error that actually happened.
    #[test]
    fn only_an_interrupted_read_is_retried() {
        use std::io::{Error, ErrorKind};
        assert!(is_retryable(&Error::from(ErrorKind::Interrupted)));
        for other in [
            ErrorKind::BrokenPipe,
            ErrorKind::PermissionDenied,
            ErrorKind::WouldBlock,
        ] {
            assert!(!is_retryable(&Error::from(other)), "{other:?}");
        }
    }
    use std::time::Duration;

    #[test]
    fn bounded_read_times_out_when_writer_stalls() {
        // Writer end open but silent — a stuck/slow selection owner that opened
        // the transfer but never finishes writing. Must NOT hang forever.
        let (read_end, _write_end) = rustix::pipe::pipe().expect("pipe");
        let r = read_to_eof_bounded(read_end, Duration::from_millis(200));
        assert!(
            matches!(r, Err(GlassError::Timeout(_))),
            "expected Timeout, got {r:?}"
        );
        // _write_end stays alive above so no EOF is signalled during the read.
    }

    #[test]
    fn bounded_read_collects_until_eof() {
        let (read_end, write_end) = rustix::pipe::pipe().expect("pipe");
        let mut w = std::fs::File::from(write_end);
        w.write_all(b"clip!").expect("write");
        drop(w); // close the write end → EOF
        let got = read_to_eof_bounded(read_end, CLIP_READ_TIMEOUT).expect("read ok");
        assert_eq!(got, b"clip!");
    }

    #[test]
    fn pick_mime_prefers_charset_utf8_then_plain_then_utf8_string() {
        use super::{MIME_PLAIN, MIME_UTF8, MIME_UTF8_STR, pick_mime};
        let list = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        // Preference order wins over the order the owner advertised them in.
        assert_eq!(pick_mime(&list(&[MIME_PLAIN, MIME_UTF8])), Some(MIME_UTF8));
        // Falls through to text/plain when the charset form is absent.
        assert_eq!(
            pick_mime(&list(&[MIME_UTF8_STR, MIME_PLAIN])),
            Some(MIME_PLAIN)
        );
        // Falls through to UTF8_STRING when it is the only text form offered.
        assert_eq!(pick_mime(&list(&[MIME_UTF8_STR])), Some(MIME_UTF8_STR));
        // No text representation offered -> None (get short-circuits to an empty string).
        assert_eq!(pick_mime(&list(&["image/png", "text/html"])), None);
        assert_eq!(pick_mime(&[]), None);
    }

    /// A payload past the pipe buffer cannot be handed over in one `write`. An implementation
    /// that writes once and trusts the count truncates the paste, and the app pasting has no way
    /// to tell a short clipboard from a short write.
    #[test]
    fn a_bounded_write_delivers_a_payload_larger_than_the_pipe_buffer() {
        use std::io::Read as _;
        let (read_end, write_end) = rustix::pipe::pipe().expect("pipe");
        // 256 KiB — four times the default 64 KiB pipe buffer.
        let payload = vec![b'x'; 256 * 1024];
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            std::fs::File::from(read_end)
                .read_to_end(&mut got)
                .expect("read");
            got
        });
        write_all_bounded(write_end, &payload, Duration::from_secs(10)).expect("write ok");
        let got = reader.join().expect("reader thread");
        assert_eq!(got.len(), payload.len(), "every byte reaches the reader");
        assert!(got == payload, "the bytes that arrive are the bytes sent");
    }

    /// An app that requests a paste and then stops reading is the case this bound exists for:
    /// the write blocks once the pipe fills, on the thread whose loop honours the stop flag.
    #[test]
    fn a_bounded_write_times_out_when_the_reader_never_reads() {
        let (read_end, write_end) = rustix::pipe::pipe().expect("pipe");
        let payload = vec![b'x'; 256 * 1024];
        let r = write_all_bounded(write_end, &payload, Duration::from_millis(200));
        assert!(
            matches!(r, Err(GlassError::Timeout(_))),
            "expected Timeout, got {r:?}"
        );
        // read_end stays alive until here: a closed one would fail as EPIPE, not as a timeout.
        drop(read_end);
    }

    /// The common real failure: the app took what it wanted and closed the transfer. Silence here
    /// leaves a truncated paste indistinguishable from an empty clipboard.
    #[test]
    fn a_bounded_write_reports_a_receiver_that_closed_the_transfer() {
        let (read_end, write_end) = rustix::pipe::pipe().expect("pipe");
        drop(read_end);
        let err = write_all_bounded(write_end, b"clip!", CLIP_WRITE_TIMEOUT)
            .expect_err("a closed receiver is not a delivered paste");
        let msg = err.to_string();
        assert!(msg.contains("closed the transfer"), "{msg}");
        assert!(msg.contains("0 of 5 bytes"), "{msg}");
    }
}
