//! The Android click path as a user reaches it: a session whose backend comes from the real
//! `make_platform` factory, so the reader it selects is the one under test. Ignored; run with a
//! booted AVD + the built APKs (both come from the glass-android-agent repo:
//! `./gradlew :a11y:assembleDebug :fixture-compose:assembleDebug`):
//!   GLASS_ADB=/path/to/platform-tools/adb \
//!   GLASS_ANDROID_A11Y_APK=/path/to/a11y-debug.apk \
//!   GLASS_ANDROID_FIXTURE_APK=/path/to/fixture-compose-debug.apk \
//!     cargo test -p glass-mcp --test android_session_loop -- --ignored --nocapture

use glass_android::{A11yServiceRegistry, AgentRegistry, AndroidPlatform, EmulatorRegistry};
use glass_core::accessibility::{AxNode, AxTree, ClickMethod};
use glass_core::{AppSpec, BaselineStore, Glass, PlatformFactory, SandboxLevel};

/// Ceiling on the wait for the fixture's counter to reflect the click — the poll returns as soon
/// as it changes.
const AWAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
const AWAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// glass#287: the glass-android device tests build `ServiceA11y` by hand, so none of them can see
/// which reader a session would have picked — and picking the uiautomator one makes every click a
/// pointer tap that still reports `Ok`.
#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK + GLASS_ANDROID_FIXTURE_APK"]
fn a_session_click_reports_the_native_accessibility_action() {
    // An unset APK path is itself the degradation this test detects, so it has to fail as
    // misconfiguration rather than as a verdict.
    std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    let fixture =
        std::env::var("GLASS_ANDROID_FIXTURE_APK").expect("set GLASS_ANDROID_FIXTURE_APK");

    // Under the default attach-or-boot lifecycle a second `EmulatorRegistry` with no device
    // attached boots a second emulator, so the install below and the factory share one.
    let registry = EmulatorRegistry::new();
    // `Arc` only so the teardown below can still reach them: the factory closure takes what it
    // captures. Both leave state on the *device* that outliving the process does not clear —
    // an `adb forward`, and the companion installed and enabled as an accessibility service —
    // and neither registry has a `Drop`, so only `shutdown` takes it back off.
    let agents = std::sync::Arc::new(AgentRegistry::new());
    let a11y = std::sync::Arc::new(A11yServiceRegistry::new());

    {
        let p = AndroidPlatform::from_env(&registry, &agents).expect("attach to a device");
        p.resolved_adb()
            .run(["install", "-r", "-g", &fixture])
            .expect("install the fixture APK");
    }

    // The two shapes of `boot`'s own factory closure — `make_platform` takes the iOS Simulator
    // registry on macOS only.
    #[cfg(target_os = "macos")]
    let sim = glass_ios::SimulatorRegistry::new();
    #[cfg(target_os = "macos")]
    let factory: PlatformFactory = {
        let (agents, a11y) = (agents.clone(), a11y.clone());
        Box::new(move |b| glass_mcp::make_platform(b, &registry, &agents, &a11y, &sim))
    };
    #[cfg(not(target_os = "macos"))]
    let factory: PlatformFactory = {
        let (agents, a11y) = (agents.clone(), a11y.clone());
        Box::new(move |b| glass_mcp::make_platform(b, &registry, &agents, &a11y))
    };

    let baselines = tempfile::tempdir().expect("a temp dir for the baseline store");
    let mut glass = Glass::new(
        factory,
        "android".to_string(),
        BaselineStore::new(baselines.path()),
        10_000,
    );
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["com.fixedwidth.glassfixture/.InvokeViewFixtureActivity".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 10_000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .expect("launch the view fixture");

    let asserted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tree = glass.a11y_snapshot(None).expect("snapshot");
        let before = counter(&tree);
        let save = find(&tree.root, "SaveBtn").expect("the fixture's SaveBtn is present");
        let method = glass.click_element(save.id).expect("click SaveBtn");

        // `Pointer`'s `native_fallback` names the error that sent the click down the synthetic
        // path.
        assert!(
            matches!(method, ClickMethod::NativeAction { .. }),
            "a session click on an enabled button must fire the native action, got {method:?}"
        );

        // `NativeAction` is the session's claim about which path it took; the counter is the
        // fixture's report that something actuated.
        await_change("SaveBtn's click to register", &before, || {
            counter(&glass.a11y_snapshot(None).expect("snapshot"))
        });
    }));

    // Caught rather than left to unwind, because the device outlives this process and every
    // line below is what hands it back clean (glass#419). The session alone would survive a
    // panic — `AndroidPlatform` force-stops on drop — but the registries have no `Drop`, so an
    // unwind past them leaves the companion enabled for every later reader on this device.
    let stopped = glass.stop();
    agents.shutdown();
    a11y.shutdown();

    // The assertion's own failure first: a teardown error must not displace what it explains.
    if let Err(payload) = asserted {
        std::panic::resume_unwind(payload);
    }
    stopped.expect("stop the session");
}

/// The fixture's click counter, as the a11y tree reports it.
fn counter(tree: &AxTree) -> String {
    find(&tree.root, "Counter")
        .and_then(|n| n.name.clone().or_else(|| n.value.clone()))
        .expect("the fixture's Counter is present")
}

/// The first node in `n`'s subtree whose description or name is `label`.
fn find<'a>(n: &'a AxNode, label: &str) -> Option<&'a AxNode> {
    if n.description.as_deref() == Some(label) || n.name.as_deref() == Some(label) {
        return Some(n);
    }
    n.children.iter().find_map(|c| find(c, label))
}

/// Poll `read` until it differs from `before`. A timeout is a real failure, not a silent pass.
fn await_change(what: &str, before: &str, mut read: impl FnMut() -> String) {
    let start = std::time::Instant::now();
    loop {
        let now = read();
        if now != before {
            return;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < AWAIT_DEADLINE,
            "timed out after {elapsed:?} waiting for {what}; still reads {now:?} \
             (started at {before:?})"
        );
        std::thread::sleep(AWAIT_INTERVAL);
    }
}
