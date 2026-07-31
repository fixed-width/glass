//! Live accessibility verification against a running AVD. Ignored by default:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test a11y_loop -- --ignored --nocapture --test-threads=1
//!
//! `--test-threads=1` is required, not tidiness: both tests drive Settings on the one attached
//! device, so in parallel each tears down the app the other is mid-interaction with.
//!
//! Launches com.android.settings, snapshots its a11y tree, and asserts the tree
//! is non-trivial and carries named, role-typed elements. A second test writes into a real
//! `EditText` and checks that a landed write is confirmed against the live field — and that a
//! clear cannot be confirmed here at all, because this platform reports the emptied field's hint
//! as its text.

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

/// A real write into a real field, and a real clear of it.
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
        value: field.value.clone(),
    };

    // A write that lands: reported Ok, and the node at that id holds it afterwards.
    a11y.set_value(&ctx, &target, "glass")
        .expect("a real write into a real field must succeed");
    let mut after = a11y.snapshot(&ctx).expect("snapshot after write");
    after.assign_ids();
    let held = after
        .find(target.id)
        .and_then(|n| n.value.clone())
        .unwrap_or_default();
    assert!(
        held.contains("glass"),
        "field holds {held:?} after the write"
    );

    // Clearing the field, on the device whose behaviour decided the rule: an emptied Android
    // `EditText` reports its *hint* as its text and `uiautomator` exposes no hint attribute, so a
    // clear cannot be confirmed here at all — the deliberate cost of judging one by "reads back
    // empty". What the device still has to show is that the text went.
    //
    // The target is re-located first: typing into Settings' search renders a results list, so the
    // screen the pre-write target was captured against is gone.
    let mut before_clear = a11y.snapshot(&ctx).expect("snapshot before the clear");
    before_clear.assign_ids();
    let field = find(&before_clear.root, &|n| n.states.editable).expect("the field is still there");
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };
    match a11y.set_value(&ctx, &target, "") {
        Err(GlassError::AxValueNotApplied(_)) => {}
        other => panic!("a field that reports its hint cannot confirm a clear: {other:?}"),
    }
    let mut cleared = a11y.snapshot(&ctx).expect("snapshot after clear");
    cleared.assign_ids();
    // Found by what it is, not by an id the clear may have shifted, so this cannot pass by failing
    // to look.
    let field = find(&cleared.root, &|n| n.states.editable)
        .expect("the field is still there after the clear");
    assert_ne!(
        field.value.as_deref(),
        Some("glass"),
        "the typed text is gone from the field"
    );

    // A stale target is refused before anything is typed — the pre-write fingerprint check, which
    // predates the read-back. Kept as a regression guard, not as evidence about the read-back.
    let stale = AxTarget {
        id: target.id,
        role: target.role,
        name: Some("not the field that is there".into()),
        bounds: target.bounds,
        value: target.value.clone(),
    };
    match a11y.set_value(&ctx, &stale, "ignored") {
        Err(GlassError::AxElementChanged(_)) | Err(GlassError::AxElementNotFound(_)) => {}
        other => panic!("a stale target must not report success: {other:?}"),
    }

    platform.stop_app().expect("stop");
    drop(platform);
    agents.shutdown();
}
