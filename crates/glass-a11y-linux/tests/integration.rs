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

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn snapshot_finds_gtk_widgets() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a11y_fixture.py");
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
            window_hint: Some(WindowHint { title: Some("Glass A11y Fixture".into()), class: None }),
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
    assert!(outline.contains("Button \"Save\""), "no Save button in:\n{outline}");
    assert!(outline.contains("CheckBox \"Enable\""), "no Enable checkbox in:\n{outline}");

    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn snapshot_reads_entry_value() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a11y_fixture.py");
    let mut glass = glass_x11_with_a11y();
    glass.start(&AppSpec {
        build: None,
        run: vec!["python3".into(), fixture.into()],
        cwd: None,
        env: vec![
            ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
            ("GDK_BACKEND".into(), "x11".into()),
        ],
        window_hint: Some(WindowHint { title: Some("Glass A11y Fixture".into()), class: None }),
        timeout_ms: 35_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: true,
    }).expect("launch GTK fixture");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let tree = glass.a11y_snapshot().expect("a11y snapshot");
    let outline = tree.to_outline();
    // GTK4 Gtk.Entry exposes AT-SPI Role::Text, which maps to AxRole::TextArea.
    // read_value handles both TextField and TextArea via the Text interface.
    let entry = find_role(&tree.root, glass_core::AxRole::TextArea)
        .or_else(|| find_role(&tree.root, glass_core::AxRole::TextField))
        .unwrap_or_else(|| panic!("no TextArea/TextField node in tree:\n{outline}"));
    assert_eq!(entry.value.as_deref(), Some("hello"), "entry value should be read; tree:\n{outline}");
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_changes_entry() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a11y_fixture.py");
    let mut glass = glass_x11_with_a11y();
    glass.start(&AppSpec {
        build: None, run: vec!["python3".into(), fixture.into()], cwd: None,
        env: vec![("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()), ("GDK_BACKEND".into(), "x11".into())],
        window_hint: Some(WindowHint { title: Some("Glass A11y Fixture".into()), class: None }),
        timeout_ms: 35_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: true,
    }).expect("launch");
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
    assert_eq!(entry2.value.as_deref(), Some("changed"), "value should be updated");
    glass.stop().expect("stop");
}

#[test]
#[ignore = "needs session bus + AT-SPI registry + GTK4 fixture; run via scripts/test-a11y.sh"]
fn set_value_on_button_is_not_editable() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a11y_fixture.py");
    let mut glass = glass_x11_with_a11y();
    glass.start(&AppSpec {
        build: None, run: vec!["python3".into(), fixture.into()], cwd: None,
        env: vec![("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()), ("GDK_BACKEND".into(), "x11".into())],
        window_hint: Some(WindowHint { title: Some("Glass A11y Fixture".into()), class: None }),
        timeout_ms: 35_000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: true,
    }).expect("launch");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let tree = glass.a11y_snapshot().expect("snapshot");
    let button = find_role(&tree.root, glass_core::AxRole::Button).expect("button");
    let err = glass.set_value(button.id, "x").unwrap_err();
    assert!(matches!(err, glass_core::GlassError::AxElementNotEditable(_)), "got: {err:?}");
    glass.stop().expect("stop");
}

// Pre-order search for the first node of a given role.
fn find_role(node: &glass_core::AxNode, role: glass_core::AxRole) -> Option<&glass_core::AxNode> {
    if node.role == role { return Some(node); }
    node.children.iter().find_map(|c| find_role(c, role))
}

#[test]
#[ignore = "needs Xvfb + GTK4 fixture; run via scripts/test-a11y.sh"]
fn snapshot_without_a11y_flag_errors() {
    // With a11y:false (the default), glass spawns NO private bus, so the reader has no
    // bus address and must return a clear "relaunch with a11y:true" error rather than
    // falling back to the ambient host bus.
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a11y_fixture.py");
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
            window_hint: Some(WindowHint { title: Some("Glass A11y Fixture".into()), class: None }),
            timeout_ms: 35_000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        })
        .expect("launch GTK fixture");
    std::thread::sleep(std::time::Duration::from_millis(3_000));

    let err = glass.a11y_snapshot().expect_err("snapshot must fail without a11y:true");
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
            window_hint: Some(WindowHint { title: Some("Glass A11y Fixture".into()), class: None }),
            timeout_ms: 35_000,
            sandbox: level,
            a11y: true,
        })
        .unwrap_or_else(|e| panic!("launch GTK fixture sandboxed ({level:?}): {e}"));
    std::thread::sleep(std::time::Duration::from_millis(3_000));
    let outline = glass.a11y_snapshot().expect("a11y snapshot (sandboxed)").to_outline();
    assert!(outline.contains("Button \"Save\""), "no Save button (sandboxed {level:?}):\n{outline}");
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
