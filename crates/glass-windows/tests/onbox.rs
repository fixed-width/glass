//! On-box E2E tests for the glass-windows backend, run in the interactive desktop session via
//! `scripts/test-windows.sh --tests onbox`. All `#[ignore]d` so plain `cargo test` (Linux/CI) skips
//! them; only `--ignored` on the box runs them. `#![cfg(windows)]` so the file is empty (0 tests)
//! off Windows, keeping the dev-box `cargo test`/clippy green. Serialized by the harness
//! (`--test-threads=1`) and by a process-global lock (so a direct `cargo test --ignored` is safe too)
//! since each spawns apps/windows.
#![cfg(windows)]
// On-box E2E: opts out of the workspace `unsafe_code = "deny"` (each `unsafe` site is
// `// SAFETY:`-documented).
#![allow(unsafe_code)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use glass_a11y_windows::WindowsA11y;
use glass_core::{
    Accessibility, AppSpec, AxContext, AxNode, AxRole, AxTarget, AxTree, Backend, BaselineStore,
    ChangeSignal, DescriptionSourcing, Glass, GlassError, KeyEvent, Modifier, MouseButton,
    Platform, PlatformFactory, PointerEvent, WalkLimits, WindowGeometry, WindowHint, WindowOp,
    description_census, description_census_report, role_histogram,
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
        window_hint: Some(WindowHint {
            title: Some("Character Map".into()),
            class: None,
        }),
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// No `window_hint`: Notepad's own process (or, on Win11, the descendant its launcher hands
/// its UI to — see `onbox_handoff_grace`'s doc) is discovered purely by pid-set membership,
/// so a title hint isn't needed the way it would be for a hand-off to an *unrelated* process
/// (see `onbox_role_histogram_probe`'s doc for why that shape was left out of the probe).
fn notepad_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["notepad.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// Task Manager's own process directly owns its window — same discoverable-by-pid shape as
/// charmap/Notepad — *unless* an instance was already running before this test started, in
/// which case Task Manager's single-instance check makes our freshly spawned process just
/// activate the existing window and exit. The title hint exists for exactly that case: with
/// no window in our own pid-set, `poll_decision` keeps polling on the hint alone and the
/// system-wide title-substring fallback rung picks up the pre-existing window instead.
fn taskmgr_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["taskmgr.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: Some(WindowHint {
            title: Some("Task Manager".into()),
            class: None,
        }),
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// Bare `explorer.exe` (no path arg): opens the OS default location — "Quick access" on
/// Windows 10, "Home" on Windows 11 — the same native file-list control, and so the same UIA
/// tree, any folder view uses (what that tree actually emitted is recorded in
/// [`onbox_role_histogram_probe`]'s doc). The `class` hint is what actually finds it: the
/// spawned `explorer.exe` hands the request to the already-running shell and exits, so the
/// folder window belongs to a process that was there before glass started and is in no pid set
/// glass tracks. That also means `stop_app` cannot close it — teardown reaches the process tree
/// glass launched, which by then is gone — so the probe closes the adopted window itself (see
/// [`close_adopted_window`]); without that, every run would leave a folder window on the
/// desktop. The hint is `class`, not `title`: `CabinetWClass` is
/// every File Explorer folder window's documented window class regardless of locale, OS
/// version, or the current folder's display name — unlike a title, which would vary with
/// exactly the thing this spec deliberately leaves unset.
fn explorer_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["explorer.exe".to_string()],
        cwd: None,
        env: vec![],
        window_hint: Some(WindowHint {
            title: None,
            class: Some("CabinetWClass".into()),
        }),
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// A `Glass` session wired to the real Windows backend + UIA reader (as opposed to the bare
/// `WindowsPlatform`/`WindowsA11y` the other on-box tests drive directly) — needed wherever a test
/// wants the production `click_element` orchestration (invoke-first, pointer-fallback,
/// `ClickMethod` disclosure) rather than the reader alone. Mirrors the X11 integration suite's
/// `glass_x11_with_a11y()`. The baseline dir is leaked (not needed: this is a short-lived on-box
/// test process, and `Glass` never deletes it itself).
fn glass_windows_with_a11y() -> Glass {
    let factory: PlatformFactory = Box::new(|_backend| {
        Ok(Backend {
            platform: Box::new(WindowsPlatform::new()?),
            accessibility: Some(Box::new(WindowsA11y::new())),
        })
    });
    let dir = tempfile::tempdir().expect("tempdir for baseline store");
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    Glass::new(factory, "windows".into(), BaselineStore::new(root), 100)
}

/// The repo root: two levels above `crates/glass-windows` (this crate's `CARGO_MANIFEST_DIR`),
/// baked in at build time — so on the box's own `cargo build`, it reflects wherever that box's
/// checkout actually lives, not a hardcoded path/user.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-windows")
        .to_path_buf()
}

/// The egui input/a11y fixture (built on demand; excluded from the workspace). Paths derive from the
/// build location so the spec isn't pinned to one checkout/user. `sandbox` selects containment.
fn egui_fixture_spec(sandbox: glass_core::SandboxLevel) -> AppSpec {
    let repo_root = repo_root();
    let fixture_exe =
        repo_root.join("crates/glass-fixture-egui/target/release/glass-fixture-egui.exe");
    AppSpec {
        build: Some(
            "cargo build --release --manifest-path crates/glass-fixture-egui/Cargo.toml"
                .to_string(),
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
        p.drain_logs()
            .into_iter()
            .map(|(_, l)| l)
            .filter(|l| l.contains("wheel"))
            .collect()
    }
    let _ = p.drain_logs(); // discard startup ("ready") logs
    let (cx, cy) = (geo.width as i32 / 2, geo.height as i32 / 2);

    p.send_pointer(&PointerEvent::Scroll {
        x: cx,
        y: cy,
        dx: 0,
        dy: -3,
        modifiers: vec![],
    })
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
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(x, y)| x != y)
        .count()
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

/// Like [`first_role`], but only nodes that actually report geometry — for a test that goes on
/// to click the node, where a bounds-less match would make the follow-on assertions luck.
fn first_role_with_bounds<'a>(n: &'a AxNode, role: AxRole, out: &mut Option<&'a AxNode>) {
    if out.is_none() && n.role == role && n.bounds.is_some() {
        *out = Some(n);
    }
    for c in &n.children {
        first_role_with_bounds(c, role, out);
    }
}

/// Nodes carrying UIA's Toggle pattern that are NOT already a checkbox or radio button —
/// the evidence for whether a `ToggleButton` mapping has anything to map. `checkable` is
/// Toggle-pattern availability (see `StateFacts` in glass-a11y-windows), so this needs no
/// extra UIA call.
fn toggle_candidates<'a>(node: &'a AxNode, out: &mut Vec<&'a AxNode>) {
    if node.states.checkable && !matches!(node.role, AxRole::CheckBox | AxRole::RadioButton) {
        out.push(node);
    }
    for child in &node.children {
        toggle_candidates(child, out);
    }
}

/// Count msedge.exe processes whose command line carries `marker` (our isolated user-data-dir), via
/// CIM so the box's background Edge isn't counted.
fn our_edge_count(marker: &str) -> i32 {
    let ps = format!(
        "@(Get-CimInstance Win32_Process -Filter \"Name='msedge.exe'\" | \
         Where-Object {{ $_.CommandLine -like '*{marker}*' }}).Count"
    );
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(-1),
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

    p.send_key(&KeyEvent::Text("glass-onbox".into()))
        .expect("send_key");
    std::thread::sleep(Duration::from_millis(900));
    let f2 = p.capture_frame(None).expect("recapture");
    assert_eq!(
        f1.pixels.len(),
        f2.pixels.len(),
        "frame size stable across input"
    );
    assert!(
        changed(&f1.pixels, &f2.pixels) > 0,
        "typed text must change the frame"
    );

    let g = p.window(&WindowOp::Move { x: 140, y: 140 }).expect("move");
    assert!(
        (g.x - 140).abs() <= 2 && (g.y - 140).abs() <= 2,
        "moved within 2px: {g:?}"
    );

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
    let _geo = p
        .start_app(&spec)
        .expect("isolated Edge discovery (Job-child window)");
    std::thread::sleep(Duration::from_secs(6)); // let renderer/GPU/utility children spawn

    let before = our_edge_count(marker);
    assert!(
        before >= 2,
        "expected a multi-process Edge tree, got {before}"
    );

    let f = p.capture_frame(None).expect("capture Edge");
    assert!(!is_blank(&f.pixels), "Edge capture must be non-blank");

    p.stop_app().expect("stop_app");
    std::thread::sleep(Duration::from_secs(3)); // let the tree die with the job
    let after = our_edge_count(marker);
    let _ = std::fs::remove_dir_all(&udd);
    assert_eq!(
        after, 0,
        "Job kill-tree must leave 0 survivors, got {after}"
    );
}

/// Wait until no msedge.exe carrying `marker` is left, or `budget` elapses. Returns the final
/// count so a caller can distinguish "the tree closed itself" from "we ran out of patience".
fn wait_for_no_edge(marker: &str, budget: Duration) -> i32 {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let n = our_edge_count(marker);
        if n == 0 || std::time::Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session + Edge"]
fn onbox_stop_app_lets_edge_record_a_clean_exit() {
    // `stop_app` must ask the app to close before terminating it. Edge is the readable witness:
    // it records `exit_type` in its profile, and a tree ended by TerminateProcess (closing the
    // Job, which is all `stop_app` used to do) leaves "Crashed" behind — which is what makes the
    // next launch open with a "Restore pages?" prompt instead of the app's normal first screen.
    // Measured on-box before the fix: Crashed. An isolated `--user-data-dir` keeps this away from
    // the box's own Edge profile.
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let edge = glass_windows::onbox_support::locate_edge()
        .expect("msedge.exe not found under Program Files; Edge is required for this test");
    let marker = "glass-clean-exit-test";
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
    let _geo = p
        .start_app(&spec)
        .expect("isolated Edge discovery (Job-child window)");
    std::thread::sleep(Duration::from_secs(6)); // let the profile be written and children spawn

    p.stop_app().expect("stop_app");
    let survivors = wait_for_no_edge(marker, Duration::from_secs(20));
    // Settle before reading: Edge rewrites Preferences as part of its shutdown, so a read racing
    // that write could see the in-session "Crashed" and fail for the wrong reason. A fixed settle
    // (rather than polling until the value reads "Normal") keeps the assertion able to fail.
    std::thread::sleep(Duration::from_secs(3));
    let prefs_path = std::path::Path::new(&udd)
        .join("Default")
        .join("Preferences");
    let prefs = std::fs::read_to_string(&prefs_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", prefs_path.display()));
    let exit_type = glass_windows::onbox_support::exit_type_from_preferences(&prefs)
        .map(str::to_owned)
        .unwrap_or_else(|| "<no exit_type key>".to_string());
    let _ = std::fs::remove_dir_all(&udd);

    assert_eq!(survivors, 0, "stop_app must still leave 0 survivors");
    assert_eq!(
        exit_type, "Normal",
        "stop_app must ask the app to close, not just terminate it"
    );
}

#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_snapshot_and_click() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut glass = glass_windows_with_a11y();
    let geo = glass.start(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let tree = glass.a11y_snapshot(None).expect("a11y snapshot");
    assert!(tree.count > 0, "snapshot must have nodes");
    let (mut total, mut inter) = (0usize, 0usize);
    counts(&tree.root, &mut total, &mut inter);
    assert!(
        inter > 0,
        "charmap must expose interactable elements, got {inter}"
    );

    // A Button specifically, not just the first interactable: `first_clickable` lands on
    // charmap's font ComboBox, whose actuation verb is ExpandCollapse, not Invoke — a weaker
    // (and differently-behaving) subject for the native-path assertion below. Still requires
    // bounds (as `first_clickable` did), so the clampable-center check below stays meaningful.
    let mut hit = None;
    first_role_with_bounds(&tree.root, AxRole::Button, &mut hit);
    let n = hit.expect("charmap exposes a Button with on-screen bounds");
    let id = n.id;
    assert!(
        n.bounds
            .and_then(|b| b.clamped_center(geo.width, geo.height))
            .is_some(),
        "the Button has a clampable center"
    );
    // Capture before/after so we verify the click actually changed the UI, not merely that
    // click_element returned Ok. A Win32 Button (charmap's "Select"/"Copy", same as an
    // egui/accesskit button) publishes the UIA InvokePattern, so the production click_element
    // path must take the native-action branch here, never synthesize a pointer event.
    let before = glass.screenshot(None, None).expect("capture before click");
    let method = glass.click_element(id).expect("click element by id");
    assert_eq!(
        method,
        glass_core::ClickMethod::NativeAction,
        "a Win32 Button publishes UIA InvokePattern; click_element must take the native path"
    );
    std::thread::sleep(Duration::from_millis(700));
    let after = glass.screenshot(None, None).expect("capture after click");
    assert_eq!(
        before.pixels.len(),
        after.pixels.len(),
        "frame size stable across click"
    );
    assert!(
        changed(&before.pixels, &after.pixels) > 0,
        "clicking the element must change the UI"
    );

    let _ = glass.stop();
}

// The Button case above only reaches the ladder's Invoke rung (fire-and-report, no post-state to
// verify). charmap's "Advanced view >>" checkbox exposes UIA TogglePattern but not InvokePattern,
// so this test exercises the ladder's Toggle rung instead — the one rung with a readable
// post-state, which `run_invoke` (glass-a11y-windows) polls after firing `Toggle()` and only
// returns `Ok` once the state has actually flipped. So a plain `Ok(NativeAction)` here already
// proves the flip against a real UIA provider; the re-snapshot poll below is belt-and-braces on
// top of that, mirroring the AT-SPI GTK-switch analogue.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_native_toggle_checkbox() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let mut glass = glass_windows_with_a11y();
    let _geo = glass.start(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let tree = glass.a11y_snapshot(None).expect("a11y snapshot");
    let mut hit = None;
    first_role_with_bounds(&tree.root, AxRole::CheckBox, &mut hit);
    let n = hit.expect("charmap exposes a CheckBox (Advanced view) with on-screen bounds");
    let id = n.id;
    let before = n.states.checked;

    let method = glass.click_element(id).expect("click element by id");
    assert_eq!(
        method,
        glass_core::ClickMethod::NativeAction,
        "charmap's Advanced-view checkbox publishes only UIA TogglePattern; click_element must \
         take the native path"
    );

    // Bounded re-snapshot poll for the state flip. Toggling Advanced view resizes the charmap
    // window (a side panel opens), so re-find the checkbox by role each pass rather than trusting
    // a stale id or geometry from the pre-click snapshot.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let t = glass.a11y_snapshot(None).expect("re-snapshot");
        let mut after = None;
        first_role_with_bounds(&t.root, AxRole::CheckBox, &mut after);
        if after.is_some_and(|n2| n2.states.checked != before) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native toggle never flipped the checkbox's checked state"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = glass.stop();
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
            .args([
                "-NoProfile",
                "-Command",
                "Stop-Process -Name notepad -Force -ErrorAction SilentlyContinue",
            ])
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
    let _geo = p
        .start_app(&spec)
        .expect("notepad's handoff-to-descendant window must be discovered no-hint");
    std::thread::sleep(Duration::from_millis(800));
    let f = p
        .capture_frame(None)
        .expect("capture the adopted notepad window");
    assert!(
        !is_blank(&f.pixels),
        "the adopted handoff window must capture non-blank"
    );
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
        limits: WalkLimits::DEFAULT,
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
    assert!(
        changed(&before.pixels, &after.pixels) > 0,
        "plain click must change the UI"
    );

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
    assert_eq!(
        p.get_clipboard().expect("get_clipboard"),
        SENTINEL,
        "clipboard round-trip exact"
    );

    p.set_clipboard("").expect("set empty clipboard");
    assert!(
        p.get_clipboard().expect("get empty clipboard").is_empty(),
        "empty round-trip"
    );
}

// A CONTAINED app's own clipboard write was invisible to glass: glass set/get
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

    assert!(
        copied,
        "the contained app must have run its copy (frame counter reached)"
    );
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
        limits: WalkLimits::DEFAULT,
    };
    let tree = a11y.snapshot(&ctx).expect("a11y snapshot");

    let mut field = None;
    first_role(&tree.root, AxRole::TextField, &mut field);
    let field = field.expect("charmap must expose a TextField (Edit)");
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };

    const NEW: &str = "GLASSVALUE";
    a11y.set_value(&ctx, &target, NEW)
        .expect("set_value on the Edit field");
    std::thread::sleep(Duration::from_millis(500));

    // Re-snapshot: the field's value changed (charmap's Edit keeps a trailing CR; compare trimmed).
    let t2 = a11y.snapshot(&ctx).expect("re-snapshot");
    let mut f2 = None;
    first_role(&t2.root, AxRole::TextField, &mut f2);
    let v = f2
        .and_then(|n| n.value.as_deref())
        .expect("TextField has a value after set");
    assert_eq!(v.trim_end(), NEW, "set_value must change the field value");

    // A non-editable element (Button) must error AxElementNotEditable, never silently succeed.
    let mut button = None;
    first_role(&tree.root, AxRole::Button, &mut button);
    let b = button.expect("charmap must expose at least one Button for the not-editable guard");
    let bt = AxTarget {
        id: b.id,
        role: b.role,
        name: b.name.clone(),
        bounds: b.bounds,
        value: b.value.clone(),
    };
    assert!(
        matches!(
            a11y.set_value(&ctx, &bt, "x"),
            Err(GlassError::AxElementNotEditable(_))
        ),
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
        limits: WalkLimits::DEFAULT,
    };
    let tree = a11y
        .snapshot(&ctx)
        .expect("a11y snapshot of the egui fixture");

    // egui exposes TextEdit as a read-only AccessKit projection — UIA SetValue is accepted
    // but never applied. set_value must report that honestly (AxValueNotApplied), not false success.
    let mut field = None;
    first_role(&tree.root, AxRole::TextField, &mut field);
    let field = field.expect("the egui fixture must expose a TextField");
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };
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
// The event reaching egui was never the bug; the bug is the frame-aggregate modifier, which a
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

    assert!(
        !plain.is_empty(),
        "plain scroll must deliver a wheel event to egui"
    );
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
// the contained app (never the bug), and the modifier is held across the wheel's frame so a contained
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

    assert!(
        !plain.is_empty(),
        "plain scroll must cross the Sandboxie boundary to egui"
    );
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

    p.send_key(&KeyEvent::Chord("ctrl+z".to_string()))
        .expect("ctrl+z chord submits");
    std::thread::sleep(Duration::from_millis(600));
    let logs: Vec<String> = p.drain_logs().into_iter().map(|(_, l)| l).collect();
    for l in logs
        .iter()
        .filter(|l| l.contains("key ") || l.contains("chord Z"))
    {
        eprintln!("  {l}");
    }
    let _ = p.stop_app();

    let chord = logs
        .iter()
        .filter(|l| l.contains("chord Z"))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !chord.is_empty(),
        "ctrl+z must reach egui as a Z key press, got none"
    );
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
    let geo = p
        .start_app(&spec)
        .expect("isolated Edge discovery (Job-child window)");
    std::thread::sleep(Duration::from_secs(6));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    let tree = a11y
        .snapshot(&ctx)
        .expect("a11y snapshot on multi-process Edge");
    let (mut total, mut inter) = (0usize, 0usize);
    counts(&tree.root, &mut total, &mut inter);
    assert!(
        tree.count > 20,
        "Edge's chrome should yield a sizable a11y tree, got {}",
        tree.count
    );
    assert!(
        inter > 0,
        "Edge tree must expose interactable elements, got {inter}"
    );

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
    let active = windows
        .iter()
        .find(|w| w.active)
        .expect("an active adopted window");
    let class = active.class.clone().unwrap_or_default();
    assert_ne!(
        class, "ConsoleWindowClass",
        "glass_start adopted the Sandboxie launcher console"
    );
    assert!(
        class.starts_with("Sandbox:"),
        "expected a boxed app window class (Sandbox:<box>:...), got {class:?}"
    );
    p.stop_app().expect("stop_app");
}

/// Where the role-histogram probe writes its report: `.windows-artifacts` under the repo
/// root. Mirrors `examples/onbox.rs`'s `out_dir()`/`save()` convention (a resolved-at-runtime
/// directory + a small write-and-report helper) — but targets `.windows-artifacts` directly
/// rather than `%USERPROFILE%`. That example's `%USERPROFILE%` files get swept into
/// `.windows-artifacts` by a step in `tools/windows-validation/run-onbox.ps1` that runs only
/// for the plain `onbox` example; an `--ignored` test run (this probe's path, via
/// `scripts/test-windows.sh --tests`) has no such sweep. `.windows-artifacts` itself, though,
/// is scp'd back to the caller by `scripts/test-windows.sh` after every run regardless — and
/// it must be, since this probe's stdout is captured by the schtasks bounce into the
/// interactive session and never reaches the caller at all. So this file is the only way the
/// histogram gets out.
fn artifacts_dir() -> std::path::PathBuf {
    repo_root().join(".windows-artifacts")
}

/// Write `report` to `name` under [`artifacts_dir`], creating the directory if it doesn't
/// already exist (`run-onbox.ps1` creates it fresh before every run, but this stays robust
/// to a direct on-box invocation that skips the harness). Mirrors `examples/onbox.rs`'s
/// `save()`.
fn save_report(name: &str, report: &str) {
    let dir = artifacts_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        println!("    creating {} FAILED: {e}", dir.display());
        return;
    }
    let path = dir.join(name);
    match std::fs::write(&path, report) {
        Ok(()) => println!("    saved {}", path.display()),
        Err(e) => println!("    save {} FAILED: {e}", path.display()),
    }
}

/// Render `role_histogram(tree)` as one line per `(token, role)` bucket — unmapped
/// ([`AxRole::Other`]) buckets first, which is already the histogram's own sort order, so
/// the tokens most worth a human's attention are the first thing in the block. Returns the
/// text (rather than printing it) so [`probe_role_histogram`] can both print it directly
/// (useful running straight on a box with a desktop session) and fold it into the artifacts
/// file that's the only way it reaches a caller through the on-box harness.
fn render_role_histogram(label: &str, tree: &AxTree) -> String {
    use std::fmt::Write as _;
    let hist = role_histogram(tree);
    let mut out = String::new();
    let _ = writeln!(out, "\n===== role histogram: {label} =====");
    let _ = writeln!(
        out,
        "{} nodes, {} distinct (token, role) buckets",
        tree.count,
        hist.len()
    );
    if let Some(t) = &tree.truncated {
        let _ = writeln!(out, "  NOTE: {}", t.notice());
    }
    for entry in &hist {
        let tag = if entry.role == AxRole::Other {
            "UNMAPPED"
        } else {
            "mapped"
        };
        let _ = writeln!(
            out,
            "  {tag:>8}  x{:<5} role={:?} token={:?}",
            entry.count, entry.role, entry.raw_role
        );
    }
    out
}

/// Extra snapshots [`render_snapshot_cost`] takes of each probed app. Every sample is printed
/// beside the mean, so a single slow outlier is visible rather than hidden in the average.
const COST_REPEATS: usize = 10;

/// Re-snapshot the already-launched app [`COST_REPEATS`] times and render each sample's
/// wall-clock.
///
/// Repeating inside one launch leaves app *startup* out of the number, but each sample is still a
/// whole `snapshot` call — a fresh thread, COM initialization and the window lookup, then the walk —
/// so only the *difference* between two runs of this block is attributable to a change in what the
/// walk reads per node (glass's `description`). A per-node figure is that difference over the node
/// count in the histogram header above, never the mean over it.
///
/// Never asserted and never fatal: a latency bound would flake on a loaded box, and a snapshot that
/// fails here must not strand the window the caller is about to close, cost the later probes their
/// evidence, or skip the artifacts write. Twin of `print_snapshot_cost` in
/// `glass-macos/tests/role_probe.rs` — keep the two in step.
fn render_snapshot_cost(a11y: &mut WindowsA11y, ctx: &AxContext) -> String {
    let mut samples = Vec::with_capacity(COST_REPEATS);
    for repeat in 0..COST_REPEATS {
        let started = Instant::now();
        match a11y.snapshot(ctx) {
            Ok(tree) => {
                // Inside the timed window, so no future laziness could move walk work out from
                // under the timer.
                std::hint::black_box(&tree);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            // The samples taken so far go out with the error: "the first one failed" and
            // "they grew from 40ms to 900ms and then failed" are different findings.
            Err(e) => {
                return format!(
                    "  snapshot cost: repeat {repeat} of {COST_REPEATS} FAILED: {e}\n  \
                     samples before it: {}\n",
                    render_cost_samples(&samples)
                );
            }
        }
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    format!(
        "  snapshot cost over {COST_REPEATS} snapshots (mean {mean:.0}ms): {}\n",
        render_cost_samples(&samples)
    )
}

/// Cost samples as `19ms, 20ms, …`, or `(none)` when the first snapshot failed.
fn render_cost_samples(samples: &[f64]) -> String {
    if samples.is_empty() {
        return "(none)".to_string();
    }
    samples
        .iter()
        .map(|ms| format!("{ms:.0}ms"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// UIA control-type names glass maps to a role, and that a probed app has actually been seen to
/// emit. A histogram bucket carrying one of these must not come back [`AxRole::Other`]: the
/// token reached the reader, so a mapped role is the only correct outcome, and `Other` would
/// mean the plumbing between the reader and `map_role` broke. A token that simply does not
/// appear in a given app asserts nothing — apps differ, and an absent token is not a
/// regression.
///
/// Deliberately NOT listed: `Header`/`HeaderItem`, which the probe does observe but which map
/// to no role on purpose (a grid's column headers are not document headings), and `Button`,
/// whose `ToggleButton` promotion is state-dependent rather than a property of the token.
const MAPPED_TOKENS: &[&str] = &["Document"];

/// Every [`MAPPED_TOKENS`] bucket in `tree` that came back [`AxRole::Other`], described — the
/// one thing a histogram can check without becoming brittle about which app exposes what.
/// Everything else the probe prints is evidence for a human, not a pass/fail claim. Returned
/// rather than asserted so the caller can finish the run, record the findings in the artifacts
/// file with the histograms that explain them, and only then fail.
fn mapped_token_violations(label: &str, tree: &AxTree) -> Vec<String> {
    role_histogram(tree)
        .into_iter()
        .filter(|e| e.role == AxRole::Other && MAPPED_TOKENS.contains(&e.raw_role.as_str()))
        .map(|e| {
            format!(
                "{label}: {} node(s) reported token {:?} as Other, but glass maps that token — \
                 the reader is not feeding map_role what it reads",
                e.count, e.raw_role
            )
        })
        .collect()
}

/// Ask a still-open adopted window to close, for the case `stop_app` cannot reach: a window
/// owned by a process glass did not launch (a launcher that hands off to a long-running shell).
/// `WM_CLOSE` rather than terminating anything — the owner here is the user's shell process.
/// A no-op when the handle is absent or its window is already gone.
fn close_adopted_window(raw: Option<i64>) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_CLOSE};

    let Some(raw) = raw else { return };
    let hwnd = windows::Win32::Foundation::HWND(raw as *mut std::ffi::c_void);
    // SAFETY: both calls only query/queue against an HWND — no pointer we own is written
    // through, and neither blocks on the target thread. A stale handle fails `IsWindow`.
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// What [`probe_role_histogram`] hands back for the caller to assert on once every app has been
/// probed and the report written.
struct ProbeOutcome {
    /// See [`mapped_token_violations`].
    violations: Vec<String>,
    /// Nodes whose `description` the reader sourced. Per-app this is an observation, not a
    /// requirement — an app carrying no tooltips is a legitimate zero — but a run in which NO app
    /// reports one means the reader stopped sourcing the field, which the caller does assert on.
    described: usize,
}

/// Launch `spec`, snapshot its accessibility tree with the node cap lifted (so a big app's
/// tree is never truncated mid-probe — depth/siblings keep their generous structural-rail
/// defaults regardless; see [`WalkLimits::from_max_nodes`]), print its role histogram, append
/// it to `report`, then stop the app. Panics — failing the test — only when the app can't be
/// launched or its FIRST snapshot can't be taken at all: a real breakage, never merely an
/// unexpected role. A snapshot that fails later, inside [`render_snapshot_cost`]'s repeats, is
/// reported in the block and not fatal. Returns the [`ProbeOutcome`] the caller asserts on *after*
/// the report is saved. Beyond those checks the histogram's contents are never asserted; reading it
/// to decide which `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` a real native token now
/// justifies filling is the human's job.
#[must_use]
fn probe_role_histogram(label: &str, spec: &AppSpec, report: &mut String) -> ProbeOutcome {
    let mut p = WindowsPlatform::new().expect("WindowsPlatform::new");
    let geo = p
        .start_app(spec)
        .unwrap_or_else(|e| panic!("{label}: start_app failed: {e}"));
    std::thread::sleep(Duration::from_millis(1500));

    let mut a11y = WindowsA11y::new();
    let ctx = AxContext {
        pids: p.app_pids(),
        window: geo.clone(),
        window_handle: p.active_window_handle(),
        a11y_bus_addr: None,
        limits: WalkLimits::from_max_nodes(Some(0)),
    };
    let tree = a11y
        .snapshot(&ctx)
        .unwrap_or_else(|e| panic!("{label}: a11y snapshot failed: {e}"));

    let block = render_role_histogram(label, &tree);
    print!("{block}");
    report.push_str(&block);

    // `Sourced`: this reader reads UIA `HelpText`, so a zero here is a fact about the app —
    // modulo a read that failed, which `help_text` logs rather than counting.
    let census_block = description_census_report(label, &tree, DescriptionSourcing::Sourced);
    print!("{census_block}");
    report.push_str(&census_block);
    let described = description_census(&tree).described();

    // `COST_REPEATS` extra walks per app cost ~3s across this probe's four apps (measured on
    // box), well inside the 300s the bridge allows a run, so the block is unconditional rather
    // than behind a flag the bridge has no way to pass through.
    let cost_block = render_snapshot_cost(&mut a11y, &ctx);
    print!("{cost_block}");
    report.push_str(&cost_block);

    // Collect toggle-capable non-checkbox/radio nodes (evidence for ToggleButton row parity).
    let mut candidates = Vec::new();
    toggle_candidates(&tree.root, &mut candidates);
    if !candidates.is_empty() {
        use std::fmt::Write as _;
        let mut toggle_block = String::new();
        let _ = writeln!(
            toggle_block,
            "\n  toggle-capable (checkable, non-checkbox/radio): {} nodes",
            candidates.len()
        );
        let sample_count = candidates.len().min(5);
        if sample_count > 0 {
            let _ = writeln!(toggle_block, "    (first {sample_count}):");
            for node in candidates.iter().take(5) {
                let name = node.name.as_deref().unwrap_or("(no name)");
                let _ = writeln!(
                    toggle_block,
                    "      role={:?} raw_role={:?} name={:?}",
                    node.role, node.raw_role, name
                );
            }
        }
        print!("{toggle_block}");
        report.push_str(&toggle_block);
    }

    let adopted = p.active_window_handle();
    let _ = p.stop_app();
    // `stop_app` tears down the process tree glass launched, which is not always the process
    // that owns the window it adopted: a Win11 File Explorer folder window belongs to the
    // already-running shell `explorer.exe`, so the process glass spawned exits immediately and
    // the window it drove outlives teardown. This probe opens four apps in a row and would
    // otherwise leave one window behind on every run, so it closes what it adopted.
    close_adopted_window(adopted);
    std::thread::sleep(Duration::from_millis(500)); // let the app fully exit before the next probe

    let violations = mapped_token_violations(label, &tree);
    for v in &violations {
        let line = format!("\n  VIOLATION: {v}\n");
        print!("{line}");
        report.push_str(&line);
    }
    ProbeOutcome {
        violations,
        described,
    }
}

/// The evidence step behind the Windows half of the accessibility role-parity work: launch a
/// handful of stock apps and record each one's `role_histogram` — every native UIA token the
/// app actually emitted, unmapped (`AxRole::Other`) tokens first. The project's rule is probe
/// first, map second: a `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` may only get a
/// match arm for a token that showed up in output like this. So this test's job is to produce
/// that evidence. It asserts exactly one thing *about* the evidence — a token glass does map
/// must not come back `AxRole::Other`, which would mean the reader stopped feeding `map_role`
/// what it reads (see [`MAPPED_TOKENS`]) — and otherwise fails only when an app could not be
/// launched or snapshotted at all (see [`probe_role_histogram`]). Which role a token *should*
/// map to is never asserted here; that is the human's reading.
///
/// The histogram is both printed to stdout AND written to `role-histogram-windows.txt` under
/// [`artifacts_dir`] (see that function's doc for why the file is required, not just a nicety
/// on top of stdout): run through `scripts/test-windows.sh`, this test's stdout is captured by
/// the schtasks bounce into the interactive session and never reaches the caller, so the file
/// is the only way the evidence gets out of that path. Printing it too costs nothing and helps
/// anyone running the test directly on a box with a desktop session.
///
/// Runs charmap, Notepad, Task Manager, and File Explorer. charmap/Notepad alone gave clean
/// output but no tabular UI, so `DataItem` (UIA id 50029), `Header` (50034), and
/// `HeaderItem` (50035) — exactly the ids the `Table`/`Cell` `Gap` reasons in
/// `ROLE_SUPPORT` name — went unobserved, and Task Manager's process list and File Explorer's
/// file list were added to reach them. They got part of the way: both list views did emit
/// `Header` and `HeaderItem` for their column-header bars, but neither emitted `DataItem` at
/// all — their rows arrived as `TreeItem`, which is what the `Cell` row's Windows `Gap` reason
/// in `ROLE_SUPPORT` now records. (`Header`/`HeaderItem` are observed but deliberately left
/// unmapped: they are a grid's column headers, not document headings — see
/// `glass_a11y_windows::mapping`.) See [`taskmgr_spec`]/[`explorer_spec`] for why each is
/// expected to be discovered as reliably as charmap/Notepad despite neither being launched
/// exactly the same way.
///
/// The Settings app (`ms-settings:` via `explorer.exe`) was considered and left out: unlike
/// the four apps run here, its window has no process relationship to the process glass
/// launches — `explorer.exe` hands the request to the shell and exits, and the Settings
/// window belongs to an unrelated process the launched pid-set never contains — so discovery
/// could only find it through the title-substring fallback rung (a system-wide scan by
/// window title) rather than the pid-based one every app here uses. That fallback rung
/// exists in the backend for exactly this shape of app, but this probe declined to depend on
/// it without on-box verification that it resolves reliably.
///
/// This probe also can't surface a UIA `Window` with `IsDialog` true (the `Dialog` row's
/// Windows `Gap` reason in `ROLE_SUPPORT`): none of the four apps opens a dialog on a cold,
/// no-input launch, and `role_histogram` only ever sees what `glass-a11y-windows`'s reader
/// already reads into an `AxNode` — `IsDialog` is not among those fields (reading it is the
/// gap itself), so no launch sequence run through this probe could reveal it either way.
#[test]
#[ignore = "on-box only: needs the interactive desktop session; prints role histograms and writes them to .windows-artifacts/role-histogram-windows.txt, asserting only that a token glass maps did not come back Other"]
fn onbox_role_histogram_probe() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();

    let mut report = String::new();
    let mut violations = Vec::new();
    let mut described = 0usize;
    for (label, spec) in [
        ("charmap.exe (Character Map)", charmap_spec()),
        ("notepad.exe (Notepad)", notepad_spec()),
        ("taskmgr.exe (Task Manager)", taskmgr_spec()),
        ("explorer.exe (File Explorer)", explorer_spec()),
    ] {
        let outcome = probe_role_histogram(label, &spec, &mut report);
        violations.extend(outcome.violations);
        described += outcome.described;
    }
    save_report("role-histogram-windows.txt", &report);
    // Fail last, after every app has been probed and the artifacts file written: the histograms
    // are the evidence a violation has to be read against.
    assert!(
        violations.is_empty(),
        "a token glass maps came back unmapped:\n{}",
        violations.join("\n")
    );
    // The census prints without a caveat now that this reader sources the field, so a reader
    // regressed to `description: None` would print a plausible zero for every app and pass. Some
    // app in this set must report one: File Explorer's toolbar buttons and Task Manager's column
    // headers both carry HelpText. Deliberately NOT per-app — an app with no tooltips is a real
    // observation, not a failure.
    assert!(
        described >= 1,
        "no probed app reported a single described node: either every app lost its HelpText, or \
         the reader stopped sourcing it"
    );
}

/// The subscription must report a change a real UIA provider actually emitted — not merely
/// establish itself. Sequential on purpose: subscribe, act, then wait. `set_value` on charmap's
/// Edit field is the same change `onbox_a11y_set_value` already proves lands.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_subscription_reports_a_real_change() {
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
        limits: WalkLimits::DEFAULT,
    };

    // Before the first read, as the seam requires: a change landing between a read and the
    // subscription is announced to nobody.
    let mut signal = a11y
        .subscribe_changes(&ctx)
        .expect("UIA subscription must establish against charmap");

    let tree = a11y.snapshot(&ctx).expect("a11y snapshot");
    let mut field = None;
    first_role(&tree.root, AxRole::TextField, &mut field);
    let field = field.expect("charmap must expose a TextField (Edit)");
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };

    // Discriminating, not just documenting: the channel is open and buffers one item from the
    // moment of subscription, and the snapshot above is itself a full tree walk — so without this,
    // a stray event the walk trips on its own subscription could pass the test on a signal that
    // never actually saw `set_value`'s change.
    assert_eq!(
        signal.wait(Duration::from_millis(300)),
        glass_core::ChangeWait::Quiet,
        "no change should be pending yet — a real one hasn't been made"
    );

    a11y.set_value(&ctx, &target, "GLASSEVENT")
        .expect("set_value on the Edit field");

    let verdict = signal.wait(Duration::from_secs(2));
    let _ = p.stop_app();
    assert_eq!(
        verdict,
        glass_core::ChangeWait::Changed,
        "a ValueValue change on a real UIA provider must reach the signal"
    );
}

/// The other half, and the one the saving depends on: nothing happening must read as `Quiet`,
/// not as a change. A signal that reports `Changed` on an idle app costs a walk per interval —
/// polling, with a subscription's price on top.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a11y_subscription_is_quiet_on_an_idle_app() {
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
        limits: WalkLimits::DEFAULT,
    };
    let mut signal = a11y
        .subscribe_changes(&ctx)
        .expect("UIA subscription must establish against charmap");

    let verdict = signal.wait(Duration::from_millis(800));
    let _ = p.stop_app();
    assert_eq!(
        verdict,
        glass_core::ChangeWait::Quiet,
        "an untouched charmap must not report changes"
    );
}

/// Counts the walks a session performs. Wall-clock cannot tell a wait that waited efficiently
/// from one that walked slowly; only the count separates them.
///
/// Forwards every trait method. `subscribe_changes` has a default body, so a forgotten forward
/// would compile and silently disable the thing this measures — hence the counter on it too.
///
/// `signal_enabled: false` makes `subscribe_changes` behave like a backend with no event stream
/// at all — never calling `inner`, so no subscription thread is even started — which is the
/// baseline [`onbox_a_signal_more_than_halves_a_quiet_walk_count`] compares the live subscription
/// against: same app, same wait, the only variable is whether a signal was available to lean on.
struct Counting<A> {
    inner: A,
    walks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    signals: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    signal_enabled: bool,
}

impl<A: Accessibility> Accessibility for Counting<A> {
    fn snapshot(&mut self, ctx: &AxContext) -> glass_core::Result<glass_core::AxTree> {
        self.walks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.snapshot(ctx)
    }
    fn subscribe_changes(&mut self, ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        if !self.signal_enabled {
            return None;
        }
        let s = self.inner.subscribe_changes(ctx);
        if s.is_some() {
            self.signals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        s
    }
    fn set_value(
        &mut self,
        ctx: &AxContext,
        target: &AxTarget,
        text: &str,
    ) -> glass_core::Result<()> {
        self.inner.set_value(ctx, target, text)
    }
    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> glass_core::Result<()> {
        self.inner.invoke(ctx, target)
    }
}

/// A `Glass` session whose reader counts walks and subscriptions. `signal_enabled` gates whether
/// `subscribe_changes` ever hands back a working signal — see [`Counting`].
fn glass_counting(
    signal_enabled: bool,
) -> (
    Glass,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let walks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let signals = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (w, sg) = (walks.clone(), signals.clone());
    let factory: PlatformFactory = Box::new(move |_backend| {
        Ok(Backend {
            platform: Box::new(WindowsPlatform::new()?),
            accessibility: Some(Box::new(Counting {
                inner: WindowsA11y::new(),
                walks: w.clone(),
                signals: sg.clone(),
                signal_enabled,
            })),
        })
    });
    let dir = tempfile::tempdir().expect("tempdir for baseline store");
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    (
        Glass::new(factory, "windows".into(), BaselineStore::new(root), 100),
        walks,
        signals,
    )
}

/// The point of the subscription, measured against a real app: a wait for something that never
/// happens must stop re-reading the tree. A UIA walk is the most expensive of any backend —
/// 2360ms for 1500 nodes, measured on a Windows box — so this is where the saving is largest.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a_quiet_wait_stops_re_walking_the_tree() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let (mut glass, walks, signals) = glass_counting(true);
    glass.start(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    walks.store(0, std::sync::atomic::Ordering::Relaxed);
    signals.store(0, std::sync::atomic::Ordering::Relaxed);
    let out = glass
        .wait_for_element(&glass_core::WaitElementParams {
            name: Some("no such element in charmap".into()),
            role: None,
            value_contains: None,
            condition: glass_core::ElementCondition::Appears,
            interval_ms: 100,
            timeout_ms: 3_000,
        })
        .expect("wait");
    let walked = walks.load(std::sync::atomic::Ordering::Relaxed);
    let subscribed = signals.load(std::sync::atomic::Ordering::Relaxed);
    let _ = glass.stop();

    // Logged AND saved: `eprintln!` alone is invisible on a pass through the schtasks-bounce
    // harness (see [`artifacts_dir`] for why that is, and why the file is the only way anything
    // gets out) — save_report is what makes the number reproducible off-box, which is the entire
    // point of a walk-count test.
    let line = format!("quiet 3s wait at 100ms: {walked} walks\n");
    eprint!("{line}");
    save_report("onbox-walk-count-quiet-wait.txt", &line);
    assert!(!out.matched, "the element must not exist");
    // See `Counting`: without this the test could measure polling and pass.
    assert!(subscribed > 0, "no subscription was established");
    // One read at the start plus `QUIET_RUNS_BEFORE_REREAD`'s forced re-read about once a
    // second. The same wait polling takes many more; a UIA walk is slow enough that the
    // polling count is itself walk-bound rather than interval-bound.
    assert!(
        walked <= 5,
        "a quiet 3s wait walked {walked} times; the subscription is not suppressing walks"
    );
}

/// `walked <= 5` above bounds a number, not an improvement — a build that always walked 5 times
/// would pass it. This runs the identical 3s quiet wait through two sessions differing in exactly
/// one thing, whether `subscribe_changes` hands back a working signal (see
/// [`Counting::signal_enabled`]). That margin, not the raw walk count, is the saving worth
/// quoting.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a_signal_more_than_halves_a_quiet_walk_count() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();

    // Local, not a file-level helper: the only two callers are the two arms below, and inlining it
    // there would drown the one thing that differs between them (`signal_enabled`) in setup noise.
    fn quiet_wait_walks(signal_enabled: bool) -> (usize, usize) {
        let (mut glass, walks, signals) = glass_counting(signal_enabled);
        glass.start(&charmap_spec()).expect("start charmap");
        std::thread::sleep(Duration::from_millis(1500));

        walks.store(0, std::sync::atomic::Ordering::Relaxed);
        signals.store(0, std::sync::atomic::Ordering::Relaxed);
        let out = glass
            .wait_for_element(&glass_core::WaitElementParams {
                name: Some("no such element in charmap".into()),
                role: None,
                value_contains: None,
                condition: glass_core::ElementCondition::Appears,
                interval_ms: 100,
                timeout_ms: 3_000,
            })
            .expect("wait");
        let walked = walks.load(std::sync::atomic::Ordering::Relaxed);
        let subscribed = signals.load(std::sync::atomic::Ordering::Relaxed);
        let _ = glass.stop();
        assert!(!out.matched, "the element must not exist");
        (walked, subscribed)
    }

    let (with_signal, with_subs) = quiet_wait_walks(true);
    let (without_signal, without_subs) = quiet_wait_walks(false);
    assert!(
        with_subs > 0,
        "the signal-enabled run must have established a subscription"
    );
    assert_eq!(
        without_subs, 0,
        "the signal-disabled run must never report a subscription — otherwise this isn't \
         measuring what it claims to"
    );

    let line =
        format!("quiet 3s wait: with signal={with_signal} walks, without={without_signal} walks\n");
    eprint!("{line}");
    save_report("onbox-walk-count-comparison.txt", &line);
    assert!(
        with_signal * 2 < without_signal,
        "a live subscription must more than halve the walk count: with={with_signal} \
         without={without_signal}"
    );
}

/// A subscription establishes against a real app, and the wait still answers from its first read.
///
/// Deliberately *not* a test that a signal cannot suppress a match, which is a risk it cannot
/// reach: `glass_core`'s poll loop ticks before it ever pauses (`run_tick` starts true in
/// `poll_until_with_pause`), so no `ChangeSignal` is consulted before the first walk — and the
/// element here is already on screen when the wait starts, so that unconditional walk is what
/// matches. What can fail for a signal-related reason is the subscription never establishing.
/// A change arriving *after* the first read is the real risk, and
/// [`onbox_a_wait_wakes_on_a_late_change_from_another_thread`] is what covers it.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a_subscription_establishes_and_the_first_read_still_answers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let (mut glass, walks, signals) = glass_counting(true);
    glass.start(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    // Read the label out of the tree rather than hardcoding one: charmap's button wording is a
    // Windows-version detail, and this test is not about the wording.
    let tree = glass.a11y_snapshot(None).expect("a11y snapshot");
    let mut button = None;
    first_role(&tree.root, AxRole::Button, &mut button);
    let name = button
        .and_then(|b| b.name.clone())
        .expect("charmap exposes a named Button");

    walks.store(0, std::sync::atomic::Ordering::Relaxed);
    signals.store(0, std::sync::atomic::Ordering::Relaxed);
    let out = glass
        .wait_for_element(&glass_core::WaitElementParams {
            name: Some(name.clone()),
            role: None,
            value_contains: None,
            condition: glass_core::ElementCondition::Appears,
            interval_ms: 100,
            timeout_ms: 5_000,
        })
        .expect("wait");
    let walked = walks.load(std::sync::atomic::Ordering::Relaxed);
    let subscribed = signals.load(std::sync::atomic::Ordering::Relaxed);
    let _ = glass.stop();

    assert!(subscribed > 0, "no subscription was established");
    assert!(
        out.matched,
        "a wait for {name:?}, already on screen, must match"
    );
    assert!(
        walked <= 2,
        "an already-satisfied wait took {walked} walks; it should return on the first read"
    );
}

/// Pure Win32, independent of any `Platform`/`Glass` instance: the helper thread in
/// [`onbox_a_wait_wakes_on_a_late_change_from_another_thread`] must share no state with the
/// `Glass` session it acts on, so it finds the live charmap window itself rather than borrowing
/// anything glass tracked. Title alone, though: see [`find_charmap_window`] for why a caller
/// needing the *right* charmap window uses that instead of this directly.
fn find_window_by_title(title: &str) -> Option<i64> {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::{HSTRING, PCWSTR};
    // SAFETY: reads a null-terminated title string and returns the matching top-level HWND (or
    // an error, treated as "not found"); writes nothing.
    unsafe { FindWindowW(PCWSTR::null(), &HSTRING::from(title)) }
        .ok()
        .map(|h| h.0 as i64)
}

/// The pid of the most recently started `charmap.exe` process, via a `Get-Process` shell-out (the
/// same PowerShell-query pattern [`our_edge_count`] already uses elsewhere in this file) —
/// independent of any `Glass`/`Platform` instance. "Most recent" rather than "the only one": if an
/// earlier test leaked a `charmap.exe` — a documented, actively-checked-for risk in this file —
/// this test's own instance is still the newest by construction, since it was launched last.
fn newest_charmap_pid() -> Option<u32> {
    let ps = "(Get-Process charmap -ErrorAction SilentlyContinue | \
              Sort-Object StartTime -Descending | Select-Object -First 1).Id";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Find the live "Character Map" window and verify it belongs to `expected_pid`, the charmap.exe
/// this test started ([`newest_charmap_pid`]). [`find_window_by_title`] binds by title alone, so a
/// stale charmap.exe left by an earlier test could win the lookup and the helper thread would
/// toggle the wrong window — reaching the caller as an unexplained 5s timeout. Panics with both
/// pids named instead.
fn find_charmap_window(expected_pid: u32) -> i64 {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let handle = find_window_by_title("Character Map")
        .expect("an independent Win32 lookup must find a live 'Character Map' window");
    let hwnd = windows::Win32::Foundation::HWND(handle as *mut std::ffi::c_void);
    let mut owner_pid = 0u32;
    // SAFETY: writes the owning pid into our stack value; the HWND is only queried, never mutated.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
    assert_eq!(
        owner_pid, expected_pid,
        "the 'Character Map' window found by title belongs to pid {owner_pid}, not {expected_pid} \
         (the pid this test started) — a stale charmap.exe is likely still running from an earlier \
         test"
    );
    handle
}

/// Best-effort on-screen geometry for a raw HWND, via the plain window rect — good enough for
/// `AxContext::window`, which this path only uses for bounds translation inside a snapshot, never
/// for a click. Deliberately not `glass_windows`'s own DWM-extended `geometry_of`: that function is
/// crate-private, and reaching for it would mean this "independent" path borrowing glass's own
/// window code rather than rediscovering the window on its own.
fn window_rect_geometry(handle: i64) -> WindowGeometry {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let hwnd = windows::Win32::Foundation::HWND(handle as *mut std::ffi::c_void);
    let mut rect = RECT::default();
    // SAFETY: writes one RECT into our stack value; the HWND is only queried, never mutated. A
    // stale/invalid handle just fails, and the zeroed default below is a harmless fallback.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return WindowGeometry {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    WindowGeometry {
        x: rect.left,
        y: rect.top,
        width: (rect.right - rect.left).max(0) as u32,
        height: (rect.bottom - rect.top).max(0) as u32,
    }
}

/// The `AxContext` an independent UIA reader needs for a raw charmap HWND: no `Glass` state, just
/// the window and its rect. Shared by the helper thread below and by [`RestoreAdvancedView`],
/// which both act on the window without going through the `Glass` session under test.
fn independent_ctx(handle: i64) -> AxContext {
    AxContext {
        pids: vec![],
        window: window_rect_geometry(handle),
        window_handle: Some(handle),
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    }
}

/// Puts charmap's Advanced-view checkbox back in the state the test found it in, however the test
/// ends.
///
/// charmap persists that checkbox across launches, so a run that leaves it open changes what every
/// later charmap launch's tree holds: an open Advanced view adds an Edit, and two other tests in
/// this file bind to the *first* `TextField` in the tree. Drives the window through its own UIA
/// reader rather than the test's `Glass`, so it also works while unwinding from a panic — and
/// every step is best-effort for the same reason: a restore that panicked during a drop would
/// abort the process and bury the failure that got us here.
struct RestoreAdvancedView {
    handle: i64,
    was_checked: bool,
}

impl Drop for RestoreAdvancedView {
    fn drop(&mut self) {
        let ctx = independent_ctx(self.handle);
        let mut a11y = WindowsA11y::new();
        let Ok(tree) = a11y.snapshot(&ctx) else {
            return;
        };
        let mut cb = None;
        first_role_with_bounds(&tree.root, AxRole::CheckBox, &mut cb);
        let Some(cb) = cb.filter(|cb| cb.states.checked != self.was_checked) else {
            return;
        };
        let target = AxTarget {
            id: cb.id,
            role: cb.role,
            name: cb.name.clone(),
            bounds: cb.bounds,
            value: cb.value.clone(),
        };
        let _ = a11y.invoke(&ctx, &target);
    }
}

/// The only test that drives `ChangeWait::Changed => true` inside `Glass::wait_for_element`'s poll
/// loop from a change nothing but a real UIA event produced. The main thread blocks waiting for a
/// control that does not exist yet; a helper thread — its own `WindowsA11y`, its own `AxContext`,
/// the window pid-verified independently (see [`find_charmap_window`]), so no `Glass` state crosses
/// threads — sleeps, then actuates charmap's Advanced-view checkbox. Opening it reveals a "Search"
/// [`AxRole::Button`]: an on-box probe counted 26 nodes closed against 37 open, and "Search" was the
/// only new control whose name is unique in the tree — the others repeat labels ("Character set",
/// "Group by") that already appear elsewhere.
///
/// Neither assertion discriminates alone; each rules out a different broken build:
/// - No subscription (`subscribe_changes` always `None`): every 100ms tick reads regardless, so the
///   change is caught at the next tick — same elapsed time, but roughly one walk per tick.
/// - A subscription that never reports an event (always `Quiet`): `glass_core`'s `REREAD_AFTER`
///   forces a re-read once a second, so it cannot return before the next boundary — same low walk
///   count, but ~2000ms at the earliest.
///
/// Hence the helper acts at ~1400ms, mid-way through the second forced-re-read window (boundaries
/// ~1000ms and ~2000ms); acting near 700ms left too little margin against the first. Measured
/// on-box: `matched=true elapsed_ms=1500 walks=3` — the initial read, the ~1000ms forced re-read,
/// and the read the event triggers.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a_wait_wakes_on_a_late_change_from_another_thread() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let (mut glass, walks, signals) = glass_counting(true);
    glass.start(&charmap_spec()).expect("start charmap");
    std::thread::sleep(Duration::from_millis(1500));

    let pid = newest_charmap_pid().expect("must find the charmap.exe process this test started");
    let handle = find_charmap_window(pid);

    // charmap persists the Advanced-view checkbox across launches, so normalize to closed first:
    // the helper thread's job is to open it, and "Search" must not already be on screen when the
    // timed wait below starts. The guard puts it back — see [`RestoreAdvancedView`] for what a
    // stray open Advanced view does to the tests that run after this one.
    let tree = glass.a11y_snapshot(None).expect("initial snapshot");
    let mut cb = None;
    first_role_with_bounds(&tree.root, AxRole::CheckBox, &mut cb);
    let cb = cb.expect("charmap exposes the Advanced-view CheckBox");
    let restore = RestoreAdvancedView {
        handle,
        was_checked: cb.states.checked,
    };
    if cb.states.checked {
        glass
            .click_element(cb.id)
            .expect("close advanced view to normalize to a known starting state");
        std::thread::sleep(Duration::from_millis(800));
    }

    walks.store(0, std::sync::atomic::Ordering::Relaxed);
    signals.store(0, std::sync::atomic::Ordering::Relaxed);
    let helper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1400));
        let mut a11y = WindowsA11y::new();
        let ctx = independent_ctx(handle);
        let tree = a11y.snapshot(&ctx).expect("helper thread's own snapshot");
        let mut cb = None;
        first_role_with_bounds(&tree.root, AxRole::CheckBox, &mut cb);
        let cb = cb.expect("checkbox still present for the helper thread");
        let target = AxTarget {
            id: cb.id,
            role: cb.role,
            name: cb.name.clone(),
            bounds: cb.bounds,
            value: cb.value.clone(),
        };
        a11y.invoke(&ctx, &target)
            .expect("helper thread opens advanced view");
    });

    // Captured, not unwrapped, before the join: if `wait_for_element` errored, `.expect` below
    // would panic and unwind — and with the helper still un-joined at that point, its `JoinHandle`
    // would be dropped while the thread is still doing live Win32/UIA work against this window.
    // `SERIAL` releases as soon as this test's guard drops on the panic, so the very next on-box
    // test could start and race the orphaned helper. Joining before any panic-capable call closes
    // that gap.
    let wait_result = glass.wait_for_element(&glass_core::WaitElementParams {
        name: Some("Search".into()),
        role: Some(AxRole::Button),
        value_contains: None,
        condition: glass_core::ElementCondition::Appears,
        interval_ms: 100,
        timeout_ms: 5_000,
    });
    let walked = walks.load(std::sync::atomic::Ordering::Relaxed);
    let subscribed = signals.load(std::sync::atomic::Ordering::Relaxed);
    let helper_result = helper.join();
    // Explicitly before `stop`, which the guard's own drop would otherwise run after: restoring
    // the checkbox means driving charmap's UI, and a stopped charmap has none. On a panic the
    // guard drops first anyway, since it was declared after `glass`.
    drop(restore);
    let _ = glass.stop();

    helper_result.expect("helper thread must not panic");
    let out = wait_result.expect("wait");

    let line = format!(
        "late-change wait: matched={} elapsed_ms={} walks={}\n",
        out.matched, out.elapsed_ms, walked
    );
    eprint!("{line}");
    save_report("onbox-late-change-wait.txt", &line);
    assert!(subscribed > 0, "no subscription was established");
    assert!(
        out.matched,
        "the Search button must appear once advanced view opens"
    );
    // Rules out "no subscription at all" — see the doc comment above. A polling build would walk
    // roughly once per 100ms tick over ~1.4s (~14); a working signal walks only the initial read,
    // the ~1000ms forced re-read, and the event itself (~3, matching the quiet-wait tests' count).
    assert!(
        walked <= 7,
        "the wait walked {walked} times reaching the change; a live subscription should keep this \
         near the quiet-wait tests' count (~3), not polling's (~14)"
    );
    // Rules out "subscribed but always Quiet" — see the doc comment above. That build cannot answer
    // before the next forced-re-read boundary at ~2000ms; a working signal answers near the
    // helper's own 1400ms action instant plus one walk.
    assert!(
        out.elapsed_ms < 1_800,
        "a wait woken by a real UIA change should return near the helper's ~1400ms action instant \
         (measured on-box: 1500ms), well before the ~2000ms forced-re-read boundary an \
         always-Quiet signal would need; took {}ms",
        out.elapsed_ms
    );
}

/// The WinForms fixture, whose "Save" button becomes enabled 4s after launch.
fn winforms_fixture_spec() -> AppSpec {
    let script = repo_root().join("crates/glass-windows/fixture/a11y_fixture.ps1");
    AppSpec {
        build: None,
        run: vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: None,
        env: vec![],
        // The hint matters here in a way it does not for Notepad: the window belongs to
        // `powershell.exe`, so pid-set membership alone would also match a console window.
        window_hint: Some(WindowHint {
            title: Some("glass a11y fixture".into()),
            class: None,
        }),
        timeout_ms: 15_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

/// The documented cost of the `IsEnabled` gap, asserted rather than described.
///
/// A WinForms control becoming enabled announces nothing, so this wait cannot be woken by an
/// event — it can only be rescued by `glass_core`'s once-a-second forced re-read (`REREAD_AFTER`).
/// Both halves matter: that it still matches is the correctness claim; that it took more than one
/// walk rules out the wait's first read as the source of the match.
///
/// `walked > 1` does not prove no event fired — an announced transition and a forced re-read land
/// too close in walk count to tell apart. That WinForms never announces `IsEnabled` comes from the
/// on-box probe, not from this test. A failure says nothing about the bridge either: `walked > 1`
/// fails only at `walked == 1`, meaning the fixture's 4s flip landed before the wait began, which
/// is a timing problem here — an announced transition would wake the wait at walk #2 and pass.
#[test]
#[ignore = "on-box only: needs the interactive desktop session"]
fn onbox_a_wait_for_enabled_falls_back_to_the_forced_reread() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dpi_aware_once();
    let (mut glass, walks, signals) = glass_counting(true);
    glass
        .start(&winforms_fixture_spec())
        .expect("start fixture");
    std::thread::sleep(Duration::from_millis(1200));

    walks.store(0, std::sync::atomic::Ordering::Relaxed);
    signals.store(0, std::sync::atomic::Ordering::Relaxed);
    let out = glass
        .wait_for_element(&glass_core::WaitElementParams {
            name: Some("Save".into()),
            role: None,
            value_contains: None,
            condition: glass_core::ElementCondition::Enabled,
            interval_ms: 100,
            timeout_ms: 10_000,
        })
        .expect("wait");
    let walked = walks.load(std::sync::atomic::Ordering::Relaxed);
    let subscribed = signals.load(std::sync::atomic::Ordering::Relaxed);
    let _ = glass.stop();

    eprintln!("wait for Save to enable: {walked} walks");
    save_report(
        "onbox-wait-for-enabled-reread.txt",
        &format!("wait for Save to enable: {walked} walks\n"),
    );
    assert!(subscribed > 0, "no subscription was established");
    assert!(
        out.matched,
        "the forced re-read must still find Save enabled; the subscription must not be able to \
         make a wait miss a change the platform declined to announce"
    );
    assert!(
        walked > 1,
        "the wait matched in its first read ({walked} walk), so Save was already enabled when it \
         started — the fixture's flip beat the wait, and this run measured nothing about the \
         fallback"
    );
}
