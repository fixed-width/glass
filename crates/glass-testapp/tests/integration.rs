//! End-to-end X11 tests. These are `#[ignore]`d so the default `cargo test`
//! stays green without a display; run them via `scripts/test-x11.sh`.

mod common;

use common::Xvfb;
use glass_core::{AppSpec, Platform};
use glass_x11::X11Platform;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

fn app_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// Drain logs repeatedly until `needle` appears or `tries` attempts elapse.
fn wait_for_log(p: &mut X11Platform, needle: &str, tries: u32) -> bool {
    for _ in 0..tries {
        for (_s, line) in p.drain_logs() {
            if line.contains(needle) {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn launches_testapp_and_finds_its_window() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let geom = p.start_app(&app_spec()).unwrap_or_else(|e| panic!("start_app failed: {e}"));
    assert_eq!(geom.width, 320);
    assert_eq!(geom.height, 240);
    assert!(wait_for_log(&mut p, "READY", 40), "never saw READY on stdout");
    p.stop_app().unwrap();
}

fn pixel(frame: &glass_core::Frame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * frame.width + x) * 4) as usize;
    [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2], frame.pixels[i + 3]]
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn capture_frame_with_region_returns_subrectangle() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    std::thread::sleep(std::time::Duration::from_millis(150));
    // A region inside the top-left red quadrant, captured straight from X.
    let region = glass_core::Region { x: 10, y: 10, width: 80, height: 60 };
    let frame = p.capture_frame(Some(&region)).unwrap();
    assert_eq!((frame.width, frame.height), (80, 60));
    assert_eq!(pixel(&frame, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&frame, 79, 59), [255, 0, 0, 255]);
    // Full capture (None) still works.
    let full = p.capture_frame(None).unwrap();
    assert_eq!((full.width, full.height), (320, 240));
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn captures_known_quadrant_colors() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    // Give the server a beat to process the first Expose/draw.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let frame = p.capture_frame(None).unwrap();
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 240);
    // Sample the center of each quadrant.
    assert_eq!(pixel(&frame, 80, 60), [255, 0, 0, 255], "TL should be red");
    assert_eq!(pixel(&frame, 240, 60), [0, 255, 0, 255], "TR should be green");
    assert_eq!(pixel(&frame, 80, 180), [0, 0, 255, 255], "BL should be blue");
    assert_eq!(pixel(&frame, 240, 180), [255, 255, 255, 255], "BR should be white");
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn click_is_delivered_to_the_window() {
    use glass_core::{MouseButton, PointerEvent};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    p.send_pointer(&PointerEvent::Click { x: 30, y: 40, button: MouseButton::Left, count: 1, modifiers: vec![] })
        .unwrap();
    // The fixture echoes: EVENT button=1 x=30 y=40
    assert!(
        wait_for_log(&mut p, "button=1 x=30 y=40", 40),
        "click not echoed at expected coords"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn drag_emits_continuous_motion() {
    use glass_core::{MouseButton, PointerEvent};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    p.send_pointer(&PointerEvent::Drag {
        from_x: 20,
        from_y: 20,
        to_x: 80,
        to_y: 20,
        button: MouseButton::Left,
        modifiers: vec![],
        duration_ms: 200,
    })
    .unwrap();
    // The fixture echoes "EVENT motion ..." for each button-held motion. A
    // teleporting drag emits ~1; an interpolated 60px drag emits many.
    let mut motions = 0;
    for _ in 0..40 {
        for (_s, line) in p.drain_logs() {
            if line.starts_with("EVENT motion") {
                motions += 1;
            }
        }
        if motions >= 10 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(motions >= 10, "expected many intermediate motion events, got {motions}");
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn resize_changes_geometry_and_is_observed() {
    use glass_core::WindowOp;
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    let geo = p.window(&WindowOp::Resize { width: 200, height: 150 }).unwrap();
    assert_eq!(geo.width, 200);
    assert_eq!(geo.height, 150);
    // The fixture echoes ConfigureNotify: EVENT configure w=200 h=150
    assert!(wait_for_log(&mut p, "configure w=200 h=150", 40), "no configure echo");
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn typed_text_and_chord_reach_the_window() {
    use glass_core::{KeyEvent, WindowOp};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    // Focus the window so XTEST key events are delivered to it.
    p.window(&WindowOp::Focus).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    // Type 'a' -> keysym 0x61 (97).
    p.send_key(&KeyEvent::Text("a".into())).unwrap();
    assert!(wait_for_log(&mut p, "keysym=97", 40), "did not receive 'a'");
    // Press Return chord -> keysym 0xff0d (65293).
    p.send_key(&KeyEvent::Chord("Return".into())).unwrap();
    assert!(wait_for_log(&mut p, "keysym=65293", 40), "did not receive Return");
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn discovers_reparented_window_via_net_client_list() {
    // Launched with --reparent, the fixture's window is reparented under a frame
    // (so it is NOT a root child) and listed in _NET_CLIENT_LIST. Discovery must
    // find it via the client list — the root-children scan alone would miss it.
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string(), "--reparent".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 2000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    let geom = p
        .start_app(&spec)
        .unwrap_or_else(|e| panic!("client-list discovery failed: {e}"));
    assert_eq!((geom.width, geom.height), (320, 240));
    assert!(wait_for_log(&mut p, "READY", 40), "never saw READY");
    let frame = p.capture_frame(None).unwrap();
    assert_eq!((frame.width, frame.height), (320, 240));
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn finds_window_by_class_when_no_net_wm_pid() {
    // The fixture launched with --no-wm-pid sets no _NET_WM_PID, like Xaw/legacy
    // apps (xcalc). Discovery must fall back to the class hint (WM_CLASS).
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string(), "--no-wm-pid".to_string()],
        cwd: None,
        env: vec![],
        window_hint: Some(glass_core::WindowHint {
            title: None,
            class: Some("glass-testapp".to_string()),
        }),
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    let geom = p
        .start_app(&spec)
        .unwrap_or_else(|e| panic!("class-based discovery failed: {e}"));
    assert_eq!((geom.width, geom.height), (320, 240));
    assert!(wait_for_log(&mut p, "READY", 40), "never saw READY");
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn crop_extracts_region_of_real_capture() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    std::thread::sleep(std::time::Duration::from_millis(150)); // let the first draw land
    let full = p.capture_frame(None).unwrap();

    // A region fully inside the top-left red quadrant.
    let red = full.crop(&glass_core::Region { x: 10, y: 10, width: 80, height: 60 }).unwrap();
    assert_eq!((red.width, red.height), (80, 60));
    assert_eq!(pixel(&red, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&red, 79, 59), [255, 0, 0, 255]);

    // A region straddling the vertical midline (x=160): left half red, right half green.
    let straddle = full.crop(&glass_core::Region { x: 120, y: 60, width: 80, height: 60 }).unwrap();
    assert_eq!(pixel(&straddle, 10, 30), [255, 0, 0, 255]); // src x≈130 -> red
    assert_eq!(pixel(&straddle, 70, 30), [0, 255, 0, 255]); // src x≈190 -> green

    // Out of bounds is rejected, not clamped.
    assert!(full.crop(&glass_core::Region { x: 0, y: 0, width: 999, height: 1 }).is_err());

    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn failed_start_kills_the_child_process() {
    use glass_core::GlassError;
    // A process that never maps a window -> discovery times out. It echoes its
    // own PID first so we can confirm glass killed it rather than orphaning it.
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec!["sh".to_string(), "-c".to_string(), "echo PIDLINE=$$; exec sleep 30".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 500,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    let err = p.start_app(&spec).unwrap_err();
    assert!(matches!(err, GlassError::Timeout(_)), "expected Timeout, got {err}");

    // Recover the child PID from its stdout (captured before it was killed).
    let mut pid = None;
    for _ in 0..40 {
        for (_s, line) in p.drain_logs() {
            if let Some(rest) = line.strip_prefix("PIDLINE=") {
                pid = rest.trim().parse::<u32>().ok();
            }
        }
        if pid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let pid = pid.expect("child never reported its PID");

    // start_app kills + reaps synchronously on failure, so /proc/<pid> is gone.
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "child {pid} was orphaned after a failed start"
    );
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn enumerates_and_selects_multiple_windows() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string(), "--windows".into(), "2".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).unwrap_or_else(|e| panic!("start_app failed: {e}"));
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    std::thread::sleep(std::time::Duration::from_millis(200)); // let both windows draw

    let windows = p.list_windows().unwrap();
    assert_eq!(windows.len(), 2, "expected 2 windows, got {windows:?}");

    let main = windows.iter().find(|w| w.title.as_deref() == Some("glass-testapp")).expect("main window");
    let extra = windows.iter().find(|w| w.title.as_deref() == Some("glass-testapp-1")).expect("extra window");
    assert_ne!(main.id, extra.id);

    // Select the extra window: it is a solid magenta fill.
    p.select_window(extra.id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let f = p.capture_frame(None).unwrap();
    assert_eq!(pixel(&f, 160, 120), [255, 0, 255, 255], "extra window should be solid magenta");

    // Select the main window: top-left quadrant is red.
    p.select_window(main.id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let f = p.capture_frame(None).unwrap();
    assert_eq!(pixel(&f, 80, 60), [255, 0, 0, 255], "main window TL should be red");

    // A bogus id is rejected.
    assert!(p.select_window(glass_core::WindowId(0xDEAD_BEEF)).is_err());

    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn modified_click_carries_modifier_state() {
    use glass_core::{Modifier, MouseButton, PointerEvent};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    // Ctrl held -> ButtonPress.state has ControlMask (4) on a clean headless X server.
    p.send_pointer(&PointerEvent::Click {
        x: 30,
        y: 40,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![Modifier::Control],
    })
    .unwrap();
    assert!(wait_for_log(&mut p, "state=4", 40), "ctrl not held during click");
    // Shift -> ShiftMask (1).
    p.send_pointer(&PointerEvent::Click {
        x: 30,
        y: 40,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![Modifier::Shift],
    })
    .unwrap();
    assert!(wait_for_log(&mut p, "state=1", 40), "shift not held during click");
    p.stop_app().unwrap();
}

/// `/proc`-based liveness (Linux; these tests are Linux-only by construction).
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_until_gone(pid: u32, tries: u32) -> bool {
    for _ in 0..tries {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !pid_alive(pid)
}

/// A bare `X11Platform` dropped WITHOUT `stop_app()` must still reap its launched
/// app — parity with the Wayland/Windows backends. Guards the X11 attach-mode
/// orphan leak (before this, Drop left the app running).
#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn dropping_platform_reaps_the_app() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "never saw READY on stdout");
    let pid = p.app_pid().expect("the X11 backend exposes the launched app pid");
    assert!(pid_alive(pid), "the app should be running before drop");
    drop(p); // deliberately no stop_app(): teardown must happen via Drop
    assert!(wait_until_gone(pid, 50), "app pid {pid} still alive after Drop — orphan leak");
}

// ---------------------------------------------------------------------------
// Clipboard tests
// ---------------------------------------------------------------------------

/// Minimal X11 CLIPBOARD owner: opens its own connection, owns CLIPBOARD with
/// `text`, serves exactly one SelectionRequest (TARGETS or UTF8_STRING), then
/// returns.  Used to simulate a foreign app having clipboard ownership.
fn serve_clipboard_once(display: &str, text: &str) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, screen_num) = x11rb::connect(Some(display)).expect("serve: connect");
    let root = conn.setup().roots[screen_num].root;
    let screen = &conn.setup().roots[screen_num];

    // Intern the atoms we need.
    let clipboard = conn.intern_atom(false, b"CLIPBOARD").unwrap().reply().unwrap().atom;
    let utf8 = conn.intern_atom(false, b"UTF8_STRING").unwrap().reply().unwrap().atom;
    let targets_atom = conn.intern_atom(false, b"TARGETS").unwrap().reply().unwrap().atom;

    // Create a window to own the selection.
    let win = conn.generate_id().unwrap();
    conn.create_window(
        0,
        win,
        root,
        0, 0, 1, 1,
        0,
        WindowClass::INPUT_ONLY,
        screen.root_visual,
        &CreateWindowAux::default(),
    ).unwrap().check().unwrap();

    // Take ownership of CLIPBOARD.
    conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME).unwrap().check().unwrap();
    conn.flush().unwrap();

    // Serve events: answer one SelectionRequest then exit.
    let mut served = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !served && std::time::Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(event)) => {
                use x11rb::protocol::Event;
                match event {
                    Event::SelectionRequest(req) => {
                        // Determine the reply property (use req.property or req.target as fallback).
                        let reply_prop = if req.property != x11rb::NONE {
                            req.property
                        } else {
                            req.target
                        };

                        if req.target == targets_atom {
                            let atoms: &[u32] = &[targets_atom, utf8];
                            conn.change_property32(
                                PropMode::REPLACE,
                                req.requestor,
                                reply_prop,
                                AtomEnum::ATOM,
                                atoms,
                            ).unwrap().check().unwrap();
                        } else if req.target == utf8 {
                            conn.change_property8(
                                PropMode::REPLACE,
                                req.requestor,
                                reply_prop,
                                utf8,
                                text.as_bytes(),
                            ).unwrap().check().unwrap();
                        } else {
                            // Unsupported target: refuse by setting property to None.
                            // We notify with property=NONE.
                            let notify = SelectionNotifyEvent {
                                response_type: 31,
                                sequence: 0,
                                time: req.time,
                                requestor: req.requestor,
                                selection: req.selection,
                                target: req.target,
                                property: x11rb::NONE,
                            };
                            conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)
                                .unwrap().check().unwrap();
                            conn.flush().unwrap();
                            continue;
                        }

                        // Send SelectionNotify to the requestor.
                        let notify = SelectionNotifyEvent {
                            response_type: 31,
                            sequence: 0,
                            time: req.time,
                            requestor: req.requestor,
                            selection: req.selection,
                            target: req.target,
                            property: reply_prop,
                        };
                        conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)
                            .unwrap().check().unwrap();
                        conn.flush().unwrap();
                        served = true;
                    }
                    Event::SelectionClear(_) => {
                        // Another owner took over; we're done.
                        served = true;
                    }
                    _ => {}
                }
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => break,
        }
    }

    conn.destroy_window(win).unwrap().check().ok();
    conn.flush().ok();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn clipboard_set_then_get_roundtrips() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.set_clipboard("glass-clip-hello").unwrap();
    assert_eq!(p.get_clipboard().unwrap(), "glass-clip-hello");
    p.stop_app().ok();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn clipboard_get_reads_a_foreign_owner() {
    let xvfb = Xvfb::start();
    // A second client owns CLIPBOARD with known UTF8_STRING text, serving SelectionRequest.
    let display = xvfb.display.clone();
    let owner = std::thread::spawn(move || serve_clipboard_once(&display, "from-other-app"));
    // Give the owner thread time to set up and take ownership before we request.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let result = p.get_clipboard().unwrap();
    assert_eq!(result, "from-other-app");
    owner.join().ok();
    p.stop_app().ok();
}

// ---------------------------------------------------------------------------
// Sandbox integration tests (bwrap + Xvfb)
// ---------------------------------------------------------------------------

/// Launch `glass-testapp` under `SandboxLevel::Default` — the app must find the
/// X display through the `/tmp/.X11-unix` bind and render a non-blank frame.
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn sandbox_default_app_still_runs_and_captures() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 8000,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    let geom = p.start_app(&spec).unwrap_or_else(|e| panic!("sandboxed start_app failed: {e}"));
    assert_eq!(geom.width, 320);
    assert_eq!(geom.height, 240);
    assert!(wait_for_log(&mut p, "READY", 60), "never saw READY on stdout (sandboxed)");
    std::thread::sleep(std::time::Duration::from_millis(150));
    let frame = p.capture_frame(None).unwrap();
    // At least one non-zero pixel proves the app rendered something.
    let non_zero = frame.pixels.iter().any(|&b| b != 0);
    assert!(non_zero, "captured frame is entirely zero (blank) — app may not have connected to X");
    p.stop_app().unwrap();
}

/// Under `Default` the sandbox HOME is an ephemeral tmpfs so a write to `$HOME`
/// from inside the namespace stays inside and is invisible on the real FS.
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn sandbox_off_build_step_writes_to_real_home() {
    let sentinel_name = format!("glass-sandbox-test-sentinel-off-{}.tmp", std::process::id());
    let real_home = std::env::var("HOME").expect("$HOME must be set");
    let sentinel_path = std::path::PathBuf::from(&real_home).join(&sentinel_name);
    let _ = std::fs::remove_file(&sentinel_path);

    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: Some(format!("touch $HOME/{sentinel_name}")),
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).unwrap_or_else(|e| panic!("off-sandbox start_app failed: {e}"));
    p.stop_app().unwrap();
    let exists = sentinel_path.exists();
    std::fs::remove_file(&sentinel_path).ok();
    assert!(exists, "Off sandbox: sentinel {sentinel_path:?} must exist (unconfined write)");
}

/// Under `Default` the sandbox HOME is an ephemeral tmpfs so a write to `$HOME`
/// from inside the namespace stays inside and is invisible on the real FS.
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn sandbox_default_build_step_cannot_write_real_home() {
    let sentinel_name = format!("glass-sandbox-test-sentinel-default-{}.tmp", std::process::id());
    let real_home = std::env::var("HOME").expect("$HOME must be set");
    let sentinel_path = std::path::PathBuf::from(&real_home).join(&sentinel_name);
    let _ = std::fs::remove_file(&sentinel_path);

    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: Some(format!("touch $HOME/{sentinel_name}")),
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 8000,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    p.start_app(&spec).unwrap_or_else(|e| panic!("default-sandbox start_app failed: {e}"));
    p.stop_app().unwrap();
    assert!(
        !sentinel_path.exists(),
        "Default sandbox: sentinel {sentinel_path:?} must NOT exist (ephemeral HOME)"
    );
}

/// Under `Strict` the build step runs with `--unshare-net` and cannot reach the
/// network (TCP connection attempt fails → non-zero exit → `start_app` errors).
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn strict_blocks_network_in_build_step() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        // Try a TCP connect to 1.1.1.1:53 (Cloudflare DNS); /dev/tcp requires bash.
        build: Some("bash -c 'exec 3<>/dev/tcp/1.1.1.1/53'".into()),
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 8000,
        sandbox: glass_core::SandboxLevel::Strict,
        a11y: false,
    };
    let err = p.start_app(&spec).expect_err("Strict sandbox should block network in build step");
    assert!(
        matches!(err, glass_core::GlassError::AppNotStarted(_)),
        "expected AppNotStarted (build failure), got {err}"
    );
}

/// Under `Default` the build step can reach the network.
// NOTE: this test requires outbound network egress on the host (passes on GitHub-hosted
// runners which have egress by default; would need adjusting for an egress-restricted runner).
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn default_allows_network_in_build_step() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: Some("bash -c 'exec 3<>/dev/tcp/1.1.1.1/53'".into()),
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 8000,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    p.start_app(&spec)
        .unwrap_or_else(|e| panic!("Default sandbox should allow network in build step: {e}"));
    p.stop_app().unwrap();
}

/// When `bwrap` is not on `PATH`, `start_app` with `Default` must return
/// `GlassError::SandboxUnavailable` (fail-closed gate).
///
/// Uses a RAII guard to restore `PATH` even on panic. Relies on
/// `--test-threads=1` to avoid races with other tests.
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn fail_closed_when_bwrap_missing() {
    struct PathGuard(std::ffi::OsString);
    impl Drop for PathGuard {
        fn drop(&mut self) {
            std::env::set_var("PATH", &self.0);
        }
    }

    let xvfb = Xvfb::start();
    let sandboxed_err = {
        let _guard = {
            let original = std::env::var_os("PATH").unwrap_or_default();
            std::env::set_var("PATH", "/nonexistent");
            PathGuard(original)
        };
        let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
        let spec = AppSpec {
            build: None,
            run: vec![TESTAPP.to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 5000,
            sandbox: glass_core::SandboxLevel::Default,
            a11y: false,
        };
        let err = p.start_app(&spec).err();
        // _guard restores PATH here before any panic-able assertion.
        err
    };
    assert!(
        matches!(sandboxed_err, Some(glass_core::GlassError::SandboxUnavailable(_))),
        "expected SandboxUnavailable, got {sandboxed_err:?}"
    );
}

/// Prove that `cwd == real $HOME` no longer re-exposes the real home directory.
///
/// We write a uniquely-named sentinel file into the real `$HOME`.  Then we
/// launch a build step (`cat` the sentinel) with `cwd = real $HOME` under
/// `SandboxLevel::Default`.  Before the fix, `--bind <cwd> <cwd>` re-mounted
/// the real HOME over the ephemeral tmpfs; the sentinel was readable and the
/// build succeeded.  After the fix the bind is skipped, the sentinel is hidden
/// by the tmpfs, and the build must fail → `start_app` returns an error.
///
/// The same build step succeeds under `SandboxLevel::Off` (no sandbox, no
/// tmpfs), confirming the sentinel itself is readable — we are testing
/// isolation, not a broken test.
#[test]
#[ignore = "requires an X server + bwrap; run via scripts/test-x11.sh"]
fn sandbox_cwd_equals_home_does_not_expose_real_home() {
    let real_home = std::env::var("HOME").expect("$HOME must be set");
    let sentinel_name = format!("glass_cwdhome_probe_{}.tmp", std::process::id());
    let sentinel_path = std::path::PathBuf::from(&real_home).join(&sentinel_name);

    // Write the sentinel to the real home so we have something to try to read.
    std::fs::write(&sentinel_path, b"secret").expect("write sentinel");

    // ---- Sanity check: Off sandbox, cwd=home — sentinel IS readable ----------
    {
        let xvfb = Xvfb::start();
        let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
        let spec = AppSpec {
            build: Some(format!("cat $HOME/{sentinel_name}")),
            run: vec![TESTAPP.to_string()],
            cwd: Some(std::path::PathBuf::from(&real_home)),
            env: vec![],
            window_hint: None,
            timeout_ms: 8000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        };
        p.start_app(&spec).unwrap_or_else(|e| {
            let _ = std::fs::remove_file(&sentinel_path);
            panic!("Off sandbox: build step should succeed (sentinel visible), got {e}");
        });
        p.stop_app().unwrap();
    }

    // ---- Real test: Default sandbox, cwd=home — sentinel must NOT be readable -
    {
        let xvfb = Xvfb::start();
        let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
        let spec = AppSpec {
            build: Some(format!("cat $HOME/{sentinel_name}")),
            run: vec![TESTAPP.to_string()],
            cwd: Some(std::path::PathBuf::from(&real_home)),
            env: vec![],
            window_hint: None,
            timeout_ms: 8000,
            sandbox: glass_core::SandboxLevel::Default,
            a11y: false,
        };
        let result = p.start_app(&spec);
        let _ = std::fs::remove_file(&sentinel_path);
        assert!(
            result.is_err(),
            "Default sandbox with cwd==HOME must NOT expose real home (build step \
             reading the sentinel should fail); got Ok — containment gap still open!"
        );
        assert!(
            matches!(result.unwrap_err(), glass_core::GlassError::AppNotStarted(_)),
            "expected AppNotStarted (build-step cat of sentinel failed), got a different error"
        );
    }
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn start_app_focuses_window_so_keys_reach_it() {
    use glass_core::KeyEvent;
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    // --no-self-focus: the fixture does NOT focus itself, so a key reaches it
    // only if start_app focused it. (The default fixture self-focuses, which
    // masks this gap — see the spec.)
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string(), "--no-self-focus".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    // No explicit window(Focus)/select_window: start_app must have focused it.
    p.send_key(&KeyEvent::Text("a".into())).unwrap();
    assert!(
        wait_for_log(&mut p, "keysym=97", 40),
        "key 'a' did not reach the window — start_app did not focus it"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn select_window_focuses_the_selected_window() {
    use glass_core::{KeyEvent, WindowOp};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    // Two windows, neither self-focusing. The MAIN quadrant window echoes key
    // presses (EVENT keysym=...); the EXTRA window has no KEY_PRESS mask, so it
    // is SILENT when focused — that silence is how we detect focus moved to it.
    let spec = AppSpec {
        build: None,
        run: vec![
            TESTAPP.to_string(),
            "--no-self-focus".to_string(),
            "--windows".to_string(),
            "2".to_string(),
        ],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");

    let wins = p.list_windows().unwrap();
    let main = wins
        .iter()
        .find(|w| w.title.as_deref() == Some("glass-testapp"))
        .expect("main window")
        .id;
    let extra = wins
        .iter()
        .find(|w| w.title.as_deref() == Some("glass-testapp-1"))
        .expect("extra window")
        .id;

    // Pin focus to MAIN deterministically via window(Focus), NOT select_window, so
    // the baseline doesn't depend on the fix under test and is known no matter which
    // window start_app happened to focus.
    p.select_window(main).unwrap();
    p.window(&WindowOp::Focus).unwrap();
    p.send_key(&KeyEvent::Text("a".into())).unwrap();
    assert!(
        wait_for_log(&mut p, "keysym=97", 40),
        "baseline: 'a' should reach the focused main window"
    );

    // Select the EXTRA (silent) window. If select_window focuses it, 'b' lands on
    // a window with no KEY_PRESS mask and is NOT echoed. If select_window does NOT
    // focus (the bug), MAIN stays focused and 'b' IS echoed.
    p.select_window(extra).unwrap();
    p.send_key(&KeyEvent::Text("b".into())).unwrap();
    assert!(
        !wait_for_log(&mut p, "keysym=98", 25),
        "'b' was echoed after selecting the extra (silent) window — select_window did not move keyboard focus"
    );

    // Selecting MAIN again must bring keys back.
    p.select_window(main).unwrap();
    p.send_key(&KeyEvent::Text("c".into())).unwrap();
    assert!(
        wait_for_log(&mut p, "keysym=99", 40),
        "select_window(main) did not restore keyboard focus"
    );
    p.stop_app().unwrap();
}

/// With `sandbox: Off`, `start_app` never checks for bwrap.
#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn sandbox_off_bypasses_bwrap_check() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).unwrap_or_else(|e| panic!("Off sandbox should not require bwrap: {e}"));
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn stop_app_reaps_the_apps_forked_child() {
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    let spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string(), "--fork-child".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).unwrap();
    let mut child_pid: Option<u32> = None;
    for _ in 0..40 {
        for (_s, line) in p.drain_logs() {
            if let Some(rest) = line.strip_prefix("EVENT child_pid=") {
                child_pid = rest.trim().parse().ok();
            }
        }
        if child_pid.is_some() { break; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let child_pid = child_pid.expect("fixture should report its forked child pid");
    assert!(
        std::path::Path::new(&format!("/proc/{child_pid}")).exists(),
        "forked child should be alive while the app runs"
    );
    p.stop_app().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !std::path::Path::new(&format!("/proc/{child_pid}")).exists(),
        "stop_app must reap the app's forked child (pid {child_pid}), not orphan it"
    );
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn drag_is_time_paced() {
    use glass_core::{MouseButton, PointerEvent};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    let t = std::time::Instant::now();
    p.send_pointer(&PointerEvent::Drag {
        from_x: 30, from_y: 30, to_x: 200, to_y: 200,
        button: MouseButton::Left, modifiers: vec![], duration_ms: 200,
    })
    .unwrap();
    let el = t.elapsed();
    assert!(
        el >= std::time::Duration::from_millis(150),
        "a paced 200ms drag should span ~200ms of wall-clock, took {el:?}"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
fn click_with_modifier_reaches_app() {
    use glass_core::{Modifier, MouseButton, PointerEvent};
    let xvfb = Xvfb::start();
    let mut p = X11Platform::connect(Some(&xvfb.display)).unwrap();
    p.start_app(&app_spec()).unwrap();
    assert!(wait_for_log(&mut p, "READY", 40), "no READY");
    p.send_pointer(&PointerEvent::Click {
        x: 50,
        y: 50,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![Modifier::Control],
    })
    .unwrap();
    // The fixture echoes "EVENT button=<d> x=.. y=.. state=<mask>" on ButtonPress.
    // X11 ControlMask is 0x04; if glass pressed Control before the button, it's set.
    let mut saw_ctrl = false;
    for _ in 0..40 {
        for (_s, line) in p.drain_logs() {
            if let Some(rest) = line.strip_prefix("EVENT button=") {
                if let Some(s) = rest.split("state=").nth(1) {
                    if s.trim().parse::<u16>().unwrap_or(0) & 0x04 != 0 {
                        saw_ctrl = true;
                    }
                }
            }
        }
        if saw_ctrl {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(saw_ctrl, "Control modifier (mask 0x04) not reflected on the button event");
    p.stop_app().unwrap();
}
