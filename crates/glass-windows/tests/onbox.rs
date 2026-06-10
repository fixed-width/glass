//! On-box E2E tests for the glass-windows backend, run in the interactive desktop session via
//! `scripts/test-windows.sh --tests onbox`. All `#[ignore]d` so plain `cargo test` (Linux/CI) skips
//! them; only `--ignored` on the box runs them. `#![cfg(windows)]` so the file is empty (0 tests)
//! off Windows, keeping the dev-box `cargo test`/clippy green. Serialized by the harness
//! (`--test-threads=1`) and by a process-global lock (so a direct `cargo test --ignored` is safe too)
//! since each spawns apps/windows.
#![cfg(windows)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use glass_a11y_windows::WindowsA11y;
use glass_core::{
    Accessibility, AppSpec, AxContext, AxNode, AxRole, AxTarget, GlassError, KeyEvent, Modifier,
    MouseButton, Platform, PointerEvent, WindowHint, WindowOp,
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
    }
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
    let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
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
        // actually killed — taskkill by image name leaves them alive, which would let [B] below find a
        // stale window and wrongly succeed.
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Stop-Process -Name notepad -Force -ErrorAction SilentlyContinue"])
            .output();
        std::thread::sleep(Duration::from_millis(800));
    }
    fn notepad_spec(hint: Option<WindowHint>) -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["notepad.exe".to_string()],
            cwd: None,
            env: vec![],
            window_hint: hint,
            timeout_ms: 8_000,
            sandbox: glass_core::SandboxLevel::Off,
        }
    }

    // [A] WITH a title hint: start_app must adopt the broker/handoff window (the PR #12 grace path).
    kill_notepad();
    let mut pa = WindowsPlatform::new().expect("WindowsPlatform::new");
    let res_a =
        pa.start_app(&notepad_spec(Some(WindowHint { title: Some("Notepad".into()), class: None })));
    assert!(res_a.is_ok(), "with a title hint, start_app must adopt the handoff window: {res_a:?}");
    let _ = pa.stop_app();
    kill_notepad();

    // [B] WITHOUT a hint: notepad's launcher hands off + exits leaving no Job descendant, so it must
    // fast-fail AppExited (a stray broker window is not in our pid-set, so it cannot be adopted).
    let mut pb = WindowsPlatform::new().expect("WindowsPlatform::new");
    let t = Instant::now();
    let res_b = pb.start_app(&notepad_spec(None));
    let elapsed = t.elapsed();
    assert!(
        matches!(res_b, Err(GlassError::AppExited(_))),
        "no-hint notepad must fast-fail AppExited, got {res_b:?}"
    );
    assert!(elapsed < Duration::from_secs(5), "no-hint fast-fail must be quick, took {elapsed:?}");
    let _ = pb.stop_app();
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
    let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
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

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_set_value() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p.start_app(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
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
#[ignore = "on-box only: needs the interactive desktop session + Edge"]
fn onbox_a11y_edge_geometry_fallback() {
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
    };
    // Edge's top-level window is owned by a DESCENDANT process, so the a11y reader's exact-pid match
    // misses and the geometry fallback must recover it — the path charmap can't exercise.
    let geo = p.start_app(&spec).expect("isolated Edge discovery (Job-child window)");
    std::thread::sleep(Duration::from_secs(6));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext { pids: p.app_pids(), window: geo.clone() };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot on multi-process Edge");
    let (mut total, mut inter) = (0usize, 0usize);
    counts(&tree.root, &mut total, &mut inter);
    assert!(tree.count > 20, "Edge's chrome should yield a sizable a11y tree, got {}", tree.count);
    assert!(inter > 0, "Edge tree must expose interactable elements, got {inter}");

    p.stop_app().expect("stop_app");
    std::thread::sleep(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&udd);
}
