//! Live a11y-service round-trip. Ignored; run with a booted AVD + the built APKs (both come from
//! the glass-android-agent repo: `./gradlew :a11y:assembleDebug :fixture-compose:assembleDebug`):
//!   GLASS_ADB=/path/to/platform-tools/adb \
//!   GLASS_ANDROID_A11Y_APK=/path/to/a11y-debug.apk \
//!   GLASS_ANDROID_FIXTURE_APK=/path/to/fixture-compose-debug.apk \
//!     cargo test -p glass-android --test a11y_service_loop -- --ignored --nocapture

use std::sync::Mutex;

use glass_android::{
    A11yServiceRegistry, AgentRegistry, AndroidPlatform, EmulatorRegistry, ServiceA11y,
};
use glass_core::accessibility::{
    Accessibility, AxContext, AxDeadline, AxNode, AxTarget, WalkLimits,
};
use glass_core::{AppSpec, Platform, SandboxLevel, WindowGeometry};

mod common;

/// Serialize the two tests below: the on-device accessibility service accepts one connection
/// at a time, and both tests drive whichever activity is currently foreground, so running them
/// concurrently makes each retarget the other's window. Poison-tolerant so a panicking test
/// does not wedge the other.
static DEVICE: Mutex<()> = Mutex::new(());

#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK"]
fn a11y_service_snapshot_and_actions() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let apk = std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    // Resolve the device the same way production does, and reuse its serial-bound adb (the
    // `resolved_adb()` accessor) — no bespoke test helper. Launch Settings for an active window.
    let agents = AgentRegistry::new();
    let _stop_agent = common::StopAgent(&agents);
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
    let _restore = common::RestoreServiceState(&reg);
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
        limits: WalkLimits::DEFAULT,
        deadline: AxDeadline::UNBOUNDED,
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
            value: node.value.clone(),
        };
        a11y.set_value(&ctx, &target, "viaA11y")
            .expect("set_value via ACTION_SET_TEXT");
    }

    p.stop_app().ok();
    // The guards above restore the service settings and the forwards as they drop, and
    // scripts/test-android.sh re-reads the device after the suite to confirm they did.
}

/// Native invoke against the fixture. Ignored; same environment as the test above.
#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK + GLASS_ANDROID_FIXTURE_APK"]
fn native_invoke_actuates_the_fixture() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let apk = std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    let fixture =
        std::env::var("GLASS_ANDROID_FIXTURE_APK").expect("set GLASS_ANDROID_FIXTURE_APK");

    let agents = AgentRegistry::new();
    let _stop_agent = common::StopAgent(&agents);
    let mut p = AndroidPlatform::from_env(&EmulatorRegistry::new(), &agents).expect("attach");
    let adb = p.resolved_adb();
    adb.run(["install", "-r", "-g", &fixture])
        .expect("install fixture");

    let spec = |activity: &str| AppSpec {
        build: None,
        run: vec![format!("com.fixedwidth.glassfixture/.{activity}")],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 10_000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    };
    p.start_app(&spec("InvokeViewFixtureActivity"))
        .expect("launch view fixture");

    let reg = A11yServiceRegistry::new();
    let _restore = common::RestoreServiceState(&reg);
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
        limits: WalkLimits::DEFAULT,
        deadline: AxDeadline::UNBOUNDED,
    };

    fn by_desc<'a>(n: &'a AxNode, desc: &str) -> Option<&'a AxNode> {
        if n.description.as_deref() == Some(desc) || n.name.as_deref() == Some(desc) {
            return Some(n);
        }
        n.children.iter().find_map(|c| by_desc(c, desc))
    }
    fn target(n: &AxNode) -> AxTarget {
        AxTarget {
            id: n.id,
            role: n.role,
            name: n.name.clone(),
            bounds: n.bounds,
            value: n.value.clone(),
        }
    }
    let snap = |a: &mut ServiceA11y| {
        let mut t = a.snapshot(&ctx).expect("snapshot");
        t.assign_ids();
        t
    };
    let counter = |a: &mut ServiceA11y| {
        by_desc(&snap(a).root, "Counter")
            .and_then(|n| n.name.clone().or_else(|| n.value.clone()))
            .expect("counter is present")
    };

    // Poll `read` until it changes instead of sleeping once — `invoke` only waits internally
    // for a checkable target (`wait_for_flip`), so a plain click's effect on the tree is
    // awaited here instead. A timeout is a real failure, not a silent pass.
    fn await_change(
        what: &str,
        before: &str,
        deadline: std::time::Duration,
        mut read: impl FnMut() -> String,
    ) -> String {
        let start = std::time::Instant::now();
        loop {
            let now = read();
            if now != before {
                return now;
            }
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                panic!(
                    "timed out after {elapsed:?} waiting for {what}; still reads {now:?} \
                     (started at {before:?})"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    // Generous is free: the poll returns as soon as the value changes.
    const AWAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

    // An enabled classic button actuates.
    let before = counter(&mut a11y);
    let t = snap(&mut a11y);
    let save = target(by_desc(&t.root, "SaveBtn").expect("SaveBtn"));
    a11y.invoke(&ctx, &save)
        .expect("native invoke on an enabled button");
    await_change(
        "the enabled button's click to register",
        &before,
        AWAIT_DEADLINE,
        || counter(&mut a11y),
    );

    // A row far below the fold actuates — the case a pointer tap cannot serve.
    let t = snap(&mut a11y);
    let row_node = by_desc(&t.root, "Row250").expect("Row250");
    let b = row_node.bounds.expect("the row reports bounds");
    // If the list shrank or the screen grew, this leg would silently become a second copy of
    // the one above and still pass.
    let window_h = i32::try_from(ctx.window.height).expect("window height fits an i32");
    let row_h = i32::try_from(b.height).expect("row height fits an i32");
    assert!(
        b.y >= window_h || b.y + row_h <= 0,
        "Row250 must lie outside the window for this leg to be about a control a tap cannot \
         reach; bounds {b:?}, window height {window_h}"
    );
    let row = target(row_node);
    let before = counter(&mut a11y);
    a11y.invoke(&ctx, &row).expect("native invoke off-screen");
    let after = await_change(
        "the off-screen row's click to register",
        &before,
        AWAIT_DEADLINE,
        || counter(&mut a11y),
    );
    assert!(
        after.contains("row=250"),
        "the RIGHT row must actuate, got {after}"
    );

    // A disabled button errors, and says why.
    let t = snap(&mut a11y);
    let gated = target(by_desc(&t.root, "GatedBtn").expect("GatedBtn"));
    let err = a11y
        .invoke(&ctx, &gated)
        .expect_err("a disabled button must not report success");
    assert!(err.to_string().contains("disabled"), "{err}");
    assert!(!err.invoke_fallback_eligible(), "must not fall back: {err}");

    // A checkbox flips, and the flip is observed rather than assumed.
    let t = snap(&mut a11y);
    let box_node = by_desc(&t.root, "AgreeBox").expect("AgreeBox");
    let was = box_node.states.checked;
    let agree = target(box_node);
    a11y.invoke(&ctx, &agree)
        .expect("native invoke on a checkbox");
    let t = snap(&mut a11y);
    assert_ne!(
        by_desc(&t.root, "AgreeBox")
            .expect("AgreeBox")
            .states
            .checked,
        was,
        "the checkbox must actually toggle"
    );

    // Compose: the label is not the clickable node, so this exercises the climb.
    p.start_app(&spec("InvokeComposeFixtureActivity"))
        .expect("launch compose fixture");
    // The platform reports the app up before the Compose hierarchy has published its
    // accessibility tree, and the label's bounds keep moving while the activity animates in —
    // `invoke` fingerprints bounds, so waiting for the label to merely exist yields
    // `AxElementChanged`.
    let deadline = std::time::Instant::now() + AWAIT_DEADLINE;
    let mut settled = None;
    loop {
        let bounds = by_desc(&snap(&mut a11y).root, "Save").and_then(|n| n.bounds);
        if bounds.is_some() && bounds == settled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the compose fixture never settled on a Save label"
        );
        settled = bounds;
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    let before = counter(&mut a11y);
    let t = snap(&mut a11y);
    let save_label = by_desc(&t.root, "Save").expect("Save label");
    let actuated = a11y
        .invoke(&ctx, &target(save_label))
        .expect("native invoke via the ancestor climb")
        .expect("the climb actuated another node, and the result must name it");
    assert_ne!(
        actuated, save_label.id,
        "the disclosed node is the one that handled the click, not the label asked for"
    );
    await_change(
        "the climb's click to reach a clickable node",
        &before,
        AWAIT_DEADLINE,
        || counter(&mut a11y),
    );

    p.stop_app().ok();
    drop(p);
    // The fixture is glass's to install and glass's to remove: leaving it installed lets device
    // state accumulate across runs.
    adb.run(["uninstall", "com.fixedwidth.glassfixture"]).ok();
}
