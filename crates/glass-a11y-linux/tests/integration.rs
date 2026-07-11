//! End-to-end: glass launches the GTK4 fixture and the AT-SPI reader snapshots its
//! real accessibility tree. `#[ignore]`d — run via `scripts/test-a11y.sh`. The tests
//! launch with `a11y: true`, so glass spawns its OWN isolated session bus + AT-SPI
//! registry (no external dbus-run-session / at-spi-bus-launcher needed). The X11
//! backend self-spawns a private Xvfb for the fixture to render into.

use glass_core::{AppSpec, Backend, BaselineStore, Glass, PlatformFactory, WindowHint};

fn glass_x11_with_a11y() -> Glass {
    let factory: PlatformFactory = Box::new(|_backend| {
        Ok(Backend {
            platform: Box::new(glass_x11::X11Platform::from_env()?),
            accessibility: Some(Box::new(glass_a11y_linux::LinuxA11y::new())),
        })
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
}

/// Regression: the real MCP runs the (blocking) `glass_start` from *inside* its
/// multi-thread tokio runtime. `PrivateBus::start` must bring up the a11y bus without
/// nesting a runtime — nesting panics ("Cannot start a runtime from within a runtime"),
/// which leaves the MCP request unanswered (the app appears to hang forever) and leaks the
/// bus children. Launch the fixture from inside a multi-thread runtime and require success.
/// Host-safe under test-a11y.sh's throwaway XDG_RUNTIME_DIR.
#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn a11y_launch_succeeds_from_within_a_tokio_runtime() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut glass = glass_x11_with_a11y();
        glass
            .start(&AppSpec {
                build: None,
                run: vec!["python3".into(), fixture.into()],
                cwd: None,
                env: vec![
                    ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                    ("GDK_BACKEND".into(), "x11".into()),
                ],
                window_hint: Some(WindowHint {
                    title: Some("Glass A11y Fixture".into()),
                    class: None,
                }),
                timeout_ms: 35_000,
                sandbox: glass_core::SandboxLevel::Off,
                a11y: true,
            })
            .expect("a11y launch from within a tokio runtime must not panic/hang");
        glass.stop().expect("stop");
    });
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn snapshot_finds_gtk_widgets() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch GTK fixture");

    // GTK4 apps connecting to a fresh D-Bus session (as in test-a11y.sh) go
    // through xdg-desktop-portal initialisation before presenting the window.
    // The portal's secrets-service probe can take ~25 s on a system without
    // gnome-keyring on the private bus, so wait long enough that the window is
    // mapped and the AT-SPI tree is populated before we snapshot.
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let tree = glass.a11y_snapshot().expect("a11y snapshot");
    let outline = tree.to_outline();
    assert!(
        outline.contains("Button \"Save\""),
        "no Save button in:\n{outline}"
    );
    assert!(
        outline.contains("CheckBox \"Enable\""),
        "no Enable checkbox in:\n{outline}"
    );

    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn snapshot_reads_entry_value() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch GTK fixture");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let tree = glass.a11y_snapshot().expect("a11y snapshot");
    let outline = tree.to_outline();
    // GTK4 Gtk.Entry exposes AT-SPI Role::Text, which maps to AxRole::TextArea.
    // read_value handles both TextField and TextArea via the Text interface.
    let entry = find_role(&tree.root, glass_core::AxRole::TextArea)
        .or_else(|| find_role(&tree.root, glass_core::AxRole::TextField))
        .unwrap_or_else(|| panic!("no TextArea/TextField node in tree:\n{outline}"));
    assert_eq!(
        entry.value.as_deref(),
        Some("hello"),
        "entry value should be read; tree:\n{outline}"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_changes_entry() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let tree = glass.a11y_snapshot().expect("snapshot");
    // GTK4 Gtk.Entry exposes AT-SPI Role::Text -> maps to AxRole::TextArea.
    let entry = find_role(&tree.root, glass_core::AxRole::TextArea)
        .or_else(|| find_role(&tree.root, glass_core::AxRole::TextField))
        .expect("entry");
    let entry_id = entry.id;
    glass.set_value(entry_id, "changed").expect("set_value");

    // Re-snapshot and confirm the new value.
    let tree2 = glass.a11y_snapshot().expect("snapshot 2");
    let entry2 = find_role(&tree2.root, glass_core::AxRole::TextArea)
        .or_else(|| find_role(&tree2.root, glass_core::AxRole::TextField))
        .expect("entry 2");
    assert_eq!(
        entry2.value.as_deref(),
        Some("changed"),
        "value should be updated"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_on_button_is_not_editable() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let tree = glass.a11y_snapshot().expect("snapshot");
    let button = find_role(&tree.root, glass_core::AxRole::Button).expect("button");
    let err = glass.set_value(button.id, "x").unwrap_err();
    assert!(
        matches!(err, glass_core::GlassError::AxElementNotEditable(_)),
        "got: {err:?}"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_changes_spinbutton() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    // A GtkSpinButton exposes both EditableText and Value; set_value must write through the
    // Value interface (the only one that commits to the adjustment) rather than the entry
    // buffer, which reverts.
    let tree = glass.a11y_snapshot().expect("snapshot");
    let spin = find_role(&tree.root, glass_core::AxRole::SpinButton).expect("spinbutton");
    assert_eq!(spin.value.as_deref(), Some("1"), "fixture starts at 1");
    glass.set_value(spin.id, "4").expect("set_value");

    let tree2 = glass.a11y_snapshot().expect("snapshot 2");
    let spin2 = find_role(&tree2.root, glass_core::AxRole::SpinButton).expect("spinbutton 2");
    assert_eq!(
        spin2.value.as_deref(),
        Some("4"),
        "value should commit through the Value interface, not silently revert"
    );
    glass.stop().expect("stop");
}

// Pre-order search for the first node of a given role.
fn find_role(node: &glass_core::AxNode, role: glass_core::AxRole) -> Option<&glass_core::AxNode> {
    if node.role == role {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_role(c, role))
}

// Pre-order search for the first node of a given role whose name matches.
fn find_role_name<'a>(
    node: &'a glass_core::AxNode,
    role: glass_core::AxRole,
    name: &str,
) -> Option<&'a glass_core::AxNode> {
    if node.role == role && node.name.as_deref() == Some(name) {
        return Some(node);
    }
    node.children
        .iter()
        .find_map(|c| find_role_name(c, role, name))
}

// Pre-order search for the first node with an exact accessible name, any role — an
// open dropdown's option row is a ListItem, not addressable by role alone the way
// find_role_name is.
fn find_node<'a>(node: &'a glass_core::AxNode, name: &str) -> Option<&'a glass_core::AxNode> {
    if node.name.as_deref() == Some(name) {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_node(c, name))
}

fn launch_fixture() -> Glass {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch");
    std::thread::sleep(std::time::Duration::from_millis(3_000));
    glass
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_toggles_switch() {
    let mut glass = launch_fixture();
    // The GtkSwitch "Active" starts off; set_value must flip it on via the Action
    // "toggle" (it exposes no text/Value interface).
    let tree = glass.a11y_snapshot().expect("snapshot");
    let sw = find_role_name(&tree.root, glass_core::AxRole::CheckBox, "Active").expect("switch");
    assert!(!sw.states.checked, "switch starts off");
    glass.set_value(sw.id, "true").expect("set_value true"); // set_value polls until applied

    let tree2 = glass.a11y_snapshot().expect("snapshot 2");
    let sw2 =
        find_role_name(&tree2.root, glass_core::AxRole::CheckBox, "Active").expect("switch 2");
    assert!(
        sw2.states.checked,
        "switch should be on after set_value true"
    );

    // Idempotent: setting true again is a no-op, and false turns it back off.
    glass
        .set_value(sw2.id, "true")
        .expect("set_value true again");
    let tree3 = glass.a11y_snapshot().expect("snapshot 3");
    let sw3 =
        find_role_name(&tree3.root, glass_core::AxRole::CheckBox, "Active").expect("switch 3");
    assert!(sw3.states.checked, "still on after idempotent set");
    glass.set_value(sw3.id, "false").expect("set_value false");
    let tree4 = glass.a11y_snapshot().expect("snapshot 4");
    let sw4 =
        find_role_name(&tree4.root, glass_core::AxRole::CheckBox, "Active").expect("switch 4");
    assert!(
        !sw4.states.checked,
        "switch should be off after set_value false"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_selects_dropdown_option() {
    let mut glass = launch_fixture();
    // The GtkDropDown starts on "Acme"; set_value must select "Globex" by opening
    // the popup and picking the option through the Selection interface.
    let tree = glass.a11y_snapshot().expect("snapshot");
    let combo = find_role(&tree.root, glass_core::AxRole::ComboBox).expect("combo box");
    assert_eq!(combo.name.as_deref(), Some("Acme"), "starts on Acme");
    glass
        .set_value(combo.id, "Globex")
        .expect("set_value Globex");

    std::thread::sleep(std::time::Duration::from_millis(500));
    let tree2 = glass.a11y_snapshot().expect("snapshot 2");
    let combo2 = find_role(&tree2.root, glass_core::AxRole::ComboBox).expect("combo box 2");
    assert_eq!(
        combo2.name.as_deref(),
        Some("Globex"),
        "dropdown should now show Globex"
    );

    // A non-existent option returns a clear error listing the choices.
    let err = glass.set_value(combo2.id, "Nope").unwrap_err();
    assert!(
        matches!(err, glass_core::GlassError::AxOptionNotFound(_, _, _)),
        "got: {err:?}"
    );
    glass.stop().expect("stop");
}

// Regression (#85a): X11 capture_frame must read from the ROOT window at the target
// window's screen offset, not the window's own drawable, so an overlapping
// override-redirect popover (a separate top-level X window — e.g. the popup a
// GtkDropDown opens) shows up in the captured pixels instead of being invisible.
//
// A whole-frame pixel diff is too weak a check here: opening the GtkDropDown also
// repaints the combo button itself (pressed style / arrow), which lives in the main
// window's own drawable and would change even under the OLD (buggy) window-drawable
// capture. So the comparison is scoped to the region strictly BELOW the combo button —
// where the popover's option rows draw, and where the main window's own content (e.g.
// the switch) does NOT change when the dropdown opens. Only the new root capture can
// see anything change there, because that's where the popover (a separate top-level X
// window) is composited on top; the old window-drawable capture would show that band
// completely unchanged. So a substantial pixel change confined to that band is
// specific evidence the popover itself was captured, not just the button's own repaint.
#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn screenshot_includes_open_popover() {
    let mut glass = launch_fixture();
    let tree = glass.a11y_snapshot().expect("snapshot");
    let combo = find_role(&tree.root, glass_core::AxRole::ComboBox).expect("combo");
    let combo_id = combo.id;
    let combo_bounds = combo.bounds.expect("combo box must report bounds");
    let before = glass.screenshot(None, None).expect("before");
    glass.click_element(combo_id).expect("open");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let after = glass.screenshot(None, None).expect("after");
    assert_eq!((before.width, before.height), (after.width, after.height));

    let width = after.width as usize;
    let height = after.height as usize;
    // Row just below the combo button, window-relative. Add a margin below the
    // button's AT-SPI-reported bottom edge: opening the dropdown also repaints the
    // button itself (pressed style / arrow / focus ring), and that repaint's
    // border/shadow antialiasing bleeds a few pixels past the button's semantic
    // bounds — measured empirically at ~3px on this fixture. A margin of half the
    // button's own height comfortably clears that bleed regardless of exact theme
    // metrics, so this region only catches the popover's own rows, not runoff from
    // the button. Computed in i64 so a pathological (e.g. negative or off-screen)
    // bounds value can't wrap a usize instead of failing the bounds check below.
    let margin = (combo_bounds.height / 2).max(1) as i64;
    let y_start_signed = combo_bounds.y as i64 + combo_bounds.height as i64 + margin;
    assert!(
        y_start_signed >= 0 && (y_start_signed as usize) < height,
        "combo bounds {combo_bounds:?} (margin {margin}) leave no row below the button in a \
         {width}x{height} frame; can't scope the popover-region diff"
    );
    let y_start = y_start_signed as usize;
    // Tightly-packed RGBA8, row-major: row `y` starts at byte `y * width * 4`.
    let byte_start = y_start * width * 4;
    let changed_below_combo = before.pixels[byte_start..]
        .iter()
        .zip(after.pixels[byte_start..].iter())
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed_below_combo > 500,
        "opening the dropdown should change many captured pixels in the region below the \
         combo button, where the popover's option rows draw (popover composited); \
         changed_below_combo={changed_below_combo}"
    );
    glass.stop().expect("stop");
}

// Regression (#84): the open GtkDropDown's option rows render in a separate
// override-redirect popover window, not the app's active window — click_element must
// auto-route the click into that popover (see glass_core::session's owning_popover /
// menu_container_bounds) rather than clicking the active window's coordinates and
// silently missing. Exercises the full click_element -> commit path end to end.
#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn click_element_commits_dropdown_option() {
    let mut glass = launch_fixture();
    let tree = glass.a11y_snapshot().expect("snapshot");
    let combo = find_role(&tree.root, glass_core::AxRole::ComboBox).expect("combo");
    glass.click_element(combo.id).expect("open");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let t2 = glass.a11y_snapshot().expect("snap2");
    let globex = find_node(&t2.root, "Globex").expect("globex");
    glass.click_element(globex.id).expect("click option");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let t3 = glass.a11y_snapshot().expect("snap3");
    assert_eq!(
        find_role(&t3.root, glass_core::AxRole::ComboBox).and_then(|c| c.name.as_deref()),
        Some("Globex"),
        "clicking the popover's option row should commit the dropdown selection"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs Xvfb + GTK4 fixture; run via scripts/test-a11y.sh"]
fn snapshot_without_a11y_flag_errors() {
    // With a11y:false (the default), glass spawns NO private bus, so the reader has no
    // bus address and must return a clear "relaunch with a11y:true" error rather than
    // falling back to the ambient host bus.
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a11y_fixture.py"
    );
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), fixture.into()],
            cwd: None,
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        })
        .expect("launch GTK fixture");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let err = glass
        .a11y_snapshot()
        .expect_err("snapshot must fail without a11y:true");
    match err {
        glass_core::GlassError::AccessibilityUnavailable(msg) => {
            assert!(msg.contains("a11y:true"), "unexpected message: {msg}");
        }
        other => panic!("expected AccessibilityUnavailable, got: {other:?}"),
    }
    glass.stop().expect("stop");
}

// Phase 2: a11y must work when the *run* is sandboxed. glass binds its private a11y bus dir
// (path sockets) into the run's bwrap. The fixture lives under $HOME (shadowed by the sandbox
// home-tmpfs), so we set cwd to the fixtures dir (a home-descendant → bound rw) and run the
// script relative to it.
fn sandboxed_a11y_finds_widgets(level: glass_core::SandboxLevel) {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut glass = glass_x11_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), "a11y_fixture.py".into()],
            cwd: Some(fixtures.into()),
            env: vec![
                ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
                ("GDK_BACKEND".into(), "x11".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: level,
            a11y: true,
        })
        .unwrap_or_else(|e| panic!("launch GTK fixture sandboxed ({level:?}): {e}"));
    std::thread::sleep(std::time::Duration::from_millis(3_000));
    let outline = glass
        .a11y_snapshot()
        .expect("a11y snapshot (sandboxed)")
        .to_outline();
    assert!(
        outline.contains("Button \"Save\""),
        "no Save button (sandboxed {level:?}):\n{outline}"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture + bwrap; run via scripts/test-a11y.sh"]
fn a11y_works_under_default_sandbox() {
    sandboxed_a11y_finds_widgets(glass_core::SandboxLevel::Default);
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture + bwrap; run via scripts/test-a11y.sh"]
fn a11y_works_under_strict_sandbox() {
    sandboxed_a11y_finds_widgets(glass_core::SandboxLevel::Strict);
}

// ---- Wayland a11y (#1 Phase 3 / #6): same reader, app launched under headless sway ----

fn glass_wayland_with_a11y() -> Glass {
    let factory: PlatformFactory = Box::new(|_backend| {
        Ok(Backend {
            platform: Box::new(glass_wayland::WaylandPlatform::new()?),
            accessibility: Some(Box::new(glass_a11y_linux::LinuxA11y::new())),
        })
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    Glass::new(factory, "wayland".into(), BaselineStore::new(root), 100)
}

fn wayland_a11y_finds_widgets(level: glass_core::SandboxLevel) {
    // GTK4's GL renderer fails under headless sway → cairo (software) renderer. Run the script
    // relative to cwd=fixtures so it's reachable inside the sandbox (its $HOME is tmpfs'd).
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut glass = glass_wayland_with_a11y();
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["python3".into(), "a11y_fixture.py".into()],
            cwd: Some(fixtures.into()),
            env: vec![
                ("GSK_RENDERER".into(), "cairo".into()),
                ("GDK_BACKEND".into(), "wayland".into()),
            ],
            window_hint: Some(WindowHint {
                title: Some("Glass A11y Fixture".into()),
                class: None,
            }),
            timeout_ms: 35_000,
            sandbox: level,
            a11y: true,
        })
        .unwrap_or_else(|e| panic!("wayland a11y launch ({level:?}): {e}"));
    std::thread::sleep(std::time::Duration::from_millis(3_000));
    let outline = glass
        .a11y_snapshot()
        .expect("a11y snapshot (wayland)")
        .to_outline();
    assert!(
        outline.contains("Button \"Save\""),
        "no Save button (wayland {level:?}):\n{outline}"
    );
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs sway + dbus + at-spi + GTK4 fixture; run via scripts/test-a11y.sh"]
fn wayland_a11y_off() {
    wayland_a11y_finds_widgets(glass_core::SandboxLevel::Off);
}

#[test]
#[ignore = "needs sway + dbus + at-spi + GTK4 fixture + bwrap; run via scripts/test-a11y.sh"]
fn wayland_a11y_default_sandbox() {
    wayland_a11y_finds_widgets(glass_core::SandboxLevel::Default);
}

#[test]
#[ignore = "needs sway + dbus + at-spi + GTK4 fixture + bwrap; run via scripts/test-a11y.sh"]
fn wayland_a11y_strict_sandbox() {
    wayland_a11y_finds_widgets(glass_core::SandboxLevel::Strict);
}

// ---- #86: scroll_to_element against a virtualized GtkListView ----

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn scroll_to_element_reaches_a_virtualized_offscreen_row() {
    let mut glass = launch_fixture();
    // Precondition: "Row 060" is virtualized — absent from the initial tree. If it
    // were present, the test would prove nothing (no scroll needed).
    let tree = glass.a11y_snapshot().expect("snapshot");
    assert!(
        find_node(&tree.root, "Row 060").is_none(),
        "Row 060 should be off-screen/virtualized at start, but was in the tree"
    );
    // Anchor the scroll over the list: use a realized early row's center, so the
    // wheel lands on the scroller regardless of exact window layout.
    let seed = find_node(&tree.root, "Row 000").expect("an early row realized at start");
    let sb = seed.bounds.expect("row bounds");
    let anchor = (sb.x + sb.width as i32 / 2, sb.y + sb.height as i32 / 2);

    let out = glass
        .scroll_to_element(&glass_core::ScrollToElementParams {
            name: Some("Row 060".into()),
            role: None,
            value_contains: None,
            direction: Some(glass_core::ScrollDirection::Down),
            anchor: Some(anchor),
            step: glass_core::SCROLL_TO_DEFAULT_STEP,
            timeout_ms: glass_core::SCROLL_TO_DEFAULT_TIMEOUT_MS,
        })
        .expect("scroll_to_element");
    assert!(
        out.matched,
        "Row 060 should be reached by scrolling; {out:?}"
    );
    assert!(out.steps > 0, "should have scrolled at least once");
    let elem = out.element.expect("matched element");
    assert!(
        elem.name.as_deref().is_some_and(|n| n.contains("Row 060")),
        "matched the wrong node: {:?}",
        elem.name
    );

    // The returned id is from the final snapshot → click it and confirm the fixture
    // selected exactly that row (via its stdout).
    glass
        .click_element(elem.id)
        .expect("click the realized row");
    let seen = glass
        .wait_for_log(&glass_core::WaitLogParams {
            contains: "SELECTED Row 060".into(),
            stream: None,
            cursor: None,
            interval_ms: 100,
            timeout_ms: 4000,
        })
        .expect("wait_for_log");
    assert!(seen.matched, "click did not select Row 060");
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn scroll_to_element_reports_unmatched_for_an_absent_row() {
    let mut glass = launch_fixture();
    let tree = glass.a11y_snapshot().expect("snapshot");
    let seed = find_node(&tree.root, "Row 000").expect("an early row realized at start");
    let sb = seed.bounds.expect("row bounds");
    let anchor = (sb.x + sb.width as i32 / 2, sb.y + sb.height as i32 / 2);

    // "Row 999" does not exist (rows are 000..079): a full bidirectional sweep must
    // saturate the down end, reverse, saturate the up end, and terminate with
    // matched:false and reversed:true, not hang.
    let out = glass
        .scroll_to_element(&glass_core::ScrollToElementParams {
            name: Some("Row 999".into()),
            role: None,
            value_contains: None,
            direction: Some(glass_core::ScrollDirection::Down),
            anchor: Some(anchor),
            step: glass_core::SCROLL_TO_DEFAULT_STEP,
            timeout_ms: glass_core::SCROLL_TO_DEFAULT_TIMEOUT_MS,
        })
        .expect("scroll_to_element");
    assert!(!out.matched, "Row 999 must not match; {out:?}");
    assert!(out.element.is_none());
    assert!(out.reversed, "should have swept both ends; {out:?}");
    glass.stop().expect("stop");
}
