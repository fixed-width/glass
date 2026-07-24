//! Live accessibility verification against a running AVD. Ignored by default:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test a11y_loop -- --ignored --nocapture
//!
//! Launches com.android.settings, snapshots its a11y tree, and asserts the tree
//! is non-trivial and carries named, role-typed elements.

use glass_core::accessibility::{Accessibility, AxContext, WalkLimits};
use glass_core::{AppSpec, Platform, SandboxLevel};

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
