//! X11 clipboard (CLIPBOARD selection) — get via `convert_selection` polling and
//! set via a dedicated owner thread that serves `SelectionRequest` events.

use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use glass_core::{GlassError, Result};

// ---------------------------------------------------------------------------
// Atom helpers
// ---------------------------------------------------------------------------

struct GetAtoms {
    clipboard: Atom,
    utf8_string: Atom,
    incr: Atom,
    glass_clip: Atom,
}

fn intern_get_atoms(conn: &RustConnection) -> Result<GetAtoms> {
    let clipboard = conn.intern_atom(false, b"CLIPBOARD")
        .map_err(|e| GlassError::Backend(format!("intern CLIPBOARD: {e}")))?
        .reply()
        .map_err(|e| GlassError::Backend(format!("intern CLIPBOARD reply: {e}")))?
        .atom;
    let utf8_string = conn.intern_atom(false, b"UTF8_STRING")
        .map_err(|e| GlassError::Backend(format!("intern UTF8_STRING: {e}")))?
        .reply()
        .map_err(|e| GlassError::Backend(format!("intern UTF8_STRING reply: {e}")))?
        .atom;
    let incr = conn.intern_atom(false, b"INCR")
        .map_err(|e| GlassError::Backend(format!("intern INCR: {e}")))?
        .reply()
        .map_err(|e| GlassError::Backend(format!("intern INCR reply: {e}")))?
        .atom;
    let glass_clip = conn.intern_atom(false, b"GLASS_CLIP")
        .map_err(|e| GlassError::Backend(format!("intern GLASS_CLIP: {e}")))?
        .reply()
        .map_err(|e| GlassError::Backend(format!("intern GLASS_CLIP reply: {e}")))?
        .atom;
    Ok(GetAtoms { clipboard, utf8_string, incr, glass_clip })
}

// ---------------------------------------------------------------------------
// get — requestor side
// ---------------------------------------------------------------------------

/// Read the CLIPBOARD selection as UTF-8 text.
///
/// Opens a fresh connection to avoid disturbing the main connection's event
/// queue. Creates a temporary INPUT_ONLY window, calls `ConvertSelection` with
/// `UTF8_STRING` target + `GLASS_CLIP` transfer property, then polls for the
/// resulting `SelectionNotify`.  Returns `""` if there is no current owner.
/// INCR (incremental) transfers are refused with a `Backend` error rather than
/// silently truncating.
pub fn get(display: &str) -> Result<String> {
    // Open a separate connection so SelectionNotify is not mixed with other
    // events on the main connection.
    let (req_conn, screen_num) = x11rb::connect(Some(display))
        .map_err(|e| GlassError::Backend(format!("clipboard get: X connect: {e}")))?;
    let req_conn_root = req_conn.setup().roots[screen_num].root;

    let atoms = intern_get_atoms(&req_conn)?;

    // Create a temporary INPUT_ONLY window to receive the SelectionNotify.
    let win = req_conn.generate_id()
        .map_err(|e| GlassError::Backend(format!("generate_id: {e}")))?;
    req_conn.create_window(
        0,
        win,
        req_conn_root,
        0, 0, 1, 1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::default(),
    )
    .map_err(|e| GlassError::Backend(format!("create temp window: {e}")))?
    .check()
    .map_err(|e| GlassError::Backend(format!("create temp window check: {e}")))?;

    let _cleanup = WindowGuard { conn: &req_conn, win };

    // Request the selection conversion.
    req_conn.convert_selection(
        win,
        atoms.clipboard,
        atoms.utf8_string,
        atoms.glass_clip,
        x11rb::CURRENT_TIME,
    )
    .map_err(|e| GlassError::Backend(format!("convert_selection: {e}")))?;
    req_conn.flush().map_err(|e| GlassError::Backend(format!("flush: {e}")))?;

    // Poll for SelectionNotify up to 1 second.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match req_conn.poll_for_event()
            .map_err(|e| GlassError::Backend(format!("poll_for_event: {e}")))?
        {
            Some(event) => {
                use x11rb::protocol::Event;
                if let Event::SelectionNotify(notify) = event {
                    if notify.requestor != win {
                        // Not ours; keep polling.
                        continue;
                    }
                    if notify.property == x11rb::NONE {
                        // No owner or owner refused.
                        return Ok(String::new());
                    }
                    // Read the property value.
                    let reply = req_conn.get_property(
                        true, // delete=true
                        win,
                        atoms.glass_clip,
                        AtomEnum::ANY,
                        0,
                        u32::MAX / 4,
                    )
                    .map_err(|e| GlassError::Backend(format!("get_property: {e}")))?
                    .reply()
                    .map_err(|e| GlassError::Backend(format!("get_property reply: {e}")))?;

                    if reply.type_ == atoms.incr {
                        return Err(GlassError::Backend(
                            "clipboard selection too large (INCR unsupported)".into(),
                        ));
                    }

                    let text = String::from_utf8_lossy(&reply.value).into_owned();
                    return Ok(text);
                }
            }
            None => {
                if Instant::now() >= deadline {
                    // No SelectionNotify in time — assume no owner.
                    return Ok(String::new());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// RAII guard that destroys the temp window when it goes out of scope.
struct WindowGuard<'a> {
    conn: &'a RustConnection,
    win: Window,
}

impl Drop for WindowGuard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

// ---------------------------------------------------------------------------
// ClipboardOwner — set side
// ---------------------------------------------------------------------------

/// Readiness state signalled from the owner thread back to `spawn`.
enum ReadyState {
    Pending,
    Ok,
    Err(String),
}

/// A background thread that owns the X11 CLIPBOARD selection and serves paste
/// requests.  Created by `ClipboardOwner::spawn`; torn down by `stop()`/`Drop`.
pub struct ClipboardOwner {
    text: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardOwner {
    /// Spawn a thread that opens its own X connection, takes ownership of
    /// CLIPBOARD, and serves `SelectionRequest` events until stopped.
    ///
    /// Blocks until the owner thread has successfully called `SetSelectionOwner`
    /// (or errored out), so the caller can immediately call `get_clipboard` and
    /// find the selection already owned.  Returns `Err` if setup fails or times
    /// out (2 s).
    pub fn spawn(display: String, initial_text: String) -> Result<Self> {
        let text = Arc::new(Mutex::new(initial_text));
        let stop = Arc::new(AtomicBool::new(false));
        // Signalled by the thread once ownership is established (or it fails).
        let ready: Arc<(Mutex<ReadyState>, Condvar)> =
            Arc::new((Mutex::new(ReadyState::Pending), Condvar::new()));

        let text_clone = Arc::clone(&text);
        let stop_clone = Arc::clone(&stop);
        let ready_clone = Arc::clone(&ready);

        let handle = std::thread::Builder::new()
            .name("glass-x11-clip-owner".into())
            .spawn(move || {
                if let Err(e) = owner_thread(&display, text_clone, stop_clone, Arc::clone(&ready_clone)) {
                    eprintln!("glass: clipboard owner thread error: {e}");
                }
            })
            .map_err(GlassError::Io)?;

        // Block until the thread signals that it has taken ownership (or
        // encountered an error), with a 2 s timeout.
        let (lock, cvar) = &*ready;
        let result = cvar
            .wait_timeout_while(
                lock.lock().unwrap(),
                Duration::from_secs(2),
                |s| matches!(s, ReadyState::Pending),
            )
            .unwrap();

        if result.1.timed_out() {
            // Thread still running but didn't signal in time; stop it.
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
            return Err(GlassError::Backend(
                "clipboard set: timed out waiting for selection ownership".into(),
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

        Ok(Self { text, stop, handle: Some(handle) })
    }

    /// Update the text that will be served on the next paste.
    pub fn set_text(&self, text: &str) {
        *self.text.lock().unwrap() = text.to_string();
    }

    /// Returns `true` if the owner thread is still running (i.e. still owns the
    /// selection; `false` means `SelectionClear` was received).
    pub fn is_alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    /// Signal the thread to stop and wait for it to exit.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Owner thread body
// ---------------------------------------------------------------------------

/// Signal the ready condvar from the owner thread. Helper to reduce repetition.
fn signal_ready(ready: &Arc<(Mutex<ReadyState>, Condvar)>, state: ReadyState) {
    let (lock, cvar) = &**ready;
    *lock.lock().unwrap() = state;
    cvar.notify_one();
}

fn owner_thread(
    display: &str,
    text: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    ready: Arc<(Mutex<ReadyState>, Condvar)>,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(Some(display)).map_err(|e| {
        let msg = format!("X connect: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?;
    let root = conn.setup().roots[screen_num].root;

    // Intern atoms on this connection.
    let clipboard = conn.intern_atom(false, b"CLIPBOARD").map_err(|e| {
        let msg = format!("intern CLIPBOARD: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.reply().map_err(|e| {
        let msg = format!("intern CLIPBOARD reply: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.atom;
    let utf8_string = conn.intern_atom(false, b"UTF8_STRING").map_err(|e| {
        let msg = format!("intern UTF8_STRING: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.reply().map_err(|e| {
        let msg = format!("intern UTF8_STRING reply: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.atom;
    let targets_atom = conn.intern_atom(false, b"TARGETS").map_err(|e| {
        let msg = format!("intern TARGETS: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.reply().map_err(|e| {
        let msg = format!("intern TARGETS reply: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.atom;

    // Create the owner window.
    let win = conn.generate_id().map_err(|e| {
        let msg = format!("generate_id: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?;
    conn.create_window(
        0,
        win,
        root,
        0, 0, 1, 1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::default(),
    ).map_err(|e| {
        let msg = format!("create_window: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.check().map_err(|e| {
        let msg = format!("create_window check: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?;

    // Take ownership of CLIPBOARD.
    conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME).map_err(|e| {
        let msg = format!("set_selection_owner: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?.check().map_err(|e| {
        let msg = format!("set_selection_owner check: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?;
    conn.flush().map_err(|e| {
        let msg = format!("flush after set_selection_owner: {e}");
        signal_ready(&ready, ReadyState::Err(msg.clone()));
        stop.store(true, Ordering::Relaxed);
        msg
    })?;

    // Signal the spawner that we now own the selection.
    signal_ready(&ready, ReadyState::Ok);

    // Event loop: serve SelectionRequest until stopped or SelectionClear arrives.
    while !stop.load(Ordering::Relaxed) {
        match conn.poll_for_event()? {
            Some(event) => {
                use x11rb::protocol::Event;
                match event {
                    Event::SelectionRequest(req) => {
                        handle_selection_request(
                            &conn,
                            &req,
                            &text,
                            utf8_string,
                            targets_atom,
                        )?;
                    }
                    Event::SelectionClear(_) => {
                        // Another client took ownership; we're done serving.
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    _ => {}
                }
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    let _ = conn.destroy_window(win);
    let _ = conn.flush();
    Ok(())
}

fn handle_selection_request(
    conn: &RustConnection,
    req: &SelectionRequestEvent,
    text: &Arc<Mutex<String>>,
    utf8_string: Atom,
    targets_atom: Atom,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // The property to write into on the requestor.  If req.property is NONE
    // (old clients), fall back to writing into req.target as the property.
    let reply_prop = if req.property != x11rb::NONE {
        req.property
    } else {
        req.target
    };

    let granted_prop = if req.target == targets_atom {
        let atoms: &[u32] = &[targets_atom, utf8_string];
        conn.change_property32(
            PropMode::REPLACE,
            req.requestor,
            reply_prop,
            AtomEnum::ATOM,
            atoms,
        )?.check()?;
        reply_prop
    } else if req.target == utf8_string {
        let data = text.lock().unwrap().clone();
        conn.change_property8(
            PropMode::REPLACE,
            req.requestor,
            reply_prop,
            utf8_string,
            data.as_bytes(),
        )?.check()?;
        reply_prop
    } else {
        // Unsupported target: refuse with property = NONE.
        x11rb::NONE
    };

    // Send SelectionNotify to the requestor.
    let notify = SelectionNotifyEvent {
        response_type: 31, // SELECTION_NOTIFY event code
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: granted_prop,
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?.check()?;
    conn.flush()?;
    Ok(())
}
