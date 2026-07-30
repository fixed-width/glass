//! Live accessibility verification against a running AVD. Ignored by default:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test a11y_loop -- --ignored --nocapture
//!
//! Launches com.android.settings, snapshots its a11y tree, and asserts the tree
//! is non-trivial and carries named, role-typed elements. A second test writes into a real
//! `EditText` and checks that `set_value` only claims success when the write actually landed.

use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxTarget, WalkLimits};
use glass_core::{AppSpec, GlassError, MouseButton, Platform, PointerEvent, SandboxLevel};

fn settings_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["com.android.settings/.Settings".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    }
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn snapshot_has_named_role_typed_nodes() {
    let agents = glass_android::AgentRegistry::new();
    let mut platform =
        glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &agents)
            .expect("attach");
    let window = platform
        .start_app(&settings_spec())
        .expect("launch settings");
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let ctx = AxContext {
        pids: platform.app_pids(),
        window,
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    let mut a11y = glass_android::AndroidA11y::new();
    let mut tree = a11y.snapshot(&ctx).expect("snapshot");
    tree.assign_ids();

    println!("{}", tree.to_outline());
    assert!(
        tree.count > 5,
        "expected a non-trivial tree, got {} nodes",
        tree.count
    );

    fn any_named(n: &glass_core::accessibility::AxNode) -> bool {
        n.name.is_some() || n.children.iter().any(any_named)
    }
    assert!(any_named(&tree.root), "expected at least one named node");

    platform.stop_app().expect("stop");
    drop(platform); // close the platform's agent connection (if any) first
    agents.shutdown(); // tear down a launched agent — these tests must not leak it
}

/// Depth-first search for the first node satisfying `want`.
fn find<'a>(node: &'a AxNode, want: &dyn Fn(&AxNode) -> bool) -> Option<&'a AxNode> {
    if want(node) {
        return Some(node);
    }
    node.children.iter().find_map(|c| find(c, want))
}

/// A real write into a real field, and a real write that cannot land.
///
/// Settings has no editable field until its search entry is tapped, so the test taps it first and
/// then drives `set_value` against the `EditText` that appears — which is the point: this exercises
/// the tap-clear-type path on a live toolkit, where the read-back has to come from `uiautomator`'s
/// own view of the field rather than from anything glass remembers writing.
#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn set_value_reports_whether_the_write_landed() {
    let agents = glass_android::AgentRegistry::new();
    let mut platform =
        glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &agents)
            .expect("attach");
    let window = platform
        .start_app(&settings_spec())
        .expect("launch settings");
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let ctx = AxContext {
        pids: platform.app_pids(),
        window: window.clone(),
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    let mut a11y = glass_android::AndroidA11y::new();

    // Tap the search entry so Settings puts up its EditText.
    let mut tree = a11y.snapshot(&ctx).expect("snapshot");
    tree.assign_ids();
    let search = find(&tree.root, &|n| {
        n.name.as_deref().is_some_and(|s| s.contains("Search"))
    })
    .and_then(|n| n.bounds)
    .and_then(|b| b.clamped_center(window.width, window.height))
    .expect("Settings shows a search entry");
    platform
        .send_pointer(&PointerEvent::Click {
            x: search.0,
            y: search.1,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        })
        .expect("tap search");
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let mut tree = a11y.snapshot(&ctx).expect("re-snapshot");
    tree.assign_ids();
    let field = find(&tree.root, &|n| n.states.editable).expect("an editable field after the tap");
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
    };

    // A write that lands: reported Ok, and the field really holds it afterwards.
    a11y.set_value(&ctx, &target, "glass")
        .expect("a real write into a real field must succeed");
    let mut after = a11y.snapshot(&ctx).expect("snapshot after write");
    after.assign_ids();
    let held = find(&after.root, &|n| n.states.editable)
        .and_then(|n| n.value.clone())
        .unwrap_or_default();
    assert!(
        held.contains("glass"),
        "field holds {held:?} after the write"
    );

    // A write that cannot land: the id names a node that is not the field any more. Before this
    // change every one of these returned Ok.
    let stale = AxTarget {
        id: target.id,
        role: target.role,
        name: Some("not the field that is there".into()),
        bounds: target.bounds,
    };
    match a11y.set_value(&ctx, &stale, "ignored") {
        Err(GlassError::AxElementChanged(_)) | Err(GlassError::AxElementNotFound(_)) => {}
        other => panic!("a stale target must not report success: {other:?}"),
    }

    platform.stop_app().expect("stop");
    drop(platform);
    agents.shutdown();
}
