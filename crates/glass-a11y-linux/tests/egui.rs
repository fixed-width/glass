//! Live AccessKit text-write refusal and keyboard recovery. Build glass-fixture-egui first.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, AxNode, AxRole, AxTree, Backend, BaselineStore, BoundDispatch, Glass, GlassError,
    KeyEvent, SandboxLevel,
};

fn find_role(node: &AxNode, role: AxRole) -> Option<&AxNode> {
    if node.role == role {
        Some(node)
    } else {
        node.children
            .iter()
            .find_map(|child| find_role(child, role))
    }
}

fn wait_for_value(glass: &mut Glass, role: AxRole, expected: &str) -> AxTree {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tree = glass.a11y_snapshot(None);
        if let Ok(tree) = &tree
            && find_role(&tree.root, role)
                .is_some_and(|node| node.value.as_deref().unwrap_or("") == expected)
        {
            return tree.clone();
        }
        assert!(Instant::now() < deadline, "value {expected:?}: {tree:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "needs Xvfb, AT-SPI and a built glass-fixture-egui; optionally set GLASS_EGUI_FIXTURE"]
fn missing_editable_text_refuses_before_dispatch_and_keyboard_recovery_works() {
    let fixture = std::env::var_os("GLASS_EGUI_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../glass-fixture-egui/target/release/glass-fixture-egui")
        });
    assert!(fixture.is_file(), "build the egui fixture: {fixture:?}");
    let dir = tempfile::tempdir().unwrap();
    let mut glass = Glass::new(
        Box::new(|_| {
            Ok(Backend {
                platform: Box::new(glass_x11::X11Platform::from_env()?),
                accessibility: Some(Box::new(glass_a11y_linux::LinuxA11y::new())),
            })
        }),
        "x11".into(),
        BaselineStore::new(dir.path().join("baselines")),
        100,
    );
    glass
        .start(&AppSpec {
            build: None,
            run: vec![fixture.to_string_lossy().into_owned()],
            cwd: None,
            env: vec![("LIBGL_ALWAYS_SOFTWARE".into(), "1".into())],
            window_hint: None,
            timeout_ms: 10_000,
            sandbox: SandboxLevel::Off,
            a11y: true,
        })
        .expect("launch egui");
    let tree = wait_for_value(&mut glass, AxRole::TextField, "");
    let field = find_role(&tree.root, AxRole::TextField).unwrap();
    assert!(field.states.editable);

    let error = glass.set_value(field.id, "recovered").unwrap_err();
    assert!(
        matches!(error.cause(), GlassError::AxElementNotEditable(id) if *id == field.id.0),
        "{error:?}"
    );
    assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    assert!(!error.set_value_failed_after_writing());
    let tree = wait_for_value(&mut glass, AxRole::TextField, "");

    let field = find_role(&tree.root, AxRole::TextField).unwrap();
    glass.click_element(field.id).expect("focus text field");
    glass
        .key(&KeyEvent::Text("recovered".into()))
        .expect("type");
    let tree = wait_for_value(&mut glass, AxRole::TextField, "recovered");

    let number = find_role(&tree.root, AxRole::SpinButton).unwrap();
    glass.set_value(number.id, "73").expect("numeric write");
    wait_for_value(&mut glass, AxRole::SpinButton, "73");
    glass.stop().expect("stop egui");
}
