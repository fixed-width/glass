//! glass-testapp: a minimal X11 window used as a deterministic fixture for
//! glass-x11 integration tests. Draws four known-color quadrants and echoes
//! received input/configure events to stdout (one `EVENT ...` line each).

use std::io::Write;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

const WIDTH: u16 = 320;
const HEIGHT: u16 = 240;

// `--blink` mode: a small rectangle repainted on a fixed schedule — a deterministic stand-in
// for a blinking text caret / clock / spinner, for tests proving glass's `ignore` masks keep a
// perpetually animating region from blocking wait_stable/diff. Fully inside the TL quadrant
// (0..160 x 0..120), clear of any seam, so masking it can't accidentally exclude real content.
const BLINK_X: i16 = 16;
const BLINK_Y: i16 = 16;
const BLINK_W: u16 = 32;
const BLINK_H: u16 = 32;
// The painted grayscale level advances every this many ms, derived from wall-clock elapsed
// time rather than an incrementing counter, so it needs no cross-thread state. It never
// repeats within a short observation window (wraps every 256 * BLINK_TICK_MS ≈ 1.3s).
const BLINK_TICK_MS: u128 = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `--no-wm-pid` suppresses _NET_WM_PID so tests can exercise glass's
    // fallback window discovery (by title/class) the way Xaw/legacy apps force.
    // `--no-self-focus` skips the startup self-focus below, so the fixture
    // behaves like a real toolkit app that does NOT grab focus in a WM-less
    // session — letting tests verify that glass itself focuses the window.
    let no_wm_pid = std::env::args().any(|a| a == "--no-wm-pid");
    let reparent = std::env::args().any(|a| a == "--reparent");
    let no_self_focus = std::env::args().any(|a| a == "--no-self-focus");
    // `--fork-child` spawns a long-lived child process (a `sleep`) and prints its
    // pid as `EVENT child_pid=<n>`, so tests can verify glass reaps the whole
    // process group on shutdown rather than orphaning the app's children.
    let fork_child = std::env::args().any(|a| a == "--fork-child");
    // `--blink` repaints a small rectangle on a fixed schedule instead of blocking on X11
    // events — see the BLINK_* constants. Default (unset) behavior is unchanged: the loop
    // below still just blocks on `wait_for_event`.
    let blink = std::env::args().any(|a| a == "--blink");
    // `--windows N` opens N-1 extra plain top-levels (titled glass-testapp-1..)
    // alongside the main quadrant window, each a distinct solid color, so
    // multi-window enumeration can be tested. Default 1 = unchanged behavior.
    let window_count: usize = std::env::args()
        .position(|a| a == "--windows")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
        .max(1);
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let win = conn.generate_id()?;

    conn.create_window(
        screen.root_depth,
        win,
        root,
        0,
        0,
        WIDTH,
        HEIGHT,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::KEY_PRESS
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_MOTION,
            ),
    )?;

    // Identify ourselves so glass-x11 can find this window by PID/name.
    if !no_wm_pid {
        let pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        let cardinal = conn.intern_atom(false, b"CARDINAL")?.reply()?.atom;
        let pid = std::process::id();
        conn.change_property32(PropMode::REPLACE, win, pid_atom, cardinal, &[pid])?;
    }
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"glass-testapp",
    )?;
    // WM_CLASS is two NUL-separated strings: instance\0class\0
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"glass-testapp\0glass-testapp\0",
    )?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;

    if reparent {
        // Simulate a reparenting WM: put `win` under a frame window (the root's
        // child) and advertise `win` via _NET_CLIENT_LIST on the root. `win` is
        // still unmapped here, so the reparent needs no re-map afterwards; the
        // existing `map_window(win)` below maps it under the frame.
        let frame = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            frame,
            root,
            0,
            0,
            WIDTH,
            HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().background_pixel(screen.white_pixel),
        )?;
        conn.map_window(frame)?;
        conn.reparent_window(win, frame, 0, 0)?;
        let net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            net_client_list,
            AtomEnum::WINDOW,
            &[win],
        )?;
    }

    conn.map_window(win)?;
    if !no_self_focus {
        conn.set_input_focus(InputFocus::PARENT, win, CURRENT_TIME)?;
    }
    conn.flush()?;

    // Extra windows for multi-window tests: placed side-by-side, each a distinct
    // solid color. Up to 3 windows fit without obscuring on the 1024-wide test
    // Xvfb (the color palette is 3 entries, so that is the intended ceiling).
    let mut extras: Vec<(Window, u32)> = Vec::new();
    for i in 1..window_count {
        // Let the previous toplevel fully establish before creating the next one.
        // Creating multiple X11 toplevels back-to-back (no event-loop processing in
        // between, which a real GUI toolkit never does) races Xwayland's per-surface
        // setup under headless sway: the second window's wl_surface intermittently
        // never maps, so it goes missing from sway's tree (and thus list_windows).
        // A round-trip plus a short settle serializes creation and makes multi-window
        // enumeration deterministic. Only runs for `--windows N>1`.
        conn.get_input_focus()?.reply()?;
        std::thread::sleep(std::time::Duration::from_millis(250));
        let ewin = conn.generate_id()?;
        let ex = (i as i16) * (WIDTH as i16 + 20);
        conn.create_window(
            screen.root_depth,
            ewin,
            root,
            ex,
            0,
            WIDTH,
            HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(screen.black_pixel)
                .event_mask(EventMask::EXPOSURE),
        )?;
        if !no_wm_pid {
            let pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
            let cardinal = conn.intern_atom(false, b"CARDINAL")?.reply()?.atom;
            conn.change_property32(
                PropMode::REPLACE,
                ewin,
                pid_atom,
                cardinal,
                &[std::process::id()],
            )?;
        }
        let title = format!("glass-testapp-{i}");
        conn.change_property8(
            PropMode::REPLACE,
            ewin,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            ewin,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"glass-testapp\0glass-testapp\0",
        )?;
        conn.map_window(ewin)?;
        extras.push((ewin, extra_color(i)));
    }
    conn.flush()?;

    if fork_child {
        // A grandchild that a single-pid kill of the fixture would orphan.
        let kid = std::process::Command::new("sleep").arg("3600").spawn()?;
        println!("EVENT child_pid={}", kid.id());
        std::io::stdout().flush()?;
        std::mem::forget(kid); // don't reap on drop; glass's group-reap must get it
    }

    // Announce readiness on stdout (one line, flushed) so tests can sync.
    println!("READY w={WIDTH} h={HEIGHT}");
    std::io::stdout().flush()?;

    if blink {
        run_blink_loop(&conn, win, gc, &extras)
    } else {
        run_event_loop(&conn, win, gc, &extras)
    }
}

/// Handle one X11 event the same way regardless of loop mode: paint on Expose, echo
/// input/geometry to stdout for the test harness to assert on.
fn dispatch_event(
    conn: &impl Connection,
    win: Window,
    gc: Gcontext,
    extras: &[(Window, u32)],
    event: Event,
) -> Result<(), Box<dyn std::error::Error>> {
    match event {
        Event::Expose(e) => {
            if e.window == win {
                draw_quadrants(conn, win, gc)?;
            } else if let Some(&(ewin, color)) = extras.iter().find(|(w, _)| *w == e.window) {
                draw_solid(conn, ewin, gc, color)?;
            }
            conn.flush()?;
        }
        Event::ButtonPress(e) => {
            println!(
                "EVENT button={} x={} y={} state={}",
                e.detail,
                e.event_x,
                e.event_y,
                u16::from(e.state)
            );
            std::io::stdout().flush()?;
        }
        Event::MotionNotify(e) => {
            println!("EVENT motion x={} y={}", e.event_x, e.event_y);
            std::io::stdout().flush()?;
        }
        Event::KeyPress(e) => {
            // Fetch the keysym for this keycode under the *current* keymap so
            // a dynamically-uploaded keymap (Wayland backend) is reflected.
            let m = conn.get_keyboard_mapping(e.detail, 1)?.reply()?;
            let ks = m.keysyms.first().copied().unwrap_or(0);
            println!("EVENT keysym={ks}");
            std::io::stdout().flush()?;
        }
        Event::ConfigureNotify(e) => {
            println!("EVENT configure w={} h={}", e.width, e.height);
            std::io::stdout().flush()?;
        }
        _ => {}
    }
    Ok(())
}

/// Default loop: block on X11 events. Behavior unchanged from before `--blink` existed.
fn run_event_loop(
    conn: &impl Connection,
    win: Window,
    gc: Gcontext,
    extras: &[(Window, u32)],
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let event = conn.wait_for_event()?;
        dispatch_event(conn, win, gc, extras, event)?;
    }
}

/// `--blink`: repaint the blink rect at the current wall-clock-derived grayscale level on
/// every tick, while still servicing events non-blockingly.
fn run_blink_loop(
    conn: &impl Connection,
    win: Window,
    gc: Gcontext,
    extras: &[(Window, u32)],
) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    loop {
        while let Some(event) = conn.poll_for_event()? {
            dispatch_event(conn, win, gc, extras, event)?;
        }
        let level = ((start.elapsed().as_millis() / BLINK_TICK_MS) % 256) as u8;
        draw_blink(conn, win, gc, level)?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(BLINK_TICK_MS as u64));
    }
}

/// Fill the blink rectangle with a flat grayscale fill at `level`.
fn draw_blink(
    conn: &impl Connection,
    win: Window,
    gc: Gcontext,
    level: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let level = u32::from(level);
    let color = (level << 16) | (level << 8) | level;
    conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
    conn.poly_fill_rectangle(
        win,
        gc,
        &[Rectangle {
            x: BLINK_X,
            y: BLINK_Y,
            width: BLINK_W,
            height: BLINK_H,
        }],
    )?;
    Ok(())
}

/// Distinct solid fill color for extra window `i` (1-based).
fn extra_color(i: usize) -> u32 {
    const PALETTE: [u32; 3] = [0x00FF_00FF, 0x0000_FFFF, 0x00FF_FF00]; // magenta, cyan, yellow
    PALETTE[(i - 1) % PALETTE.len()]
}

fn draw_solid(
    conn: &impl Connection,
    win: Window,
    gc: Gcontext,
    color: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
    conn.poly_fill_rectangle(
        win,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        }],
    )?;
    Ok(())
}

fn draw_quadrants(
    conn: &impl Connection,
    win: Window,
    gc: Gcontext,
) -> Result<(), Box<dyn std::error::Error>> {
    let (hw, hh) = (WIDTH / 2, HEIGHT / 2);
    let cells: [(i16, i16, u16, u16, u32); 4] = [
        (0, 0, hw, hh, 0x00FF_0000),                 // TL red
        (hw as i16, 0, hw, hh, 0x0000_FF00),         // TR green
        (0, hh as i16, hw, hh, 0x0000_00FF),         // BL blue
        (hw as i16, hh as i16, hw, hh, 0x00FF_FFFF), // BR white
    ];
    for (x, y, w, h, color) in cells {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
        conn.poly_fill_rectangle(
            win,
            gc,
            &[Rectangle {
                x,
                y,
                width: w,
                height: h,
            }],
        )?;
    }
    Ok(())
}
