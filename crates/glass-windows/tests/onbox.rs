//! On-box E2E tests for the glass-windows backend, run in the interactive desktop session via
//! `scripts/test-windows.sh --tests onbox`. All `#[ignore]d` so plain `cargo test` (Linux/CI) skips
//! them; only `--ignored` on the box runs them. `#![cfg(windows)]` so the file is empty (0 tests)
//! off Windows, keeping the dev-box `cargo test`/clippy green. Serialized by the harness
//! (`--test-threads=1`) and by a process-global lock (so a direct `cargo test --ignored` is safe too)
//! since each spawns apps/windows.
#![cfg(windows)]

use std::sync::Mutex;
use std::time::Duration;

use glass_a11y_windows::WindowsA11y;
use glass_core::{
    Accessibility, AppSpec, AxContext, AxNode, AxRole, AxTarget, GlassError, KeyEvent, Modifier,
    MouseButton, Platform, PointerEvent, WindowGeometry, WindowHint, WindowOp,
};
use glass_windows::WindowsPlatform;

/// Serialize the on-box tests: each spawns apps and grabs screen/input focus, so they must not run
/// concurrently even if invoked without `--test-threads=1`. Poison-tolerant so a panicking test does
/// not wedge the rest.
static SERIAL: Mutex<()> = Mutex::new(());

/// Per-Monitor-V2 awareness, once per test process (tests carry no manifest; capture/coords need it).
fn dpi_aware_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: process-global DPI setting, no preconditions; harmless if already set.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
    });
}

fn charmap_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["charmap.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: Some(WindowHint { title: Some("Character Map".into()), class: None }),
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// The egui input/a11y fixture (built on demand; excluded from the workspace). Paths derive from the
/// build location so the spec isn't pinned to one checkout/user. `sandbox` selects containment.
fn egui_fixture_spec(sandbox: glass_core::SandboxLevel) -> AppSpec {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-windows")
        .to_path_buf();
    let fixture_exe =
        repo_root.join("crates/glass-fixture-egui/target/release/glass-fixture-egui.exe");
    AppSpec {
        build: Some(
            "cargo build --release --manifest-path crates/glass-fixture-egui/Cargo.toml".to_string(),
        ),
        run: vec![fixture_exe.to_string_lossy().into_owned()],
        cwd: Some(repo_root),
        env: vec![],
        window_hint: None,
        timeout_ms: 120_000, // first egui build is slow
        sandbox,
        a11y: false, // Windows: UIA is ambient
    }
}

/// Drive a plain wheel then a ctrl+wheel at the window center; return the fixture's "wheel" log lines
/// for each. Each line carries both `ev_ctrl` (the modifier on the wheel event — delivery) and
/// `frame_ctrl` (the frame-aggregate `i.modifiers.ctrl` a handler gates on — held across the frame).
/// Used to verify wheel + modifier delivery AND modifier-hold across containment levels.
fn scroll_evidence(p: &mut WindowsPlatform, geo: &WindowGeometry) -> (Vec<String>, Vec<String>) {
    fn wheel_lines(p: &mut WindowsPlatform) -> Vec<String> {
        p.drain_logs().into_iter().map(|(_, l)| l).filter(|l| l.contains("wheel")).collect()
    }
    let _ = p.drain_logs(); // discard startup ("ready") logs
    let (cx, cy) = (geo.width as i32 / 2, geo.height as i32 / 2);

    p.send_pointer(&PointerEvent::Scroll { x: cx, y: cy, dx: 0, dy: -3, modifiers: vec![] })
        .expect("plain scroll submits");
    std::thread::sleep(Duration::from_millis(500));
    let plain = wheel_lines(p);

    p.send_pointer(&PointerEvent::Scroll {
        x: cx,
        y: cy,
        dx: 0,
        dy: -3,
        modifiers: vec![Modifier::Control],
    })
    .expect("ctrl scroll submits");
    std::thread::sleep(Duration::from_millis(500));
    let ctrl = wheel_lines(p);
    (plain, ctrl)
}

fn is_blank(px: &[u8]) -> bool {
    match px.chunks_exact(4).next() {
        Some(first) => px.chunks_exact(4).all(|c| c == first),
        None => true,
    }
}

fn changed(a: &[u8], b: &[u8]) -> usize {
    a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(x, y)| x != y).count()
}

fn counts(n: &AxNode, total: &mut usize, interactable: &mut usize) {
    *total += 1;
    if n.role.is_interactable() {
        *interactable += 1;
    }
    for c in &n.children {
        counts(c, total, interactable);
    }
}

fn first_clickable<'a>(n: &'a AxNode, out: &mut Option<&'a AxNode>) {
    if out.is_none() && n.role.is_interactable() && n.bounds.is_some() {
        *out = Some(n);
    }
    for c in &n.children {
        first_clickable(c, out);
    }
}

fn first_role<'a>(n: &'a AxNode, role: AxRole, out: &mut Option<&'a AxNode>) {
    if out.is_none() && n.role == role {
        *out = Some(n);
    }
    for c in &n.children {
        first_role(c, role, out);
    }
}

/// Count msedge.exe processes whose command line carries `marker` (our isolated user-data-dir), via
/// CIM so the box's background Edge isn't counted.
fn our_edge_count(marker: &str) -> i32 {
    let ps = format!(
        "@(Get-CimInstance Win32_Process -Filter \"Name='msedge.exe'\" | \
         Where-Object {{ $_.CommandLine -like '*{marker}*' }}).Count"
    );
    match std::process::Command::new("powershell").args(["-NoProfile", "-Command", &ps]).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(-1),
        Err(_) => -1,
    }
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_capture_and_input() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let _geo = p.start_app(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let f1 = p.capture_frame(None).expect("capture");
    assert!(!is_blank(&f1.pixels), "capture must be non-blank");

    p.send_key(&KeyEvent::Text("glass-onbox".into())).expect("send_key");
    std::thread::sleep(Duration::from_millis(900));
    let f2 = p.capture_frame(None).expect("recapture");
    assert_eq!(f1.pixels.len(), f2.pixels.len(), "frame size stable across input");
    assert!(changed(&f1.pixels, &f2.pixels) > 0, "typed text must change the frame");

    let g = p.window(&WindowOp::Move { x: 140, y: 140 }).expect("move");
    assert!((g.x - 140).abs() <= 2 && (g.y - 140).abs() <= 2, "moved within 2px: {g:?}");

    let _ = p.stop_app();
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session + Edge"]
fn onbox_isolated_edge_killtree() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let edge = glass_windows::onbox_support::locate_edge()
        .expect("msedge.exe not found under Program Files; Edge is required for this test");
    let marker = "glass-kt-test";
    let udd = glass_windows::onbox_support::scratch_dir(marker);
    let _ = std::fs::remove_dir_all(&udd);

    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let spec = AppSpec {
        build: None,
        run: vec![
            edge,
            format!("--user-data-dir={udd}"),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--new-window".to_string(),
            "about:blank".to_string(),
        ],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 25_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    let _geo = p.start_app(&spec).expect("isolated Edge discovery (Job-child window)");
    std::thread::sleep(Duration::from_secs(6)); // let renderer/GPU/utility children spawn

    let before = our_edge_count(marker);
    assert!(before >= 2, "expected a multi-process Edge tree, got {before}");

    let f = p.capture_frame(None).expect("capture Edge");
    assert!(!is_blank(&f.pixels), "Edge capture must be non-blank");

    p.stop_app().expect("stop_app");
    std::thread::sleep(Duration::from_secs(3)); // let the tree die with the job
    let after = our_edge_count(marker);
    let _ = std::fs::remove_dir_all(&udd);
    assert_eq!(after, 0, "Job kill-tree must leave 0 survivors, got {after}");
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_snapshot_and_click() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p.start_app(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
    };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot");
    assert!(tree.count > 0, "snapshot must have nodes");
    let (mut total, mut inter) = (0usize, 0usize);
    counts(&tree.root, &mut total, &mut inter);
    assert!(inter > 0, "charmap must expose interactable elements, got {inter}");

    let mut hit = None;
    first_clickable(&tree.root, &mut hit);
    let n = hit.expect("an interactable element with on-screen bounds");
    let (cx, cy) = n
        .bounds
        .and_then(|b| b.clamped_center(geo.width, geo.height))
        .expect("first interactable has a clampable center");
    // Capture before/after so we verify the click actually changed the UI (exercising the
    // bounds -> clamped_center -> click coordinate path), not merely that send_pointer returned Ok.
    let before = p.capture_frame(None).expect("capture before click");
    p.send_pointer(&PointerEvent::Click {
        x: cx,
        y: cy,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    })
    .expect("click element by center");
    std::thread::sleep(Duration::from_millis(700));
    let after = p.capture_frame(None).expect("capture after click");
    assert_eq!(before.pixels.len(), after.pixels.len(), "frame size stable across click");
    assert!(changed(&before.pixels, &after.pixels) > 0, "clicking the element must change the UI");

    let _ = p.stop_app();
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_handoff_grace() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();

    fn kill_notepad() {
        // Stop-Process (not `taskkill /IM notepad.exe`) so broker-hosted Win11 Notepad windows are
        // actually killed — taskkill by image name leaves them alive.
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Stop-Process -Name notepad -Force -ErrorAction SilentlyContinue"])
            .output();
        std::thread::sleep(Duration::from_millis(800));
    }

    // Win11 Notepad's launcher hands its UI to a DESCENDANT process and the launcher exits, so the
    // window is owned by a child in the pid-set (a cold no-hint launch was measured to yield
    // app_pids=[<descendant>, <root>] and a real window). discover_window's grace period — keep polling
    // while the pid-set still holds a live descendant — must adopt that window even with NO hint
    // (the PR #14 behavior; pre-#14 this fast-failed AppExited before the descendant's window mapped).
    // The no-hint fast-fail-on-true-crash path is covered by the discovery::poll_decision unit tests.
    kill_notepad();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let spec = AppSpec {
        build: None,
        run: vec!["notepad.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 8_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    let _geo = p.start_app(&spec).expect("notepad's handoff-to-descendant window must be discovered no-hint");
    std::thread::sleep(Duration::from_millis(800));
    let f = p.capture_frame(None).expect("capture the adopted notepad window");
    assert!(!is_blank(&f.pixels), "the adopted handoff window must capture non-blank");
    let _ = p.stop_app();
    kill_notepad();
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_modifier_click() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p.start_app(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1200));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
    };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot");
    let mut hit = None;
    first_clickable(&tree.root, &mut hit);
    let n = hit.expect("an interactable element with on-screen bounds");
    let (cx, cy) = n
        .bounds
        .and_then(|b| b.clamped_center(geo.width, geo.height))
        .expect("first interactable has a clampable center");

    // A plain click must land (frame changes) — proves clicks reach the window.
    let before = p.capture_frame(None).expect("capture before click");
    p.send_pointer(&PointerEvent::Click {
        x: cx,
        y: cy,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    })
    .expect("plain click");
    std::thread::sleep(Duration::from_millis(500));
    let after = p.capture_frame(None).expect("capture after click");
    assert!(changed(&before.pixels, &after.pixels) > 0, "plain click must change the UI");

    // Modifier-held clicks must submit cleanly (the modifier-VK-down -> mouse -> ups SendInput batch
    // builds and sends; modifier *delivery* is asserted by the X11/Wayland integration tests).
    for mods in [vec![Modifier::Control], vec![Modifier::Shift]] {
        p.send_pointer(&PointerEvent::Click {
            x: cx,
            y: cy,
            button: MouseButton::Left,
            count: 1,
            modifiers: mods,
        })
        .expect("modifier-held click must submit");
        std::thread::sleep(Duration::from_millis(200));
    }

    let _ = p.stop_app();
}

#[test]
#[ignore = "on-box only: runs in the interactive session via the harness"]
fn onbox_clipboard_roundtrip() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");

    // Includes non-ASCII to exercise the UTF-16 round-trip.
    const SENTINEL: &str = "glass-clip-\u{2713}-\u{e9}-\u{4e16}\u{754c}";
    p.set_clipboard(SENTINEL).expect("set_clipboard");
    assert_eq!(p.get_clipboard().expect("get_clipboard"), SENTINEL, "clipboard round-trip exact");

    p.set_clipboard("").expect("set empty clipboard");
    assert!(p.get_clipboard().expect("get empty clipboard").is_empty(), "empty round-trip");
}

// The dogfood found that a CONTAINED app's own clipboard write was invisible to glass: glass set/get
// round-tripped and glass->app paste worked, but the app's ctx.copy_text (-> arboard -> user32
// SetClipboardData, detoured into the private store) read back empty. This reproduces it with the
// fixture auto-copying a sentinel under Sandboxie, and isolates the app-write path from the store
// itself (glass's own set/get is checked after, on the same private store).
#[test]
#[ignore = "on-box only: needs the interactive desktop session + Sandboxie + the clip hook + builds the egui fixture"]
fn onbox_contained_clipboard_app_write() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let _geo = p
        .start_app(&egui_fixture_spec(glass_core::SandboxLevel::Default))
        .expect("build + launch the egui fixture under Sandboxie");
    std::thread::sleep(Duration::from_millis(3000)); // let it start AND auto-copy (frame >= 60)

    let logs: Vec<String> = p.drain_logs().into_iter().map(|(_, l)| l).collect();
    let copied = logs.iter().any(|l| l.contains("copied sentinel"));
    let after_app = p.get_clipboard().unwrap_or_default();
    // Isolate: does glass's own set/get work on this private store? (Run after, so it can't mask the
    // app-write result.) after_glass tells real-app-write-lost from store/route-broken.
    let after_glass = match p.set_clipboard("GLASS-SEEDED") {
        Ok(()) => p.get_clipboard().unwrap_or_default(),
        Err(e) => format!("<set_clipboard err: {e}>"),
    };
    eprintln!("copied-log={copied} after_app={after_app:?} after_glass={after_glass:?}");
    let _ = p.stop_app();

    assert!(copied, "the contained app must have run its copy (frame counter reached)");
    assert_eq!(
        after_app, "GLASS-CLIP-SENTINEL",
        "glass must read the contained app's own clipboard write (glass's own set/get on the same \
         private store = {after_glass:?})"
    );
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_set_value() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p.start_app(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
    };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot");

    let mut field = None;
    first_role(&tree.root, AxRole::TextField, &mut field);
    let field = field.expect("charmap must expose a TextField (Edit)");
    let target =
        AxTarget { id: field.id, role: field.role, name: field.name.clone(), bounds: field.bounds };

    const NEW: &str = "GLASSVALUE";
    a11y.set_value(&ctx, &target, NEW).expect("set_value on the Edit field");
    std::thread::sleep(Duration::from_millis(500));

    // Re-snapshot: the field's value changed (charmap's Edit keeps a trailing CR; compare trimmed).
    let t2 = a11y.snapshot(&ctx).expect("re-snapshot");
    let mut f2 = None;
    first_role(&t2.root, AxRole::TextField, &mut f2);
    let v = f2.and_then(|n| n.value.as_deref()).expect("TextField has a value after set");
    assert_eq!(v.trim_end(), NEW, "set_value must change the field value");

    // A non-editable element (Button) must error AxElementNotEditable, never silently succeed.
    let mut button = None;
    first_role(&tree.root, AxRole::Button, &mut button);
    let b = button.expect("charmap must expose at least one Button for the not-editable guard");
    let bt = AxTarget { id: b.id, role: b.role, name: b.name.clone(), bounds: b.bounds };
    assert!(
        matches!(a11y.set_value(&ctx, &bt, "x"), Err(GlassError::AxElementNotEditable(_))),
        "set_value on a Button must error AxElementNotEditable"
    );

    let _ = p.stop_app();
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session + builds the egui fixture"]
fn onbox_egui_set_value_honesty() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p
        .start_app(&egui_fixture_spec(glass_core::SandboxLevel::Off))
        .expect("build + launch the egui fixture");
    std::thread::sleep(Duration::from_millis(2000));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
    };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot of the egui fixture");

    // egui exposes TextEdit as a read-only AccessKit projection — UIA SetValue is accepted
    // but never applied. set_value must report that honestly (AxValueNotApplied), not false success.
    let mut field = None;
    first_role(&tree.root, AxRole::TextField, &mut field);
    let field = field.expect("the egui fixture must expose a TextField");
    let target =
        AxTarget { id: field.id, role: field.role, name: field.name.clone(), bounds: field.bounds };
    assert!(
        matches!(
            a11y.set_value(&ctx, &target, "hello"),
            Err(GlassError::AxValueNotApplied(_))
        ),
        "set_value on an egui TextEdit must error AxValueNotApplied (read-only projection), not false success"
    );

    let _ = p.stop_app();
}

// Uncontained: end-to-end verification that a Windows ctrl+scroll both DELIVERS the wheel with its
// modifier on the event (`ev_ctrl`) AND holds the modifier across the wheel's frame so the
// frame-aggregate `i.modifiers.ctrl` (`frame_ctrl`) — the layer a real handler gates on — reads it.
// The event reaching egui was never the gap; the gap is the frame-aggregate modifier, which a
// one-burst modifier+wheel+release drops (the modifier is released in the same frame the wheel
// lands). run_scroll's hold-dwell-release fixes it. This is the working baseline the Sandboxie repro
// is measured against.
#[test]
#[ignore = "on-box only: needs the interactive desktop session + builds the egui fixture"]
fn onbox_scroll_modifier_delivery() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p
        .start_app(&egui_fixture_spec(glass_core::SandboxLevel::Off))
        .expect("build + launch the egui fixture");
    std::thread::sleep(Duration::from_millis(2000));

    let (plain, ctrl) = scroll_evidence(&mut p, &geo);
    eprintln!("[uncontained] plain={plain:?} ctrl={ctrl:?}");
    let _ = p.stop_app();

    assert!(!plain.is_empty(), "plain scroll must deliver a wheel event to egui");
    assert!(
        ctrl.iter().any(|l| l.contains("ev_ctrl=true")),
        "ctrl+scroll must deliver a wheel event carrying ctrl to egui, got {ctrl:?}"
    );
    assert!(
        ctrl.iter().any(|l| l.contains("frame_ctrl=true")),
        "ctrl+scroll must hold ctrl across the wheel's frame (frame-aggregate i.modifiers.ctrl), \
         got {ctrl:?}"
    );
}

// The same two assertions across the Sandboxie boundary: the wheel + its event modifier cross into
// the contained app (never the gap), and the modifier is held across the wheel's frame so a contained
// handler reading `i.modifiers.ctrl` sees it.
#[test]
#[ignore = "on-box only: needs the interactive desktop session + Sandboxie + builds the egui fixture"]
fn onbox_scroll_modifier_delivery_sandboxed() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p
        .start_app(&egui_fixture_spec(glass_core::SandboxLevel::Default))
        .expect("build + launch the egui fixture under Sandboxie");
    std::thread::sleep(Duration::from_millis(2500));

    let (plain, ctrl) = scroll_evidence(&mut p, &geo);
    eprintln!("[sandboxed] plain={plain:?} ctrl={ctrl:?}");
    let _ = p.stop_app();

    assert!(!plain.is_empty(), "plain scroll must cross the Sandboxie boundary to egui");
    assert!(
        ctrl.iter().any(|l| l.contains("ev_ctrl=true")),
        "ctrl+scroll must cross the Sandboxie boundary carrying ctrl on the event, got {ctrl:?}"
    );
    assert!(
        ctrl.iter().any(|l| l.contains("frame_ctrl=true")),
        "ctrl+scroll must hold ctrl across the wheel's frame inside the sandbox, got {ctrl:?}"
    );
}

// A synthetic key chord must hold the modifier across the frame the key lands in, so the
// standard egui hotkey idiom (`key_pressed(K) && i.modifiers.command`) fires — as it does for real
// hardware that holds the modifier across many frames. If glass injects ctrl-down/Z/ctrl-up in one
// burst, egui drains them into a single frame and the frame-aggregate modifier is already false.
#[test]
#[ignore = "on-box only: needs the interactive desktop session + builds the egui fixture"]
fn onbox_chord_modifier_frame() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let _geo = p
        .start_app(&egui_fixture_spec(glass_core::SandboxLevel::Off))
        .expect("build + launch the egui fixture");
    std::thread::sleep(Duration::from_millis(2000));
    let _ = p.drain_logs(); // discard startup logs

    p.send_key(&KeyEvent::Chord("ctrl+z".to_string())).expect("ctrl+z chord submits");
    std::thread::sleep(Duration::from_millis(600));
    let logs: Vec<String> = p.drain_logs().into_iter().map(|(_, l)| l).collect();
    for l in logs.iter().filter(|l| l.contains("key ") || l.contains("chord Z")) {
        eprintln!("  {l}");
    }
    let _ = p.stop_app();

    let chord = logs.iter().filter(|l| l.contains("chord Z")).cloned().collect::<Vec<_>>();
    assert!(!chord.is_empty(), "ctrl+z must reach egui as a Z key press, got none");
    assert!(
        chord.iter().any(|l| l.contains("undo_idiom=true")),
        "ctrl+z must let `key_pressed(Z) && modifiers.command` hold in one frame, got {chord:?}"
    );
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session + Edge"]
fn onbox_a11y_edge_multiprocess() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let edge = glass_windows::onbox_support::locate_edge()
        .expect("msedge.exe not found under Program Files; Edge is required for this test");
    let udd = glass_windows::onbox_support::scratch_dir("glass-a11y-edge-test");
    let _ = std::fs::remove_dir_all(&udd);

    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let spec = AppSpec {
        build: None,
        run: vec![
            edge,
            format!("--user-data-dir={udd}"),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--new-window".to_string(),
            "about:blank".to_string(),
        ],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 25_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    // Edge's top-level window is owned by a DESCENDANT process. glass adopts it as the active window;
    // the a11y reader reads it via that adopted handle (ctx.window_handle) — verifying a11y on a
    // multi-process app whose window a single-process target like charmap can't exercise.
    let geo = p.start_app(&spec).expect("isolated Edge discovery (Job-child window)");
    std::thread::sleep(Duration::from_secs(6));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
    };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot on multi-process Edge");
    let (mut total, mut inter) = (0usize, 0usize);
    counts(&tree.root, &mut total, &mut inter);
    assert!(tree.count > 20, "Edge's chrome should yield a sizable a11y tree, got {}", tree.count);
    assert!(inter > 0, "Edge tree must expose interactable elements, got {inter}");

    p.stop_app().expect("stop_app");
    std::thread::sleep(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&udd);
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session + Sandboxie"]
fn onbox_contained_launch_adopts_app_not_console() {
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let spec = AppSpec {
        build: None,
        run: vec!["notepad.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None, // the whole point: no hint needed once scaffolding is excluded
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Default,
        a11y: false,
    };
    // Before the fix this "succeeds" by adopting the boxed `cmd /c launch.cmd` launcher console;
    // the assertions below fail. After the fix, discovery adopts the boxed Notepad window.
    let _geo = p
        .start_app(&spec)
        .expect("contained Notepad must adopt the app window, not the launcher console");
    let windows = p.list_windows().expect("list_windows");
    let active = windows.iter().find(|w| w.active).expect("an active adopted window");
    let class = active.class.clone().unwrap_or_default();
    assert_ne!(class, "ConsoleWindowClass", "glass_start adopted the Sandboxie launcher console");
    assert!(
        class.starts_with("Sandbox:"),
        "expected a boxed app window class (Sandbox:<box>:...), got {class:?}"
    );
    p.stop_app().expect("stop_app");
}
