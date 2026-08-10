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

/// The registries whose `ensure` switches something on for the whole device, put back when this
/// goes out of scope — a panic unwinds through it, where a trailing `shutdown()` is skipped.
///
/// A guard rather than a block around the assertions, because the state is already on by then:
/// `Glass::start` runs the factory before `start_app`, and the factory enables the companion. A
/// launch that fails — the likeliest failure here — panics with it enabled, and only `shutdown`
/// restores the secure settings and removes the `adb forward` each registry opened. The APK
/// itself is left installed, as glass owns that package (glass#419).
struct Companions {
    agents: AgentRegistry,
    a11y: A11yServiceRegistry,
    emulators: EmulatorRegistry,
}

impl Drop for Companions {
    fn drop(&mut self) {
        // Reached while a panic unwinds, where a second panic aborts the process — nothing here
        // may assert.
        self.agents.shutdown();
        self.a11y.shutdown();
        // A no-op unless glass booted the emulator rather than attaching to one.
        self.emulators.kill_all();
    }
}

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

    // First, so it drops last: the app has to be force-stopped before the companion reading it
    // goes away. Under the default attach-or-boot lifecycle a second `EmulatorRegistry` with no
    // device attached boots a second emulator, so the install below and the factory share one.
    let device = Companions {
        agents: AgentRegistry::new(),
        a11y: A11yServiceRegistry::new(),
        emulators: EmulatorRegistry::new(),
    };

    {
        let p = AndroidPlatform::from_env(&device.emulators, &device.agents)
            .expect("attach to a device");
        p.resolved_adb()
            .run(["install", "-r", "-g", &fixture])
            .expect("install the fixture APK");
    }

    // The two shapes of `boot`'s own factory closure — `make_platform` takes the iOS Simulator
    // registry on macOS only. Each registry is a handle to shared state, so the clones the
    // closure moves are the same registries `device` shuts down.
    #[cfg(target_os = "macos")]
    let sim = glass_ios::SimulatorRegistry::new();
    #[cfg(target_os = "macos")]
    let factory: PlatformFactory = {
        let (emulators, agents, a11y) = (
            device.emulators.clone(),
            device.agents.clone(),
            device.a11y.clone(),
        );
        Box::new(move |b| glass_mcp::make_platform(b, &emulators, &agents, &a11y, &sim))
    };
    #[cfg(not(target_os = "macos"))]
    let factory: PlatformFactory = {
        let (emulators, agents, a11y) = (
            device.emulators.clone(),
            device.agents.clone(),
            device.a11y.clone(),
        );
        Box::new(move |b| glass_mcp::make_platform(b, &emulators, &agents, &a11y))
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

    let tree = glass.a11y_snapshot(None).expect("snapshot");
    let before = counter(&tree);
    let save = find(&tree.root, "SaveBtn").expect("the fixture's SaveBtn is present");
    let method = glass.click_element(save.id).expect("click SaveBtn");

    // `Pointer`'s `native_fallback` names the error that sent the click down the synthetic path.
    assert!(
        matches!(method, ClickMethod::NativeAction { .. }),
        "a session click on an enabled button must fire the native action, got {method:?}"
    );

    // `NativeAction` is the session's claim about which path it took; the counter is the
    // fixture's report that something actuated.
    await_change("SaveBtn's click to register", &before, || {
        counter(&glass.a11y_snapshot(None).expect("snapshot"))
    });

    // The path a user takes. An assertion above panics past it, and the drops then do the same
    // work: `AndroidPlatform` force-stops the app, `Companions` puts the device settings back.
    glass.stop().expect("stop the session");
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
