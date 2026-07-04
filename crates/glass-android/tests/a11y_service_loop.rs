//! Live a11y-service round-trip. Ignored; run with a booted AVD + the built APK:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!   GLASS_ANDROID_A11Y_APK=$HOME/git-sources/fw/glass-android-agent/a11y/build/outputs/apk/debug/a11y-debug.apk \
//!     cargo test -p glass-android --test a11y_service_loop -- --ignored --nocapture

use glass_android::{
    A11yServiceRegistry, AgentRegistry, AndroidPlatform, EmulatorRegistry, ServiceA11y,
};
use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxTarget};
use glass_core::{AppSpec, Platform, SandboxLevel, WindowGeometry};

#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK"]
fn a11y_service_snapshot_and_actions() {
    let apk = std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    // Resolve the device the same way production does, and reuse its serial-bound adb (the
    // `resolved_adb()` accessor) — no bespoke test helper. Launch Settings for an active window.
    let agents = AgentRegistry::new();
    let mut p = AndroidPlatform::from_env(&EmulatorRegistry::new(), &agents).expect("attach");
    let spec = AppSpec {
        build: None,
        run: vec!["com.android.settings/.Settings".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 10_000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec).expect("launch settings");
    let adb = p.resolved_adb();

    let reg = A11yServiceRegistry::new();
    let client = reg.ensure(&adb, &apk).expect("install + enable + connect");
    let mut a11y = ServiceA11y::new(client, String::new());
    let ctx = AxContext {
        pids: vec![],
        window: WindowGeometry {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        },
        window_handle: None,
        a11y_bus_addr: None,
    };

    let mut tree = a11y.snapshot(&ctx).expect("snapshot");
    tree.assign_ids();
    assert!(
        tree.count > 1,
        "expected a non-trivial a11y tree, got {}",
        tree.count
    );

    // If the active window has an editable field, set it via the service (ACTION_SET_TEXT — the
    // reliable high-fidelity action). Settings' top screen has none, so this is best-effort; point
    // the test at the :fixture-compose app (which has a Name EditText) to exercise it for real.
    fn first_editable(n: &AxNode) -> Option<&AxNode> {
        if n.states.editable {
            return Some(n);
        }
        n.children.iter().find_map(first_editable)
    }
    if let Some(node) = first_editable(&tree.root) {
        let target = AxTarget {
            id: node.id,
            role: node.role,
            name: node.name.clone(),
            bounds: node.bounds,
        };
        a11y.set_value(&ctx, &target, "viaA11y")
            .expect("set_value via ACTION_SET_TEXT");
    }

    reg.shutdown(); // restores enabled_accessibility_services + removes the forward
    p.stop_app().ok();
    drop(p);
    agents.shutdown(); // tear down any agent the platform launched (no leaks)

    // Then the controller condition-polls the device to confirm teardown left no enabled service
    // and no forward (don't snapshot immediately — the unbind is async, ~1s; mirror the agent_loop leak check).
}
