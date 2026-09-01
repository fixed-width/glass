//! End-to-end Wayland-backend tests. `#[ignore]`d; run via
//! `scripts/test-wayland.sh` (which skips if no glass-discoverable sway >=1.12).

#![cfg(target_os = "linux")]
// Two tests set `PATH` on the process to force a launch failure that has no other seam.
// `env::set_var` is `unsafe` from edition 2024 on (it races concurrent env readers);
// `scripts/test-wayland.sh` runs this suite with `--test-threads=1`, so there are none.
// Each site carries a `// SAFETY:` note; the file opts out of `unsafe_code = "deny"`.
#![allow(unsafe_code)]

use glass_core::{AppSpec, Backend, BaselineStore, Glass, GlassError, Platform, PlatformFactory};
use glass_wayland::WaylandPlatform;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

// drain_until polls every 50ms. Budgets are generous so slow/loaded CI runners
// (sway + Xwayland + the app cold-starting) don't time out spuriously.
const READY_TRIES: u32 = 300; // ~15s: app start under a freshly-spawned Xwayland
const ECHO_TRIES: u32 = 120; //  ~6s: input echoed back once the app is up
const APP_TIMEOUT_MS: u64 = 15_000; // start_app: wait this long for sway's socket

fn spec(run: Vec<String>, timeout_ms: u64) -> AppSpec {
    AppSpec {
        build: None,
        run,
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// `start_app`, but dump the captured sway/Xwayland/app logs before panicking on
/// failure. A failed launch otherwise surfaces only as a bare `Timeout(ms)` (the
/// window never reached sway's tree) with no clue why. The captured buffer holds
/// sway's and Xwayland's stderr plus the app's own stdout/stderr (all piped through
/// sway), so a CI flake here names which layer failed instead of being opaque.
fn start(p: &mut WaylandPlatform, spec: &AppSpec) -> glass_core::WindowGeometry {
    match p.start_app(spec) {
        Ok(geom) => geom,
        Err(e) => {
            eprintln!("\nstart_app failed: {e}\n--- captured sway/Xwayland/app logs ---");
            for (stream, line) in p.drain_logs() {
                eprintln!("  [{stream:?}] {line}");
            }
            eprintln!("--- end captured logs ---");
            panic!("start_app failed: {e}");
        }
    }
}

fn drain_until(p: &mut WaylandPlatform, needle: &str, tries: u32) -> bool {
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

/// Poll `list_windows` until at least `n` windows are present, or give up.
/// Xwayland maps a client's extra top-levels into sway's tree asynchronously,
/// so a freshly-launched multi-window app needs a moment to fully surface.
fn list_until(p: &mut WaylandPlatform, n: usize, tries: u32) -> Vec<glass_core::WindowInfo> {
    let mut last = Vec::new();
    for _ in 0..tries {
        last = p.list_windows().unwrap();
        if last.len() >= n {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    last
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn launches_app_and_reports_window_geometry() {
    let mut p = WaylandPlatform::new().unwrap();
    let geom = start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    // Per-window geometry: the floating glass-testapp at its natural 320x240,
    // not the 1280x720 headless output.
    assert_eq!((geom.width, geom.height), (320, 240));
    assert!(
        drain_until(&mut p, "READY", READY_TRIES),
        "glass-testapp READY never reached the logs"
    );
    p.stop_app().unwrap();
}

fn pixel(f: &glass_core::Frame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * f.width + x) * 4) as usize;
    [
        f.pixels[i],
        f.pixels[i + 1],
        f.pixels[i + 2],
        f.pixels[i + 3],
    ]
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn captures_active_window_pixels() {
    use glass_core::Region;
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    std::thread::sleep(std::time::Duration::from_millis(300)); // let the first draw land

    let frame = p.capture_frame(None).unwrap();
    // Per-window capture: just the app's 320x240 surface, filled with its quadrants.
    assert_eq!((frame.width, frame.height), (320, 240));
    assert_eq!(pixel(&frame, 80, 60), [255, 0, 0, 255], "TL red");
    assert_eq!(pixel(&frame, 240, 60), [0, 255, 0, 255], "TR green");
    assert_eq!(pixel(&frame, 80, 180), [0, 0, 255, 255], "BL blue");
    assert_eq!(pixel(&frame, 240, 180), [255, 255, 255, 255], "BR white");

    // Region capture over the red quadrant -> all red.
    let red = p
        .capture_frame(Some(&Region {
            x: 0,
            y: 0,
            width: 80,
            height: 60,
        }))
        .unwrap();
    assert_eq!((red.width, red.height), (80, 60));
    assert_eq!(pixel(&red, 40, 30), [255, 0, 0, 255]);

    // Non-origin region: green (top-right) quadrant -> all green. Proves the
    // source x/y offset, not just crop-at-origin.
    let green = p
        .capture_frame(Some(&Region {
            x: 160,
            y: 0,
            width: 80,
            height: 60,
        }))
        .unwrap();
    assert_eq!((green.width, green.height), (80, 60));
    assert_eq!(pixel(&green, 40, 30), [0, 255, 0, 255]);

    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn click_reaches_the_app() {
    use glass_core::{MouseButton, PointerEvent};
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    p.send_pointer(&PointerEvent::Click {
        x: 30,
        y: 40,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    })
    .unwrap();
    assert!(
        drain_until(&mut p, "button=1 x=30 y=40", ECHO_TRIES),
        "click not echoed at (30,40)"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn drag_emits_continuous_motion() {
    use glass_core::{MouseButton, PointerEvent};
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
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
    let mut motions = 0;
    for _ in 0..ECHO_TRIES {
        for (_s, line) in p.drain_logs() {
            if line.starts_with("EVENT motion") {
                motions += 1;
            }
        }
        if motions >= 10 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(motions >= 10, "expected many motion events, got {motions}");
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn scroll_reaches_the_app() {
    use glass_core::PointerEvent;
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    // A downward wheel step. Xwayland maps wl_pointer vertical axis to X11 button 5.
    p.send_pointer(&PointerEvent::Scroll {
        x: 30,
        y: 40,
        dx: 0,
        dy: 1,
        modifiers: vec![],
    })
    .unwrap();
    assert!(
        drain_until(&mut p, "button=5", ECHO_TRIES),
        "scroll-down not echoed as wheel button 5"
    );
    p.stop_app().unwrap();
}

/// PIDs of all running sway compositor processes (empty if `pgrep` is
/// unavailable). Matches both a system `sway` and the glass bundle's `sway.real`
/// (the bundle's `sway` is a wrapper script that `exec`s `sway.real`).
fn sway_pids() -> std::collections::HashSet<u32> {
    std::process::Command::new("pgrep")
        .args(["-x", r"sway(\.real)?"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Wait for the fixture's `EVENT pid=<n>` startup line and return it, so a teardown test can
/// check the app process itself is gone (the compositor's window list can't answer that).
fn wait_for_app_pid(p: &mut WaylandPlatform) -> u32 {
    for _ in 0..READY_TRIES {
        for (_s, line) in p.drain_logs() {
            if let Some(pid) = line
                .strip_prefix("EVENT pid=")
                .and_then(|r| r.trim().parse().ok())
            {
                return pid;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("fixture never reported its pid");
}

/// Teardown must ask the app to close before signalling it: a signalled toolkit app runs no
/// shutdown path at all (see `glass_proc_linux::CLOSE_GRACE`), so state it would flush on exit is
/// lost. Under Wayland the ask goes through sway, which sends the client a close request; the
/// fixture prints `EVENT closed_gracefully` from the handler that only runs when it was asked.
#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn stop_app_asks_the_app_to_close_before_signalling_it() {
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    let pid = wait_for_app_pid(&mut p);
    p.stop_app().unwrap();
    // The fixture writes the line just before exiting; give the reader thread a moment to pick
    // it up off the pipe (it travels via sway's stdout, one hop further than under X11).
    std::thread::sleep(std::time::Duration::from_millis(300));
    let logs: Vec<String> = p.drain_logs().into_iter().map(|(_s, l)| l).collect();
    assert!(
        logs.iter().any(|l| l.contains("EVENT closed_gracefully")),
        "stop_app must ask the app to close so its shutdown path runs; got: {logs:?}"
    );
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the app must still be gone after a graceful close (pid {pid})"
    );
}

/// Teardown must reap what the app forked, not just what dies with the display.
///
/// This is the test that binds the signal actually reaching the launch. sway `setsid`s every app
/// it `exec`s, so the app is in neither the compositor's process group nor its session; a
/// teardown that signals only that group still *looks* correct, because a display client dies on
/// its own when its compositor goes away. A forked child that never talks to the display does
/// not — it is left running with `ppid 1`.
#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn stop_app_reaps_the_apps_forked_child() {
    let mut p = WaylandPlatform::new().unwrap();
    start(
        &mut p,
        &spec(
            vec![TESTAPP.to_string(), "--fork-child".to_string()],
            APP_TIMEOUT_MS,
        ),
    );
    let mut child_pid: Option<u32> = None;
    for _ in 0..READY_TRIES {
        for (_s, line) in p.drain_logs() {
            if let Some(rest) = line.strip_prefix("EVENT child_pid=") {
                child_pid = rest.trim().parse().ok();
            }
        }
        if child_pid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let child_pid = child_pid.expect("fixture should report its forked child pid");
    assert!(
        std::path::Path::new(&format!("/proc/{child_pid}")).exists(),
        "the forked child should be alive while the app runs"
    );
    p.stop_app().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !std::path::Path::new(&format!("/proc/{child_pid}")).exists(),
        "stop_app must reap the app's forked child (pid {child_pid}), not orphan it"
    );
}

/// An app that was asked and does not go must be signalled once the grace runs out, and the
/// whole teardown must still fit the budget glass-mcp allows it.
#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn stop_app_signals_an_app_that_ignores_the_close_request() {
    let mut p = WaylandPlatform::new().unwrap();
    start(
        &mut p,
        &spec(
            vec![TESTAPP.to_string(), "--ignore-close".to_string()],
            APP_TIMEOUT_MS,
        ),
    );
    let pid = wait_for_app_pid(&mut p);
    let t = std::time::Instant::now();
    p.stop_app().unwrap();
    let elapsed = t.elapsed();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let logs: Vec<String> = p.drain_logs().into_iter().map(|(_s, l)| l).collect();
    assert!(
        logs.iter()
            .any(|l| l.contains("EVENT ignoring_close_request")),
        "the compositor should have passed the close request to the app; got: {logs:?}"
    );
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "an app that ignores the request must still be signalled (pid {pid} survived)"
    );
    assert!(
        elapsed >= glass_proc_linux::CLOSE_GRACE,
        "the app must get the whole close grace before being signalled (took {elapsed:?})"
    );
    assert!(
        elapsed < glass_core::TEARDOWN_BUDGET,
        "teardown must fit in the budget glass-mcp gives it ({:?}); this took {elapsed:?}",
        glass_core::TEARDOWN_BUDGET
    );
}

/// A client that never opted into `WM_DELETE_WINDOW` cannot be asked, and under Wayland glass
/// cannot tell: sway acknowledges the `kill` command either way, and wlroots closes the X
/// connection of a client that has no close protocol. What teardown must still guarantee is
/// what this pins — the app is gone, its shutdown path did not run, and no compositor is left
/// behind. (The X11 backend, which reads `WM_PROTOCOLS` itself, does report this case.)
#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn stop_app_leaves_nothing_behind_when_the_app_cannot_be_asked() {
    let before = sway_pids();
    let mut p = WaylandPlatform::new().unwrap();
    start(
        &mut p,
        &spec(
            vec![TESTAPP.to_string(), "--no-wm-delete".to_string()],
            APP_TIMEOUT_MS,
        ),
    );
    let pid = wait_for_app_pid(&mut p);
    p.stop_app().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let logs: Vec<String> = p.drain_logs().into_iter().map(|(_s, l)| l).collect();
    assert!(
        !logs.iter().any(|l| l.contains("EVENT closed_gracefully")),
        "an app with no WM_DELETE_WINDOW protocol cannot have closed gracefully; got: {logs:?}"
    );
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the app must be gone after teardown (pid {pid})"
    );
    let leaked: Vec<_> = sway_pids().difference(&before).copied().collect();
    assert!(
        leaked.is_empty(),
        "teardown leaked sway process(es): {leaked:?}"
    );
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn start_app_failure_leaves_no_orphan() {
    let before = sway_pids();
    let mut p = WaylandPlatform::new().unwrap();
    // `true` exits ~immediately and never maps a window. sway (the compositor)
    // keeps running, so window discovery times out and start_app fails
    // (AppExited / Timeout / Backend). Every failure path kills+waits the sway
    // child; the invariant under test is that none of them leaks a sway process.
    match p.start_app(&spec(vec!["true".to_string()], 1500)) {
        Ok(_) => p.stop_app().unwrap(),
        Err(e) => assert!(
            matches!(
                e,
                GlassError::AppExited(_) | GlassError::Timeout(_) | GlassError::Backend(_)
            ),
            "unexpected error variant: {e}"
        ),
    }
    std::thread::sleep(std::time::Duration::from_millis(200)); // let a killed sway leave the table
    let after = sway_pids();
    let leaked: Vec<_> = after.difference(&before).collect();
    assert!(
        leaked.is_empty(),
        "start_app leaked sway process(es): {leaked:?}"
    );
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn types_text_reaches_the_app() {
    use glass_core::KeyEvent;
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    // Type 'a' -> keysym 0x61 (97), mirroring the X11 keyboard test.
    p.send_key(&KeyEvent::Text("a".into())).unwrap();
    assert!(
        drain_until(&mut p, "keysym=97", ECHO_TRIES),
        "typed 'a' not echoed"
    );
    p.stop_app().unwrap();
}

/// Drain logs, collecting every `keysym=N` value in arrival order until `want` of them have
/// arrived or `tries` polls elapse — so a test can assert the *full* delivered sequence and
/// catch dropped, reordered, or collapsed keystrokes.
fn collect_keysyms(p: &mut WaylandPlatform, want: usize, tries: u32) -> Vec<u32> {
    let mut got = Vec::new();
    for _ in 0..tries {
        for (_s, line) in p.drain_logs() {
            if let Some((_, n)) = line.split_once("keysym=")
                && let Ok(ks) = n.trim().parse::<u32>()
            {
                got.push(ks);
            }
        }
        if got.len() >= want {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    got
}

/// Whether `got` is `expected` with zero or more elements removed (order preserved) — the
/// signature of *dropped* keystrokes, as opposed to a *substituted* (wrong or extra) one.
fn is_subsequence(got: &[u32], expected: &[u32]) -> bool {
    let mut expected = expected.iter();
    got.iter().all(|g| expected.any(|e| e == g))
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn typed_multichar_strings_arrive_intact() {
    use glass_core::KeyEvent;
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");

    // The same shapes that broke the Windows backend: runs of adjacent identical characters
    // and spaces. For ASCII printables the keysym equals the char code, so each character
    // must arrive exactly once, in order — no drops, no collapse to the last char. Repeated
    // to shake out any timing race (the Linux analog of the Windows on-box rigor).
    // Two distinct flakes can hit Wayland typing under load, with distinguishable signatures:
    //   * a wrong or extra keysym (a *substitution*) — the keysym-mapping bug this test guards.
    //     Never retry it away: the substitution race is probabilistic, so retrying would give a
    //     re-introduced mapping bug a second chance and let it slip through. Fail on sight.
    //   * a *dropped* keystroke — the received sequence is `expected` with keys missing (a strict
    //     subsequence). Rare, nondeterministic, immune to pacing, and clears on a resend, so it
    //     may be retried once.
    let mut retries = 0u32;
    for _ in 0..3 {
        for s in ["aaa bbb ccc", "hello world", "the quick brown fox"] {
            let expected: Vec<u32> = s.chars().map(|c| c as u32).collect();

            let type_once = |p: &mut WaylandPlatform| -> Vec<u32> {
                let _ = p.drain_logs(); // clear anything pending before this string
                p.send_key(&KeyEvent::Text(s.to_string())).unwrap();
                collect_keysyms(p, expected.len(), ECHO_TRIES)
            };

            let got = type_once(&mut p);
            if got == expected {
                continue;
            }
            // Anything that isn't a pure drop is a real corruption — fail immediately, on the
            // first attempt, so the substitution regression can never be retried into a pass.
            assert!(
                got.len() < expected.len() && is_subsequence(&got, &expected),
                "typing {s:?} corrupted on Wayland (wrong keysym, not a dropped one): \
                 got {got:?}, expected {expected:?}"
            );
            eprintln!("wayland dropped a keystroke typing {s:?} (got {got:?}); retrying once");
            retries += 1;
            let got = type_once(&mut p);
            assert_eq!(
                got, expected,
                "typing {s:?} did not arrive intact on Wayland after a retry"
            );
        }
    }
    // The drop flake is rare on a healthy compositor (well under one send in nine); needing
    // several retries in one run means typing reliability has regressed, so fail loudly even
    // though every string eventually matched.
    assert!(
        retries <= 2,
        "Wayland typing needed {retries} retries across 9 sends — dropped-keystroke rate regressed"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn chord_reaches_the_app() {
    use glass_core::KeyEvent;
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    // Press the Return chord -> keysym 0xff0d (65293).
    p.send_key(&KeyEvent::Chord("Return".into())).unwrap();
    assert!(
        drain_until(&mut p, "keysym=65293", ECHO_TRIES),
        "Return chord not echoed"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn enumerates_selects_and_captures_multiple_windows() {
    use glass_core::WindowId;
    let mut p = WaylandPlatform::new().unwrap();
    start(
        &mut p,
        &spec(
            vec![TESTAPP.to_string(), "--windows".into(), "2".into()],
            APP_TIMEOUT_MS,
        ),
    );
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");

    // The 2nd Xwayland toplevel surfaces in sway's tree asynchronously; poll.
    let windows = list_until(&mut p, 2, READY_TRIES);
    assert_eq!(windows.len(), 2, "expected 2 windows, got {windows:?}");
    let main = windows
        .iter()
        .find(|w| w.title.as_deref() == Some("glass-testapp"))
        .expect("main window");
    let extra = windows
        .iter()
        .find(|w| w.title.as_deref() == Some("glass-testapp-1"))
        .expect("extra window");
    // Two distinct, separately-addressable windows, each at its natural size.
    // (sway centers both floating toplevels, so their screen rects coincide;
    // the real distinctness is id + title + per-window captured content below.)
    assert_ne!(main.id, extra.id);
    assert_eq!(
        (main.geometry.width, main.geometry.height),
        (320, 240),
        "main natural size"
    );
    assert_eq!(
        (extra.geometry.width, extra.geometry.height),
        (320, 240),
        "extra natural size"
    );
    let (main_id, extra_id) = (main.id, extra.id);

    p.select_window(extra_id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let f = p.capture_frame(None).unwrap();
    assert_eq!(
        pixel(&f, 160, 120),
        [255, 0, 255, 255],
        "extra window is solid magenta"
    );

    p.select_window(main_id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let f = p.capture_frame(None).unwrap();
    assert_eq!(
        pixel(&f, 80, 60),
        [255, 0, 0, 255],
        "main window TL quadrant is red"
    );

    assert!(p.select_window(WindowId(0xDEAD_BEEF)).is_err());
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn modified_click_carries_modifier_state() {
    use glass_core::{Modifier, MouseButton, PointerEvent};
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    // Ctrl held -> the Xwayland app's ButtonPress.state has ControlMask (4).
    p.send_pointer(&PointerEvent::Click {
        x: 30,
        y: 40,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![Modifier::Control],
    })
    .unwrap();
    assert!(
        drain_until(&mut p, "state=4", ECHO_TRIES),
        "ctrl not held during click"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn resize_and_move_change_geometry() {
    use glass_core::WindowOp;
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");

    // Resize the (floating) window via sway IPC; geometry reflects the new size.
    let resized = p
        .window(&WindowOp::Resize {
            width: 500,
            height: 360,
        })
        .unwrap();
    assert_eq!(
        (resized.width, resized.height),
        (500, 360),
        "resize geometry"
    );
    // The fixture (under Xwayland) echoes the ConfigureNotify.
    assert!(
        drain_until(&mut p, "configure w=500 h=360", ECHO_TRIES),
        "no configure echo"
    );

    // Move it; geometry reflects the new output-absolute origin, size preserved.
    let moved = p.window(&WindowOp::Move { x: 120, y: 90 }).unwrap();
    assert_eq!(
        (moved.x, moved.y, moved.width, moved.height),
        (120, 90, 500, 360),
        "move geometry"
    );

    // The Geometry op re-reads the live rect.
    let geo = p.window(&WindowOp::Geometry).unwrap();
    assert_eq!(
        (geo.x, geo.y, geo.width, geo.height),
        (120, 90, 500, 360),
        "geometry op"
    );
    p.stop_app().unwrap();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn clipboard_set_then_get_roundtrips() {
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    p.set_clipboard("glass-wl-clip").unwrap();
    assert_eq!(p.get_clipboard().unwrap(), "glass-wl-clip");
    p.stop_app().ok();
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn clipboard_get_with_no_selection_returns_empty() {
    // Nothing has set the selection on this freshly-spawned compositor, so no data-control offer
    // exists. `get_clipboard` must report an empty clipboard (Ok("")), not error.
    let mut p = WaylandPlatform::new().unwrap();
    start(&mut p, &spec(vec![TESTAPP.to_string()], APP_TIMEOUT_MS));
    assert!(drain_until(&mut p, "READY", READY_TRIES), "no READY");
    assert_eq!(p.get_clipboard().unwrap(), "");
    p.stop_app().ok();
}

// ---------------------------------------------------------------------------
// Sandbox integration tests (bwrap + sway)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ArtifactBackend {
    Wayland,
    Xwayland,
}

struct RunningPlatform(Option<WaylandPlatform>);

impl Drop for RunningPlatform {
    fn drop(&mut self) {
        if let Some(platform) = self.0.as_mut() {
            let _ = platform.stop_app();
        }
    }
}

fn assert_artifact_reads_denied(backend: ArtifactBackend, level: glass_core::SandboxLevel) {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let artifacts = temp.path().join("artifacts");
    let marker = artifacts.join("marker");
    let lease = temp.path().join("artifacts.lease");
    std::fs::create_dir(&artifacts).unwrap();
    std::fs::write(&marker, b"wayland-host-marker-7f1d").unwrap();
    std::fs::write(&lease, b"wayland-host-lease-3a92").unwrap();

    let proc_root_marker = std::path::PathBuf::from("/proc")
        .join(std::process::id().to_string())
        .join("root")
        .join(marker.strip_prefix("/").unwrap());
    let probe = temp.path().join("artifact-probe.sh");
    std::fs::write(
        &probe,
        b"#!/bin/sh\nfor item in ordinary:$1 proc_root:$2 lease:$3; do label=${item%%:*}; path=${item#*:}; if content=$(cat -- \"$path\" 2>/dev/null); then printf 'ARTIFACT_%s_ALLOWED:%s\\n' \"$label\" \"$content\"; else printf 'ARTIFACT_%s_DENIED\\n' \"$label\"; fi; done\nexec \"$4\" \"$5\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();

    let fixture = std::fs::canonicalize(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../glass-a11y-linux/tests/fixtures/a11y_fixture.py"
    ))
    .unwrap();
    let gdk_backend = match backend {
        ArtifactBackend::Wayland => "wayland",
        ArtifactBackend::Xwayland => "x11",
    };
    let mut platform = WaylandPlatform::new().unwrap();
    assert_eq!(
        platform
            .configure_protected_host_paths(&[
                glass_core::ProtectedHostPath::directory(&artifacts),
                glass_core::ProtectedHostPath::file(&lease),
            ])
            .unwrap(),
        glass_core::HostPathProtectionMode::SandboxRules
    );
    let spec = AppSpec {
        build: None,
        run: vec![
            probe.into_os_string().into_string().unwrap(),
            marker.clone().into_os_string().into_string().unwrap(),
            proc_root_marker.into_os_string().into_string().unwrap(),
            lease.clone().into_os_string().into_string().unwrap(),
            "python3".into(),
            fixture.into_os_string().into_string().unwrap(),
        ],
        cwd: Some(temp.path().to_path_buf()),
        env: vec![
            ("GDK_BACKEND".into(), gdk_backend.into()),
            ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
        ],
        window_hint: None,
        timeout_ms: APP_TIMEOUT_MS,
        sandbox: level,
        a11y: false,
    };
    if let Err(error) = platform.start_app(&spec) {
        panic!(
            "contained artifact probe failed: {error}; logs: {:?}",
            platform.drain_logs()
        );
    }
    let mut running = RunningPlatform(Some(platform));
    let platform = running.0.as_mut().unwrap();
    let mut results = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline && results.len() < 3 {
        for (_, line) in platform.drain_logs() {
            assert!(
                !line.contains("_ALLOWED:"),
                "target read a protected path: {line}"
            );
            assert!(!line.contains("wayland-host-marker-7f1d"));
            assert!(!line.contains("wayland-host-lease-3a92"));
            if let Some(label) = line
                .strip_prefix("ARTIFACT_")
                .and_then(|line| line.strip_suffix("_DENIED"))
            {
                results.insert(label.to_string());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        results,
        ["ordinary", "proc_root", "lease"].map(String::from).into()
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"wayland-host-marker-7f1d");
    assert_eq!(std::fs::read(&lease).unwrap(), b"wayland-host-lease-3a92");
    platform.stop_app().unwrap();
    running.0 = None;
}

#[test]
#[ignore = "requires sway + native Wayland GTK fixture + bwrap; run via scripts/test-wayland.sh artifact"]
fn artifact_paths_are_hidden_from_default_wayland_target() {
    assert_artifact_reads_denied(ArtifactBackend::Wayland, glass_core::SandboxLevel::Default);
}

#[test]
#[ignore = "requires sway + native Wayland GTK fixture + bwrap; run via scripts/test-wayland.sh artifact"]
fn artifact_paths_are_hidden_from_strict_wayland_target() {
    assert_artifact_reads_denied(ArtifactBackend::Wayland, glass_core::SandboxLevel::Strict);
}

#[test]
#[ignore = "requires sway + Xwayland GTK fixture + bwrap; run via scripts/test-wayland.sh artifact"]
fn artifact_paths_are_hidden_from_default_xwayland_target() {
    assert_artifact_reads_denied(ArtifactBackend::Xwayland, glass_core::SandboxLevel::Default);
}

#[test]
#[ignore = "requires sway + Xwayland GTK fixture + bwrap; run via scripts/test-wayland.sh artifact"]
fn artifact_paths_are_hidden_from_strict_xwayland_target() {
    assert_artifact_reads_denied(ArtifactBackend::Xwayland, glass_core::SandboxLevel::Strict);
}

#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh artifact"]
fn invalid_artifact_path_fails_before_wayland_target_launch() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let launched = temp.path().join("launched");
    let launch_script = temp.path().join("launch.sh");
    std::fs::write(&launch_script, b"#!/bin/sh\ntouch \"$1\"\nexec \"$2\"\n").unwrap();
    std::fs::set_permissions(&launch_script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let factory: PlatformFactory =
        Box::new(|_backend| Ok(Backend::display_only(Box::new(WaylandPlatform::new()?))));
    let mut glass = Glass::new(
        factory,
        "wayland".into(),
        BaselineStore::new(temp.path().join("baselines")),
        100,
    );
    glass
        .set_protected_host_paths(vec![glass_core::ProtectedHostPath::file(
            temp.path().join("missing"),
        )])
        .unwrap();
    let spec = AppSpec {
        run: vec![
            launch_script.into_os_string().into_string().unwrap(),
            launched.clone().into_os_string().into_string().unwrap(),
            TESTAPP.into(),
        ],
        timeout_ms: 5000,
        ..spec(vec![TESTAPP.into()], 5000)
    };

    let error = glass
        .start(&spec)
        .expect_err("missing protected path must prevent launch");
    assert!(matches!(error, GlassError::SandboxUnavailable(_)));
    assert!(!launched.exists());
}

/// Launch `glass-testapp` under `SandboxLevel::Default` inside the sway
/// compositor. The app must reach the Wayland socket (runtime_dir rw-bind) and
/// the binary itself (binary ro-bind when it is under $HOME), render a window,
/// and produce a non-blank capture.
#[test]
#[ignore = "requires sway + bwrap; run via scripts/test-wayland.sh"]
fn sandbox_default_app_still_runs_and_captures() {
    let mut p = WaylandPlatform::new().unwrap();
    let sandboxed_spec = AppSpec {
        build: None,
        run: vec![TESTAPP.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: APP_TIMEOUT_MS,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    let geom = start(&mut p, &sandboxed_spec);
    assert_eq!((geom.width, geom.height), (320, 240), "window geometry");
    assert!(
        drain_until(&mut p, "READY", READY_TRIES),
        "never saw READY (sandboxed)"
    );
    std::thread::sleep(std::time::Duration::from_millis(300)); // let the first draw land
    let frame = p.capture_frame(None).unwrap();
    // At least one non-zero pixel proves the app rendered something — the
    // runtime_dir rw-bind and binary ro-bind are working inside the namespace.
    let non_zero = frame.pixels.iter().any(|&b| b != 0);
    assert!(
        non_zero,
        "captured frame is entirely zero (blank) — app did not connect to Wayland"
    );
    p.stop_app().unwrap();
}

/// When `bwrap` is not on `PATH`, `start_app` with `Default` must return
/// `GlassError::SandboxUnavailable` — the fail-closed gate works on Wayland too.
///
/// Uses a RAII guard to restore `PATH` even on panic. Relies on
/// `--test-threads=1` to avoid races with other tests.
#[test]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
fn fail_closed_when_bwrap_missing_wayland() {
    struct PathGuard(std::ffi::OsString);
    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: `--test-threads=1` (see the header) — no concurrent env reader.
            unsafe { std::env::set_var("PATH", &self.0) };
        }
    }

    let sandboxed_err = {
        let _guard = {
            let original = std::env::var_os("PATH").unwrap_or_default();
            // SAFETY: `--test-threads=1` (see the header) — no concurrent env reader.
            unsafe { std::env::set_var("PATH", "/nonexistent") };
            PathGuard(original)
        };
        let mut p = WaylandPlatform::new().unwrap();
        let sandboxed_spec = AppSpec {
            build: None,
            run: vec![TESTAPP.to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: APP_TIMEOUT_MS,
            sandbox: glass_core::SandboxLevel::Default,
            a11y: false,
        };
        let err = p.start_app(&sandboxed_spec).err();
        // _guard restores PATH here before any panic-able assertion.
        err
    };
    assert!(
        matches!(
            sandboxed_err,
            Some(glass_core::GlassError::SandboxUnavailable(_))
        ),
        "expected SandboxUnavailable, got {sandboxed_err:?}"
    );
}

/// Wayland twin of the X11 `sandbox_default_reaches_launch_target_via_argument_path`
/// test: the reported bug reproduced on the other Linux backend, which shares the same
/// `launch_ro_binds` fix. A launch target reached only through an **argument**
/// (`run[1]`), not `run[0]` itself, must still be reachable under the default sandbox.
/// See the X11 test in `tests/integration.rs` for the full rationale — the fixture
/// shape (temp dir + sibling testapp copy + `run.sh`) is identical.
#[test]
#[ignore = "requires sway + bwrap; run via scripts/test-wayland.sh"]
fn sandbox_default_reaches_launch_target_via_argument_path() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("create temp dir");
    let testapp_copy = dir.path().join("glass-testapp");
    std::fs::copy(TESTAPP, &testapp_copy).expect("copy glass-testapp into temp dir");

    let run_sh = dir.path().join("run.sh");
    std::fs::write(
        &run_sh,
        format!("#!/bin/sh\nexec \"{}\"\n", testapp_copy.display()),
    )
    .expect("write run.sh");
    // `fs::copy` preserves the source's executable bit, but `fs::write` does not —
    // the wrapper script needs it set explicitly.
    std::fs::set_permissions(&run_sh, std::fs::Permissions::from_mode(0o755))
        .expect("chmod +x run.sh");

    let mut p = WaylandPlatform::new().unwrap();
    let sandboxed_spec = AppSpec {
        build: None,
        run: vec!["sh".to_string(), run_sh.to_string_lossy().into_owned()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: APP_TIMEOUT_MS,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    let geom = start(&mut p, &sandboxed_spec);
    assert_eq!(
        (geom.width, geom.height),
        (320, 240),
        "arg-path launch target unreachable under the default sandbox"
    );
    assert!(
        drain_until(&mut p, "READY", READY_TRIES),
        "never saw READY (sandboxed, arg-path launch)"
    );
    p.stop_app().unwrap();
}

/// Wayland twin of the X11 `sandbox_default_reaches_relative_launch_token_with_defaulted_cwd`.
/// A relative launch token with `cwd: None` resolves against glass's DEFAULTED cwd (its own
/// working directory), which is `--chdir`'d and rw-bound into the namespace. See the X11 test for
/// the full rationale; the fixture and the shared `launch_ro_binds` / cwd-default wiring are
/// identical. Load-bearing on the same code path (verified on X11 by stubbing the cwd default).
#[test]
#[ignore = "requires sway + bwrap; run via scripts/test-wayland.sh"]
fn sandbox_default_reaches_relative_launch_token_with_defaulted_cwd() {
    use std::os::unix::fs::PermissionsExt;

    struct CwdGuard(std::path::PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    let dir = tempfile::tempdir().expect("create temp dir");
    let testapp_copy = dir.path().join("glass-testapp");
    std::fs::copy(TESTAPP, &testapp_copy).expect("copy glass-testapp into temp dir");
    let run_sh = dir.path().join("run.sh");
    std::fs::write(
        &run_sh,
        format!("#!/bin/sh\nexec \"{}\"\n", testapp_copy.display()),
    )
    .expect("write run.sh");
    std::fs::set_permissions(&run_sh, std::fs::Permissions::from_mode(0o755))
        .expect("chmod +x run.sh");

    let original_cwd = std::env::current_dir().expect("read cwd");
    let _cwd_guard = CwdGuard(original_cwd);
    std::env::set_current_dir(dir.path()).expect("chdir into temp dir");

    let mut p = WaylandPlatform::new().unwrap();
    let sandboxed_spec = AppSpec {
        build: None,
        run: vec!["sh".to_string(), "./run.sh".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: APP_TIMEOUT_MS,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    let geom = start(&mut p, &sandboxed_spec);
    assert_eq!(
        (geom.width, geom.height),
        (320, 240),
        "relative launch token unreachable with a defaulted cwd"
    );
    assert!(
        drain_until(&mut p, "READY", READY_TRIES),
        "never saw READY (relative token, defaulted cwd)"
    );
    p.stop_app().unwrap();
}

/// Wayland twin of the X11 `sandbox_default_reaches_bare_name_program_on_a_shadowed_path_dir`.
/// A bare-name program resolvable only via a `PATH` dir under a shadowed root must launch under the
/// default sandbox because `launch_ro_binds` resolves `run[0]` against `$PATH` and binds the
/// resolving directory. See the X11 test for the full rationale. Load-bearing on the same shared
/// code path (verified on X11 by stubbing the resolution bind).
#[test]
#[ignore = "requires sway + bwrap; run via scripts/test-wayland.sh"]
fn sandbox_default_reaches_bare_name_program_on_a_shadowed_path_dir() {
    use std::os::unix::fs::PermissionsExt;

    struct PathGuard(std::ffi::OsString);
    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: `--test-threads=1` (see the header) — no concurrent env reader.
            unsafe { std::env::set_var("PATH", &self.0) };
        }
    }

    let dir = tempfile::tempdir().expect("create temp dir"); // under /tmp (tmpfs-shadowed)
    let bindir = dir.path().join("bin");
    std::fs::create_dir_all(&bindir).expect("create bin dir");
    let tool = bindir.join("glass-testapp-bare");
    std::fs::copy(TESTAPP, &tool).expect("copy glass-testapp as a bare-name tool");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod +x tool");

    let mut p = WaylandPlatform::new().unwrap();

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut prepended = bindir.clone().into_os_string();
    prepended.push(":");
    prepended.push(&original_path);
    // SAFETY: `--test-threads=1` (see the header) — no concurrent env reader.
    unsafe { std::env::set_var("PATH", &prepended) };
    let _path_guard = PathGuard(original_path);

    let sandboxed_spec = AppSpec {
        build: None,
        run: vec!["glass-testapp-bare".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: APP_TIMEOUT_MS,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    let geom = start(&mut p, &sandboxed_spec);
    assert_eq!(
        (geom.width, geom.height),
        (320, 240),
        "bare-name program on a shadowed PATH dir unreachable under the default sandbox"
    );
    assert!(
        drain_until(&mut p, "READY", READY_TRIES),
        "never saw READY (bare-name on a shadowed PATH dir)"
    );
    p.stop_app().unwrap();
}

// ---------------------------------------------------------------------------
// Build-step integration tests (bwrap + sway)
// ---------------------------------------------------------------------------

/// The build step runs BEFORE the compositor starts and its effect is visible when
/// the app launches. With `sandbox = Default`, a build command that writes a marker
/// file must have run (marker exists) and the app must then launch and render a
/// non-blank frame — proving build-then-launch ordering.
///
/// `cwd` is set to the tempdir so bwrap binds it rw inside the namespace, making
/// the `touch` write visible on the host path after the build step completes.
#[test]
#[ignore = "requires sway + bwrap; run via scripts/test-wayland.sh"]
fn wayland_build_step_runs_before_launch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("glass-wayland-build-marker");

    let build_spec = AppSpec {
        // Use a relative path; cwd is the tempdir so the file lands there.
        build: Some("touch glass-wayland-build-marker".into()),
        run: vec![TESTAPP.to_string()],
        cwd: Some(tmp.path().to_path_buf()),
        env: vec![],
        window_hint: None,
        timeout_ms: APP_TIMEOUT_MS,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    let mut p = WaylandPlatform::new().unwrap();
    let geom = start(&mut p, &build_spec);

    // Build ran: marker must exist on the host (the tempdir was rw-bound).
    assert!(
        marker.exists(),
        "build marker not found — build step did not run before launch"
    );

    // App launched: sensible geometry and a non-blank frame.
    assert_eq!((geom.width, geom.height), (320, 240), "window geometry");
    assert!(
        drain_until(&mut p, "READY", READY_TRIES),
        "never saw READY after build step"
    );
    std::thread::sleep(std::time::Duration::from_millis(300)); // let the first draw land
    let frame = p.capture_frame(None).unwrap();
    let non_zero = frame.pixels.iter().any(|&b| b != 0);
    assert!(
        non_zero,
        "captured frame is blank — app did not render after build step"
    );

    p.stop_app().unwrap();
    drop(tmp); // clean up marker dir
}

// (Build-step network containment tests removed: the build step is unsandboxed by design —
// only the launched run is contained.)

// geometry_settle_measurement (the #263 Wayland launch-race measurement) lives in its own test
// target, tests/wayland_geometry_settle_measurement.rs, run via
// scripts/wayland-geometry-settle-measurement.sh — not part of this file, so its 20 cold
// launches + sway spawns per run don't ride along with every scripts/test-wayland.sh (and so
// every CI) invocation.
