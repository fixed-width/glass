//! X11 clipboard (CLIPBOARD selection) — get via `convert_selection` polling and
//! set via a dedicated owner thread that serves `SelectionRequest` events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
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

/// Intern one atom, naming which half of the round trip failed.
///
/// `wrap` turns the message into the caller's error type: the owner thread's has to signal the
/// spawner on its way out, so it cannot be the getter's.
fn intern<E>(
    conn: &RustConnection,
    name: &str,
    wrap: impl Fn(String) -> E,
) -> std::result::Result<Atom, E> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(|e| wrap(format!("intern {name}: {e}")))?
        .reply()
        .map_err(|e| wrap(format!("intern {name} reply: {e}")))?
        .atom)
}

fn intern_get_atoms(conn: &RustConnection) -> Result<GetAtoms> {
    Ok(GetAtoms {
        clipboard: intern(conn, "CLIPBOARD", GlassError::Backend)?,
        utf8_string: intern(conn, "UTF8_STRING", GlassError::Backend)?,
        incr: intern(conn, "INCR", GlassError::Backend)?,
        glass_clip: intern(conn, "GLASS_CLIP", GlassError::Backend)?,
    })
}

// ---------------------------------------------------------------------------
// get — requestor side
// ---------------------------------------------------------------------------

/// Read the CLIPBOARD selection as UTF-8 text.
///
/// Opens a fresh connection to avoid disturbing the main connection's event
/// queue. Creates a temporary INPUT_ONLY window, calls `ConvertSelection` with
/// `UTF8_STRING` target + `GLASS_CLIP` transfer property, then polls for the
/// resulting `SelectionNotify`. Returns `""` when the selection is genuinely
/// unowned (the X server replies with `property == NONE`); a selection whose
/// owner does not answer within a second is a timeout and returns a `Backend`
/// error rather than reading as an empty clipboard. INCR (incremental)
/// transfers are refused with a `Backend` error rather than silently
/// truncating.
pub fn get(display: &str) -> Result<String> {
    // Open a separate connection so SelectionNotify is not mixed with other
    // events on the main connection.
    let (req_conn, screen_num) = x11rb::connect(Some(display))
        .map_err(|e| GlassError::Backend(format!("clipboard get: X connect: {e}")))?;
    let req_conn_root = req_conn.setup().roots[screen_num].root;

    let atoms = intern_get_atoms(&req_conn)?;

    // A temporary INPUT_ONLY window to receive the SelectionNotify. It needs no explicit
    // teardown: `req_conn` is dropped when this returns, and an X server destroys everything
    // a client created once its connection closes (close-down mode defaults to DestroyAll).
    let win = req_conn
        .generate_id()
        .map_err(|e| GlassError::Backend(format!("generate_id: {e}")))?;
    req_conn
        .create_window(
            0,
            win,
            req_conn_root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::default(),
        )
        .map_err(|e| GlassError::Backend(format!("create temp window: {e}")))?
        .check()
        .map_err(|e| GlassError::Backend(format!("create temp window check: {e}")))?;

    // Request the selection conversion.
    req_conn
        .convert_selection(
            win,
            atoms.clipboard,
            atoms.utf8_string,
            atoms.glass_clip,
            x11rb::CURRENT_TIME,
        )
        .map_err(|e| GlassError::Backend(format!("convert_selection: {e}")))?;
    req_conn
        .flush()
        .map_err(|e| GlassError::Backend(format!("flush: {e}")))?;

    // Poll for SelectionNotify up to 1 second.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match req_conn
            .poll_for_event()
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
                    let reply = req_conn
                        .get_property(
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
                    // No SelectionNotify in time. Reaching the deadline means an owner exists and
                    // did not answer within a second (a busy app, a modal loop pumping its own
                    // events, a large selection, an app stopped in a debugger) — a genuinely
                    // unowned selection already answered on the `property == NONE` branch above,
                    // so this is a timeout to report, not an empty clipboard to assume.
                    return Err(GlassError::Backend(
                        "clipboard read: the selection owner did not respond within 1s".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
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

/// Recover a poisoned readiness lock instead of propagating the panic, and say so.
///
/// The state behind the lock still says what happened, so panicking here would turn a reportable
/// clipboard failure into a crashed tool call — but recovering in silence leaves the owner thread
/// that panicked invisible.
fn recover_readiness<T>(what: &str, r: std::result::Result<T, PoisonError<T>>) -> T {
    match r {
        Ok(v) => v,
        Err(poisoned) => {
            eprintln!("glass: clipboard {what}: a thread panicked holding this lock");
            poisoned.into_inner()
        }
    }
}

/// A background thread that owns the X11 CLIPBOARD selection and serves paste
/// requests. Created by `ClipboardOwner::spawn`; torn down by dropping it.
pub struct ClipboardOwner {
    // `expect`, not the readiness lock's `PoisonError::into_inner`: this one is off the readiness
    // path and its two holders only clone the string or assign to it, so nothing under it panics.
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
                if let Err(e) = owner_thread(&display, text_clone, &stop_clone, ready_clone) {
                    eprintln!("glass: clipboard owner thread error: {e}");
                }
            })
            .map_err(GlassError::Io)?;

        // Block until the thread signals that it has taken ownership (or
        // encountered an error), with a 2 s timeout.
        let (lock, cvar) = &*ready;
        let result = recover_readiness(
            "set: readiness wait",
            cvar.wait_timeout_while(
                recover_readiness("set: readiness lock", lock.lock()),
                Duration::from_secs(2),
                |s| matches!(s, ReadyState::Pending),
            ),
        );

        let timed_out = result.1.timed_out();
        // Copy the outcome out and release the ready lock before either path below touches
        // `handle`: a signal_ready reachable from a thread-exit path would otherwise deadlock the
        // join.
        let failure = match &*result.0 {
            ReadyState::Err(msg) => Some(msg.clone()),
            // `Pending` is the state on the timed-out path, handled by the branch below;
            // either way there is no failure message to carry.
            ReadyState::Ok | ReadyState::Pending => None,
        };
        drop(result);

        if timed_out {
            stop.store(true, Ordering::Relaxed);
            // Deliberately not joined: timing out means the thread hasn't reached the loop that
            // reads `stop`, so a join would wait on the wedged X server with no bound. Detaching
            // is self-cleaning: the setup fails and the thread exits; the setup finishes and the
            // `stop` check `owner_thread` makes before `set_selection_owner` returns without ever
            // taking the selection; or `stop` arrives after that check and the first serving
            // iteration breaks out. Drop that check and it can unwedge later and steal a live
            // owner's selection.
            if handle.is_finished() && handle.join().is_err() {
                // Finished without ever signalling: it panicked during setup. Reporting that as a
                // timeout would send the reader looking for a slow X server.
                return Err(GlassError::Backend(
                    "clipboard set: owner thread panicked during setup".into(),
                ));
            }
            return Err(GlassError::Backend(
                "clipboard set: timed out waiting for selection ownership".into(),
            ));
        }

        if let Some(msg) = failure {
            let _ = handle.join();
            return Err(GlassError::Backend(format!("clipboard set: {msg}")));
        }

        Ok(Self {
            text,
            stop,
            handle: Some(handle),
        })
    }

    /// Update the text that will be served on the next paste.
    pub fn set_text(&self, text: &str) {
        *self.text.lock().expect("clipboard text mutex") = text.to_string();
    }

    /// Returns `true` while the owner thread is still serving the selection.
    ///
    /// `false` covers every way the thread can stop: another client took CLIPBOARD
    /// (`SelectionClear`), its connection failed, or it panicked. `set_clipboard` spawns a
    /// replacement on `false`.
    pub fn is_alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
}

/// Retires the owner however its thread leaves.
///
/// `stop` is the only thing [`ClipboardOwner::is_alive`] consults, and the thread's only way to
/// say it has gone. A statement at the end of the thread body would miss a panic, which is the
/// one exit route that reaches no statement at all.
struct RetireOnExit<'a>(&'a AtomicBool);

impl Drop for RetireOnExit<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
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
    *recover_readiness("owner: readiness signal", lock.lock()) = state;
    cvar.notify_one();
}

/// A setup failure that has already been reported to the spawner.
///
/// Only [`setup_failed`] constructs one and [`take_selection`] returns nothing else, so reporting
/// a step's failure through `GlassError` instead is a compile error.
#[derive(Debug)]
struct SetupFailed(String);

impl std::fmt::Display for SetupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupFailed {}

/// Report a setup failure to the spawner and stop the thread, handing `msg` back as the error.
///
/// Every fallible step before the `ReadyState::Ok` signal goes through this: the spawner blocks on
/// the condvar, so one that returns without signalling leaves it to time out and blame the wait for
/// a failure that already had a message.
#[must_use = "the caller must return this, or the spawner is told a failure the thread ignored"]
fn setup_failed(
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    stop: &AtomicBool,
    msg: String,
) -> SetupFailed {
    signal_ready(ready, ReadyState::Err(msg.clone()));
    // Inert today — `spawn` returns `Err`, so no `ClipboardOwner` exists to consult `is_alive`.
    // Kept for a caller added later.
    stop.store(true, Ordering::Relaxed);
    SetupFailed(msg)
}

/// What the serve loop needs, once the selection is ours.
struct Owned {
    conn: RustConnection,
    win: Window,
    utf8_string: Atom,
    targets_atom: Atom,
}

/// Connect, intern, create the window and take CLIPBOARD, reporting any failure to the spawner on
/// the way out. `None` means the spawner gave up first, so the selection must be left alone.
fn take_selection(
    display: &str,
    stop: &AtomicBool,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
) -> std::result::Result<Option<Owned>, SetupFailed> {
    let failed = |msg| setup_failed(ready, stop, msg);
    let (conn, screen_num) =
        x11rb::connect(Some(display)).map_err(|e| failed(format!("X connect: {e}")))?;
    let root = conn.setup().roots[screen_num].root;

    // Intern atoms on this connection.
    let clipboard = intern(&conn, "CLIPBOARD", failed)?;
    let utf8_string = intern(&conn, "UTF8_STRING", failed)?;
    let targets_atom = intern(&conn, "TARGETS", failed)?;

    // Create the owner window.
    let win = conn
        .generate_id()
        .map_err(|e| failed(format!("generate_id: {e}")))?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::default(),
    )
    .map_err(|e| failed(format!("create_window: {e}")))?
    .check()
    .map_err(|e| failed(format!("create_window check: {e}")))?;

    // A detached thread (spawn already timed out and returned to the caller) must not take a
    // selection the caller has already been told it failed to get. `spawn`'s detach comment
    // counts on this check.
    if stop.load(Ordering::Relaxed) {
        // Skips the `destroy_window` the normal exit performs: `conn` is dropped on the way out,
        // and an X server destroys everything a client created once its connection closes
        // (close-down mode defaults to DestroyAll).
        return Ok(None);
    }

    // Take ownership of CLIPBOARD.
    conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| failed(format!("set_selection_owner: {e}")))?
        .check()
        .map_err(|e| failed(format!("set_selection_owner check: {e}")))?;
    conn.flush()
        .map_err(|e| failed(format!("flush after set_selection_owner: {e}")))?;

    Ok(Some(Owned {
        conn,
        win,
        utf8_string,
        targets_atom,
    }))
}

fn owner_thread(
    display: &str,
    text: Arc<Mutex<String>>,
    stop: &AtomicBool,
    ready: Arc<(Mutex<ReadyState>, Condvar)>,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // First, so no early return below escapes it.
    let _retire = RetireOnExit(stop);

    let Some(Owned {
        conn,
        win,
        utf8_string,
        targets_atom,
    }) = take_selection(display, stop, &ready)?
    else {
        return Ok(());
    };

    // Past here `setup_failed` is out of scope, so a later failure cannot overwrite an `Ok` the
    // spawner may not have read.
    signal_ready(&ready, ReadyState::Ok);

    // Event loop: serve SelectionRequest until stopped or SelectionClear arrives.
    while !stop.load(Ordering::Relaxed) {
        match conn.poll_for_event()? {
            Some(event) => {
                use x11rb::protocol::Event;
                match event {
                    Event::SelectionRequest(req) => {
                        // Any client that gives up first leaves a window ID the reply cannot be
                        // written to, glass's own reader included once its read deadline expires
                        // (glass#372). A connection that is genuinely broken fails
                        // `poll_for_event` above.
                        if let Err(e) =
                            handle_selection_request(&conn, &req, &text, utf8_string, targets_atom)
                        {
                            eprintln!("glass: clipboard owner: selection request: {e}");
                        }
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

/// Write the converted selection onto the requestor, returning the property it landed in.
///
/// `x11rb::NONE` is the ICCCM refusal, for a target this owner cannot produce. An `Err` is the
/// write itself failing, which the caller turns into a refusal too.
fn convert_onto_requestor(
    conn: &RustConnection,
    req: &SelectionRequestEvent,
    reply_prop: Atom,
    text: &Arc<Mutex<String>>,
    utf8_string: Atom,
    targets_atom: Atom,
) -> std::result::Result<Atom, Box<dyn std::error::Error>> {
    if req.target == targets_atom {
        let atoms: &[u32] = &[targets_atom, utf8_string];
        conn.change_property32(
            PropMode::REPLACE,
            req.requestor,
            reply_prop,
            AtomEnum::ATOM,
            atoms,
        )?
        .check()?;
        Ok(reply_prop)
    } else if req.target == utf8_string {
        let data = text.lock().expect("clipboard text mutex").clone();
        conn.change_property8(
            PropMode::REPLACE,
            req.requestor,
            reply_prop,
            utf8_string,
            data.as_bytes(),
        )?
        .check()?;
        Ok(reply_prop)
    } else {
        Ok(x11rb::NONE)
    }
}

fn handle_selection_request(
    conn: &RustConnection,
    req: &SelectionRequestEvent,
    text: &Arc<Mutex<String>>,
    utf8_string: Atom,
    targets_atom: Atom,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // A NONE property is an old client, which means writing into req.target instead.
    let reply_prop = if req.property != x11rb::NONE {
        req.property
    } else {
        req.target
    };

    // A write that failed is still a request that must be answered: an owner that goes quiet
    // keeps the selection, and every later paste on the display waits out its own timeout.
    let granted_prop =
        match convert_onto_requestor(conn, req, reply_prop, text, utf8_string, targets_atom) {
            Ok(prop) => prop,
            Err(e) => {
                eprintln!(
                    "glass: clipboard owner: converting for requestor {:#x}: {e}",
                    req.requestor
                );
                x11rb::NONE
            }
        };

    // Send SelectionNotify to the requestor.
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: granted_prop,
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?
        .check()?;
    conn.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testx::TestX;

    /// Long enough that a truncated read comes back visibly short.
    const LONG_TEXT: &str = "the quick brown fox jumps over the lazy dog";

    /// Wait for `f` to hold, up to `within`. The owner thread runs on its own connection, so
    /// what it has done is observed rather than sequenced.
    fn eventually(within: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    /// Run `f` on its own thread and wait `within` for its result, failing with `what` if it does
    /// not arrive. The calls these tests bound hang outright when the bound under test is missing,
    /// and a hang on the test thread wedges the whole suite.
    fn on_a_thread<T: Send + 'static>(
        within: Duration,
        what: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(within).expect(what)
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_unowned_clipboard_reads_as_empty_rather_than_failing() {
        // A display where nothing has ever copied is the normal starting state, not an error.
        let x = TestX::start();
        assert_eq!(get(x.display()).expect("get"), "");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn repeated_reads_leave_no_windows_behind() {
        // Each read creates a temporary window to receive the SelectionNotify on. An agent
        // polls the clipboard, so one leaked window per read accumulates for the session.
        let x = TestX::start();
        let _owner =
            ClipboardOwner::spawn(x.display().to_string(), "text".to_string()).expect("spawn");
        let before = x.root_child_count();
        for _ in 0..5 {
            get(x.display()).expect("get");
        }
        // Polled, not asserted outright: each temp window dies when the server processes its
        // own connection closing, and no request this connection can send is ordered against
        // that.
        assert!(
            eventually(Duration::from_secs(3), || x.root_child_count() == before),
            "five reads left {} windows behind",
            x.root_child_count() - before
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn text_handed_to_the_owner_reads_back_whole() {
        let x = TestX::start();
        let _owner = ClipboardOwner::spawn(x.display().to_string(), LONG_TEXT.to_string())
            .expect("the owner should take the selection");
        assert_eq!(get(x.display()).expect("get"), LONG_TEXT);
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_later_copy_replaces_what_the_owner_serves() {
        let x = TestX::start();
        let owner =
            ClipboardOwner::spawn(x.display().to_string(), "first".to_string()).expect("spawn");
        assert_eq!(get(x.display()).expect("get"), "first");
        owner.set_text("second");
        assert_eq!(get(x.display()).expect("get"), "second");
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_owner_is_alive_until_it_is_dropped_and_then_serves_nothing() {
        let x = TestX::start();
        let owner =
            ClipboardOwner::spawn(x.display().to_string(), "gone soon".to_string()).expect("spawn");
        assert!(owner.is_alive());
        drop(owner);
        assert_eq!(
            get(x.display()).expect("get"),
            "",
            "a dropped owner must release the selection, not keep serving it"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn another_client_taking_the_selection_retires_the_owner() {
        // The server sends SelectionClear; ignoring it leaves a thread that believes it still
        // owns a selection it does not, so the next copy is served by nobody.
        let x = TestX::start();
        let owner =
            ClipboardOwner::spawn(x.display().to_string(), "ours".to_string()).expect("spawn");
        assert!(owner.is_alive());

        x.take_clipboard();
        assert!(
            eventually(Duration::from_secs(3), || !owner.is_alive()),
            "the owner should have retired when another client took the selection"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_reply_that_cannot_be_delivered_costs_one_request_not_the_owner() {
        // A requestor that asks and then exits leaves a window ID the reply cannot be written
        // to — glass's own reader does, dropping its temporary window when its read deadline
        // expires (glass#372). Ending the thread there hands the next copy to an owner that
        // serves nobody.
        let x = TestX::start();
        let _owner =
            ClipboardOwner::spawn(x.display().to_string(), "ours".to_string()).expect("spawn");
        let owner_win = x.clipboard_owner();
        assert_ne!(owner_win, x11rb::NONE, "the owner should hold CLIPBOARD");

        let into = x.intern(b"TEST_CLIP_TRANSFER");
        x.selection_request_from(owner_win, x.unused_id(), x.intern(b"UTF8_STRING"), into);

        // The read, not `is_alive`, is what discriminates: a thread that has exited still
        // reported itself alive before this was fixed (glass#371).
        assert_eq!(
            get(x.display()).expect("get"),
            "ours",
            "the owner should still be serving after a reply it could not deliver"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_conversion_the_owner_cannot_write_is_refused_rather_than_left_unanswered() {
        // ICCCM: an owner answers every request, refusing with `property = NONE`. Returning
        // before the reply leaves a live requestor waiting out its own timeout.
        let x = TestX::start();
        let _owner =
            ClipboardOwner::spawn(x.display().to_string(), "ours".to_string()).expect("spawn");
        let owner_win = x.clipboard_owner();
        assert_ne!(owner_win, x11rb::NONE, "the owner should hold CLIPBOARD");

        // A live requestor, so the reply has somewhere to go, and an XID where an atom
        // belongs, so the value write fails with `BadAtom`.
        let requestor = x.window().unmapped().create();
        x.selection_request_from(
            owner_win,
            requestor,
            x.intern(b"UTF8_STRING"),
            x.unused_id(),
        );

        assert_eq!(
            x.awaited_selection_notify(requestor, Duration::from_secs(2)),
            Some(x11rb::NONE),
            "a conversion the owner could not write is a refusal, not silence"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn an_owner_whose_connection_breaks_stops_reporting_itself_alive() {
        // `set_clipboard` hands the text to whatever `is_alive` says is still serving, so a
        // thread that is gone has to say so however it left — here, its connection closed under
        // it. Reporting `true` puts the copy in a mutex nothing reads.
        let x = TestX::start();
        let owner =
            ClipboardOwner::spawn(x.display().to_string(), "ours".to_string()).expect("spawn");
        assert!(owner.is_alive());

        let owner_win = x.clipboard_owner();
        // `KillClient` reads resource 0 as `AllTemporary`, so an owner that never took the
        // selection would kill nothing here and fail below blaming the retire logic.
        assert_ne!(owner_win, x11rb::NONE, "the owner should hold CLIPBOARD");
        x.kill_client(owner_win);

        assert!(
            eventually(Duration::from_secs(3), || !owner.is_alive()),
            "the owner thread is gone; is_alive must not keep claiming otherwise"
        );
    }

    /// Retiring on the way out is a value's `Drop`, not a statement at the end of the thread
    /// body, and this is the difference between them: unwinding runs one and skips the other.
    #[test]
    fn an_owner_thread_that_panics_retires_like_one_that_returned() {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let panicked = std::thread::spawn(move || {
            let _retire = RetireOnExit(&thread_stop);
            // The panic message below is the test working, not the test failing.
            panic!("an owner thread panicking mid-serve");
        })
        .join();

        assert!(panicked.is_err(), "the thread was supposed to panic");
        assert!(
            stop.load(Ordering::Relaxed),
            "a thread that panicked is as gone as one that returned"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn the_owner_advertises_the_targets_it_can_convert_to() {
        let x = TestX::start();
        let _owner =
            ClipboardOwner::spawn(x.display().to_string(), "text".to_string()).expect("spawn");
        let targets = x.intern(b"TARGETS");
        let utf8 = x.intern(b"UTF8_STRING");
        let (requestor, granted) = x
            .request_selection(targets, Duration::from_secs(2))
            .expect("the owner should answer a TARGETS request");
        assert_ne!(
            granted,
            x11rb::NONE,
            "TARGETS is a request every selection owner must answer"
        );
        assert!(
            x.property_atoms(requestor, granted).contains(&utf8),
            "a requestor branches on this list, so it has to name the target glass can \
             actually convert to"
        );
    }

    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_target_the_owner_cannot_produce_is_refused_rather_than_answered_wrongly() {
        // Refusal is `property = NONE`. Answering with the text regardless would hand a
        // requestor asking for an image a string it cannot use.
        let x = TestX::start();
        let _owner =
            ClipboardOwner::spawn(x.display().to_string(), "text".to_string()).expect("spawn");
        assert_ne!(
            x.clipboard_owner(),
            x11rb::NONE,
            "with nobody owning the selection the SERVER replies NONE, and this test would \
             pass without glass having refused anything"
        );
        let png = x.intern(b"image/png");
        let (_requestor, granted) = x
            .request_selection(png, Duration::from_secs(2))
            .expect("the owner should still reply");
        assert_eq!(granted, x11rb::NONE);
    }

    /// A detached setup thread that unwedges after `spawn` already gave up is in exactly this
    /// state: still running, with `stop` already true. Calling `owner_thread` directly with
    /// `stop` pre-set reproduces it without needing a server that actually stalls — it must
    /// stand down before `set_selection_owner`, not take the selection from whatever owner is
    /// now live.
    #[test]
    #[ignore = "starts a real X server; needs Xvfb"]
    fn a_stopped_owner_thread_does_not_take_the_selection_from_a_live_owner() {
        let x = TestX::start();
        let live =
            ClipboardOwner::spawn(x.display().to_string(), "mine".to_string()).expect("spawn");
        assert_eq!(get(x.display()).expect("get"), "mine");

        let text = Arc::new(Mutex::new("stolen".to_string()));
        let stop = Arc::new(AtomicBool::new(true));
        let ready = Arc::new((Mutex::new(ReadyState::Pending), Condvar::new()));

        // On its own thread: a stalled X server (or, on an ablated build, the trailing
        // set_selection_owner) would otherwise hang this call forever.
        let display = x.display().to_string();
        let outcome = on_a_thread(
            Duration::from_secs(10),
            "a stalled X server must fail this test, not hang the whole suite",
            move || {
                owner_thread(&display, text, &stop, ready)
                    .err()
                    .map(|e| e.to_string())
            },
        );
        if let Some(msg) = outcome {
            panic!("a detached thread standing down is not an owner error: {msg}");
        }

        assert!(
            live.is_alive(),
            "the live owner must still be serving after a detached thread stands down"
        );
        assert_eq!(get(x.display()).expect("get"), "mine");
    }

    /// An owner thread that panics holding the readiness lock poisons it. The readiness path has
    /// to keep working through that: propagating the poison instead turns a clipboard failure the
    /// caller could report into a second panic in the caller.
    #[test]
    fn a_poisoned_readiness_lock_is_recovered_rather_than_propagated() {
        let ready = Arc::new((Mutex::new(ReadyState::Pending), Condvar::new()));
        let poisoner = Arc::clone(&ready);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.0.lock().expect("lock");
            // The panic message below is the test working, not the test failing.
            panic!("an owner thread panicking with the readiness lock held");
        })
        .join();
        assert!(panicked.is_err(), "the thread was supposed to panic");
        assert!(
            ready.0.is_poisoned(),
            "a panic under the lock is what poisons it; without that this test proves nothing"
        );

        signal_ready(&ready, ReadyState::Ok);
        let state = ready.0.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            matches!(*state, ReadyState::Ok),
            "signalling through a poisoned lock must still land the state"
        );
    }

    #[test]
    fn spawning_against_an_unreachable_display_fails_instead_of_hanging() {
        let started = Instant::now();
        let Err(err) = ClipboardOwner::spawn(":9999".to_string(), "text".to_string()) else {
            panic!("there is no server on :9999, so no owner can take its selection");
        };
        assert!(matches!(err, GlassError::Backend(_)), "{err:?}");
        // The connect failure's own words: a step that reported nothing would leave the spawner
        // to time out and blame the wait.
        assert!(err.to_string().contains("X connect"), "{err}");
        // Signalled, not the 2 s readiness bound expiring — the only difference a caller sees
        // when a step forgets to signal.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// The signal, the stop and the message: dropping any one leaves the spawner on the condvar
    /// until its own bound expires.
    #[test]
    fn a_setup_failure_signals_the_spawner_stops_the_thread_and_keeps_its_message() {
        let ready = Arc::new((Mutex::new(ReadyState::Pending), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let returned = setup_failed(&ready, &stop, "X connect: refused".to_string());

        assert_eq!(returned.to_string(), "X connect: refused");
        assert!(
            stop.load(Ordering::Relaxed),
            "the thread must not go on to take the selection"
        );
        match &*recover_readiness("test", ready.0.lock()) {
            ReadyState::Err(msg) => assert_eq!(msg, "X connect: refused"),
            _ => panic!("the spawner was left on Pending and will wait out its bound"),
        }
    }

    /// Bind the X11 TCP port of the first free display number, returning a listener and the
    /// display string that reaches it. x11rb maps `host:N` to TCP port 6000+N, and a non-empty
    /// host means it never looks at /tmp/.X11-unix — so this fakes a server without writing into
    /// shared host state.
    fn wedged_x_server() -> (std::net::TcpListener, String) {
        for n in 40..80 {
            if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", 6000 + n)) {
                return (l, format!("127.0.0.1:{n}"));
            }
        }
        panic!("no free X11 TCP port in 6040..6080");
    }

    /// An X server that accepts the connection and never sends its greeting is what the 2 s
    /// readiness bound is for. Joining the setup thread there waits on the wedged server with no
    /// bound at all, so spawn has to return without it — every tool call is behind the session
    /// lock this holds.
    #[test]
    fn an_owner_gives_up_on_a_server_that_accepts_and_never_answers() {
        let (listener, display) = wedged_x_server();
        let accepted = std::thread::spawn(move || listener.accept().map(|(s, _)| s));

        // On its own thread: a spawn that never returns must fail this test, not wedge the whole
        // suite.
        let started = Instant::now();
        let outcome = on_a_thread(
            Duration::from_secs(10),
            "spawn must return when its own 2 s bound expires, not wait on the server",
            move || {
                ClipboardOwner::spawn(display, "text".to_string())
                    .err()
                    .map(|e| e.to_string())
            },
        );
        // Ties the test to the 2 s bound itself, not just to the 10 s ceiling above: a bound
        // loosened to e.g. 9 s would still return before the ceiling but fail this, and a bound
        // shrunk to ~0 (which would also pass the ceiling and still contain "timed out") fails
        // the lower check below.
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "spawn took {elapsed:?}, expected it to give up near its own 2 s bound"
        );
        assert!(
            elapsed >= Duration::from_secs(1),
            "spawn took only {elapsed:?}, expected it to wait close to its own 2 s bound"
        );
        let msg = outcome.expect("a server that never answers is not an owned selection");
        assert!(msg.contains("timed out"), "{msg}");

        // Closing the server side lets the detached setup thread fail and exit.
        drop(
            accepted
                .join()
                .expect("accept thread")
                .expect("accepted stream"),
        );
    }
}
