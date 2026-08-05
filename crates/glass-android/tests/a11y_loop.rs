//! Live accessibility verification against a running AVD. Ignored by default:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test a11y_loop -- --ignored --nocapture --test-threads=1
//!
//! `--test-threads=1` is required, not tidiness: these tests drive Settings on the one attached
//! device, so in parallel each tears down the app the other is mid-interaction with.
//!
//! They cover a non-trivial tree with named, role-typed elements; a write confirmed against the
//! live field — and a clear that cannot be confirmed here at all, because this platform reports
//! the emptied field's hint as its text; that a read takes its dump file with it; and that a
//! snapshot taken while another app is foreground says which app it describes.

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

/// The dump files on the device, as `adb shell ls` reports them.
///
/// A device is shared, so what a test can assert is that reading left the listing as it found it.
fn dump_files() -> String {
    let adb = std::env::var("GLASS_ADB").unwrap_or_else(|_| "adb".to_string());
    let out = std::process::Command::new(adb)
        .args(["shell", "ls", "/sdcard/glass_dump*"])
        .output()
        .expect("adb shell ls");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The only test that can tell a read's housekeeping from a no-op: the unit tests model the device
/// filesystem, and a model cannot disagree with itself.
#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn a_snapshot_leaves_no_dump_file_on_the_device() {
    let agents = glass_android::AgentRegistry::new();
    let mut platform =
        glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &agents)
            .expect("attach");
    let window = platform
        .start_app(&settings_spec())
        .expect("launch settings");
    let before = dump_files();

    let ctx = AxContext {
        pids: platform.app_pids(),
        window,
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    let mut a11y = glass_android::AndroidA11y::new();
    a11y.snapshot(&ctx).expect("first snapshot");
    a11y.snapshot(&ctx).expect("second snapshot");

    assert_eq!(
        dump_files(),
        before,
        "each read must take its own file with it"
    );

    platform.stop_app().expect("stop");
    drop(platform);
    agents.shutdown();
}

/// Depth-first search for the first node satisfying `want`.
fn find<'a>(node: &'a AxNode, want: &dyn Fn(&AxNode) -> bool) -> Option<&'a AxNode> {
    if want(node) {
        return Some(node);
    }
    node.children.iter().find_map(|c| find(c, want))
}

/// An attempt that saw nothing which is evidence about `set_value` — the search screen went away
/// under it, or the device could not be read — carrying the step that noticed. Retried rather
/// than asserted on.
struct Abandoned(String);

/// How long [`set_value_reports_whether_the_write_landed`] may spend getting Settings' search
/// screen to stay up, and the pause between attempts at it.
///
/// A device's FIRST boot after a data wipe takes the search screen away twice over, neither of
/// them anything glass did (glass#323), and both settle within the first half-minute:
///
///   - `com.google.android.settings.intelligence` owns the search screen and is a different
///     package from the `com.android.settings` this test launches. It takes a Phenotype config
///     commit ~10s after `boot_completed` and the platform force-stops it — `Killing …(adj 0):
///     change com.google.android.settings.intelligence`, then `Force removing
///     ActivityRecord{…SearchActivity}: app died, no saved state`.
///   - Until that package is serving, Settings' home screen draws no search entry to tap.
///
/// Do not delete this as defensive noise: without it the test is 12/12 red on wiped cold boots
/// (0/22 warm).
const SETUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
const SETUP_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether Settings still has its search screen up — the premise every assertion in the write leg
/// rests on. An editable field is what this test drives, and Settings' home screen has none.
fn search_field_is_up(a11y: &mut glass_android::AndroidA11y, ctx: &AxContext) -> bool {
    match a11y.snapshot(ctx) {
        Ok(mut t) => {
            t.assign_ids();
            find(&t.root, &|n| n.states.editable).is_some()
        }
        Err(_) => false,
    }
}

/// A step of the write leg did not do what it must. Panics when the field is still on screen,
/// and abandons the attempt only when the screen the step was judging has gone. A guard bug
/// refuses with the field still there, so it can never be retried away.
fn abandon_or_fail(
    a11y: &mut glass_android::AndroidA11y,
    ctx: &AxContext,
    what: String,
) -> Abandoned {
    assert!(!search_field_is_up(a11y, ctx), "{what}");
    Abandoned(what)
}

/// One attempt at the write leg, opening the search screen it needs for itself.
fn write_leg(
    platform: &mut glass_android::AndroidPlatform,
    a11y: &mut glass_android::AndroidA11y,
    ctx: &AxContext,
) -> Result<(), Abandoned> {
    // Tap the search entry so Settings puts up its EditText.
    let mut tree = a11y
        .snapshot(ctx)
        .map_err(|e| Abandoned(format!("snapshot: {e}")))?;
    tree.assign_ids();
    let search = find(&tree.root, &|n| {
        n.name.as_deref().is_some_and(|s| s.contains("Search"))
    })
    .and_then(|n| n.bounds)
    .and_then(|b| b.clamped_center(ctx.window.width, ctx.window.height))
    .ok_or_else(|| Abandoned("Settings shows no search entry".into()))?;
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

    let mut tree = a11y
        .snapshot(ctx)
        .map_err(|e| Abandoned(format!("re-snapshot: {e}")))?;
    tree.assign_ids();
    let field = find(&tree.root, &|n| n.states.editable)
        .ok_or_else(|| Abandoned("no editable field after the tap".into()))?;
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };

    // A write that lands: reported Ok, and the node at that id holds it afterwards.
    if let Err(e) = a11y.set_value(ctx, &target, "glass") {
        return Err(abandon_or_fail(
            a11y,
            ctx,
            format!("a real write into a real field must succeed: {e}"),
        ));
    }
    let mut after = a11y
        .snapshot(ctx)
        .map_err(|e| Abandoned(format!("snapshot after write: {e}")))?;
    after.assign_ids();
    let held = after
        .find(target.id)
        .and_then(|n| n.value.clone())
        .unwrap_or_default();
    if !held.contains("glass") {
        return Err(abandon_or_fail(
            a11y,
            ctx,
            format!("field holds {held:?} after the write"),
        ));
    }

    // Clearing the field, on the device whose behaviour decided the rule: an emptied Android
    // `EditText` reports its *hint* as its text and `uiautomator` exposes no hint attribute, so a
    // clear cannot be confirmed here at all — the deliberate cost of judging one by "reads back
    // empty". What the device still has to show is that the text went.
    //
    // The target is re-located first: typing into Settings' search renders a results list, so the
    // screen the pre-write target was captured against is gone.
    let mut before_clear = a11y
        .snapshot(ctx)
        .map_err(|e| Abandoned(format!("snapshot before the clear: {e}")))?;
    before_clear.assign_ids();
    let field = find(&before_clear.root, &|n| n.states.editable)
        .ok_or_else(|| Abandoned("the field went before the clear".into()))?;
    let target = AxTarget {
        id: field.id,
        role: field.role,
        name: field.name.clone(),
        bounds: field.bounds,
        value: field.value.clone(),
    };
    let verdict = a11y.set_value(ctx, &target, "");
    if !matches!(verdict, Err(GlassError::AxValueNotApplied(_))) {
        return Err(abandon_or_fail(
            a11y,
            ctx,
            format!("a field that reports its hint cannot confirm a clear: {verdict:?}"),
        ));
    }
    let mut cleared = a11y
        .snapshot(ctx)
        .map_err(|e| Abandoned(format!("snapshot after clear: {e}")))?;
    cleared.assign_ids();
    // Found by what it is, not by an id the clear may have shifted, so this cannot pass by failing
    // to look.
    let field = find(&cleared.root, &|n| n.states.editable)
        .ok_or_else(|| Abandoned("the field went during the clear".into()))?;
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
    // Any of the three refusals: nothing on this screen carries that name, so the usual verdict
    // is `AxElementGone`, but a tree that renumbered or lost the id answers first.
    match a11y.set_value(ctx, &stale, "ignored") {
        Err(
            GlassError::AxElementGone(_)
            | GlassError::AxElementChanged(_)
            | GlassError::AxElementNotFound(_),
        ) => {}
        other => panic!("a stale target must not report success: {other:?}"),
    }
    Ok(())
}

/// A real write into a real field, and a real clear of it.
///
/// Settings has no editable field until its search entry is tapped, so the test taps it first and
/// drives `set_value` against the `EditText` that appears. The read-back comes from `uiautomator`'s
/// view of the field, not from anything glass remembers writing.
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
        window,
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    let mut a11y = glass_android::AndroidA11y::new();

    let deadline = std::time::Instant::now() + SETUP_BUDGET;
    let mut abandoned: Vec<String> = Vec::new();
    while let Err(Abandoned(why)) = write_leg(&mut platform, &mut a11y, &ctx) {
        eprintln!("abandoned: {why}; reopening Settings");
        abandoned.push(why);
        assert!(
            std::time::Instant::now() < deadline,
            "Settings never held its search screen up for one write leg in {SETUP_BUDGET:?}: \
             {abandoned:?}"
        );
        reopen_settings();
        std::thread::sleep(SETUP_PAUSE);
    }

    platform.stop_app().expect("stop");
    drop(platform);
    agents.shutdown();
}

/// Bring `component` (`pkg/.Activity`) to the foreground with a bare `am start` — bypassing
/// `AndroidPlatform::start_app`, whose bookkeeping only ever tracks the one app it launched, so
/// it still names the fixture while the device's real foreground has moved to something else.
fn bring_to_front(component: &str) {
    let adb = std::env::var("GLASS_ADB").unwrap_or_else(|_| "adb".to_string());
    let out = std::process::Command::new(&adb)
        .args(["shell", "am", "start", "-n", component])
        .output()
        .expect("adb shell am start");
    assert!(
        out.status.success(),
        "am start {component} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Restart Settings from nothing, for [`set_value_reports_whether_the_write_landed`]'s retry.
///
/// Do not drop the force-stop: Settings is already front-most by then, so a bare `am start` is
/// only delivered to it — `START … result code=3` — and a home screen that lost its search entry
/// never grew one back without a fresh `com.android.settings` process.
fn reopen_settings() {
    let adb = std::env::var("GLASS_ADB").unwrap_or_else(|_| "adb".to_string());
    let out = std::process::Command::new(&adb)
        .args(["shell", "am", "force-stop", "com.android.settings"])
        .output()
        .expect("adb shell am force-stop");
    assert!(
        out.status.success(),
        "am force-stop com.android.settings failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    bring_to_front("com.android.settings/.Settings");
}

/// The service reader answering while something else holds the foreground. Ignored by default:
///   GLASS_ADB=$HOME/android-sdk/platform-tools/adb GLASS_ANDROID_A11Y_APK=... \
///     cargo test -p glass-android --test a11y_loop -- --ignored --nocapture --test-threads=1
///
/// Only a device shows this: the unit tests model the companion's replies with a fake, so a
/// mismatched reply is whatever the fixture hands the fake — never proof that a real
/// `AccessibilityService` names the true foreground window (glass#286).
#[test]
#[ignore = "requires a booted AVD + the a11y companion installed and enabled"]
fn a_snapshot_taken_while_another_app_is_foreground_says_so() {
    let get = |k: &str| std::env::var(k).ok();
    let apk = glass_android::a11y_apk(&get)
        .expect("set GLASS_ANDROID_A11Y_APK to the built a11y-debug.apk");

    let agents = glass_android::AgentRegistry::new();
    let mut platform =
        glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &agents)
            .expect("attach");
    let window = platform
        .start_app(&settings_spec())
        .expect("launch settings (the fixture asked about)");
    std::thread::sleep(std::time::Duration::from_millis(1200));

    bring_to_front("com.google.android.deskclock/com.android.deskclock.DeskClock");
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let a11y_registry = glass_android::A11yServiceRegistry::new();
    let client = a11y_registry
        .ensure(&platform.resolved_adb(), &apk)
        .expect("install + enable the service");
    let mut svc = glass_android::ServiceA11y::new(client, "com.android.settings".to_string());

    let ctx = AxContext {
        pids: platform.app_pids(),
        window,
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    let tree = svc
        .snapshot(&ctx)
        .expect("a snapshot must still succeed for the wrong app");
    let subject = tree
        .subject
        .expect("the service must disclose it answered about the foreground app, not Settings");
    assert_eq!(subject.asked, "com.android.settings");
    assert_eq!(subject.actual, "com.google.android.deskclock");

    let adb = std::env::var("GLASS_ADB").unwrap_or_else(|_| "adb".to_string());
    let _ = std::process::Command::new(&adb)
        .args(["shell", "am", "force-stop", "com.google.android.deskclock"])
        .output();
    platform.stop_app().expect("stop");
    drop(platform);
    agents.shutdown();
    a11y_registry.shutdown();
}
