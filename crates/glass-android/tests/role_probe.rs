//! Role-histogram PROBE for the Android half of the accessibility role-parity work — not a
//! pass/fail assertion test. Launches whatever apps `GLASS_A11Y_PROBE_APPS` names (a
//! comma-separated list of `package/activity` components), snapshots each with the node cap
//! lifted, and prints `glass_core::role_histogram`: every widget class the device actually
//! reported, unmapped ([`glass_core::AxRole::Other`]) classes first. The project's rule is
//! probe first, map second — a `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` may only
//! get a match arm for a class that showed up in output like this — so this file's job is to
//! produce that evidence.
//!
//! It asserts exactly one thing *about* the evidence: a class glass does map must not come
//! back `AxRole::Other`, which would mean the reader stopped feeding `axmap::class_to_role`
//! what it reads (see [`MAPPED_CLASSES`]). Otherwise it fails a run only when an app could not
//! be launched or snapshotted at all — a real breakage, distinct from an app simply exposing
//! something unexpected. Which role a class *should* map to is never asserted here.
//!
//! Both readers are probed, because both share one map and they do not see the same tree: the
//! `uiautomator` dump is filtered to nodes marked important-for-accessibility, while the
//! on-device AccessibilityService walks the live `AccessibilityNodeInfo` tree — which is also
//! where Jetpack Compose's semantics-to-`className` translation shows up.
//!
//! Ignored by default; run against a booted AVD:
//!
//! ```sh
//! GLASS_ADB=/path/to/adb \
//! GLASS_A11Y_PROBE_APPS=com.android.settings/.Settings \
//!   cargo test -p glass-android --test role_probe -- --ignored --nocapture
//! ```
//!
//! The service probe additionally needs `GLASS_ANDROID_A11Y_APK` pointing at the built
//! accessibility-service APK. With `GLASS_A11Y_PROBE_APPS` unset (or set but empty) both
//! probes print what to set and pass without probing anything, so a run that did not ask for
//! this never fails because of it.

use std::time::Duration;

use glass_android::{
    A11yServiceRegistry, AgentRegistry, AndroidA11y, AndroidPlatform, EmulatorRegistry, ServiceA11y,
};
use glass_core::accessibility::{Accessibility, AxContext, WalkLimits};
use glass_core::{role_histogram, AppSpec, AxRole, AxTree, Platform, SandboxLevel};

/// Comma-separated `package/activity` components to probe, e.g.
/// `com.android.settings/.Settings`. Each element is exactly what `AppSpec::run`'s first
/// element accepts on this backend. Unset (or set but empty) skips the probe rather than
/// failing a run that never asked for it.
const PROBE_APPS_VAR: &str = "GLASS_A11Y_PROBE_APPS";

/// How long to wait after `start_app` before snapshotting — an activity keeps inflating and
/// animating for a beat after the window appears, and a tree read too early is missing the
/// content the probe exists to see. Matches the settle the sibling on-device tests use.
const STARTUP_SETTLE: Duration = Duration::from_millis(1500);

/// Widget classes glass maps to a role, and that a probed app has actually been seen to
/// report. A histogram bucket carrying one of these must not come back [`AxRole::Other`]: the
/// class reached the reader, so a mapped role is the only correct outcome, and `Other` would
/// mean the plumbing between the reader and `axmap::class_to_role` broke. A class that simply
/// does not appear in a given app asserts nothing — apps differ, and an absent class is not a
/// regression.
const MAPPED_CLASSES: &[&str] = &[
    "android.widget.Button",
    "android.widget.TextView",
    "android.widget.EditText",
    "android.widget.ImageView",
    "android.widget.ImageButton",
    "android.widget.FrameLayout",
    "android.widget.LinearLayout",
    "android.widget.RelativeLayout",
    "android.widget.ScrollView",
    "android.view.View",
    "android.view.ViewGroup",
    "androidx.recyclerview.widget.RecyclerView",
    "androidx.cardview.widget.CardView",
    "androidx.appcompat.widget.LinearLayoutCompat",
    "androidx.compose.ui.platform.ComposeView",
    "androidx.viewpager.widget.ViewPager",
];

/// Every [`MAPPED_CLASSES`] bucket in `tree` that came back [`AxRole::Other`], described — the
/// one thing a histogram can check without becoming brittle about which app exposes what.
/// Everything else the probe prints is evidence for a human, not a pass/fail claim.
fn mapped_class_violations(label: &str, tree: &AxTree) -> Vec<String> {
    role_histogram(tree)
        .into_iter()
        .filter(|e| e.role == AxRole::Other && MAPPED_CLASSES.contains(&e.raw_role.as_str()))
        .map(|e| {
            format!(
                "{label}: {} node(s) reported class {:?} as Other, but glass maps that class \
                 — the reader is not feeding class_to_role what it reads",
                e.count, e.raw_role
            )
        })
        .collect()
}

/// Print `role_histogram(tree)` as one line per `(class, role)` bucket — unmapped
/// ([`AxRole::Other`]) buckets first, which is already the histogram's own sort order, so the
/// classes most worth a human's attention are the first thing printed.
fn print_role_histogram(label: &str, tree: &AxTree) {
    let hist = role_histogram(tree);
    println!("\n===== role histogram: {label} =====");
    println!(
        "{} nodes, {} distinct (class, role) buckets",
        tree.count,
        hist.len()
    );
    if let Some(t) = &tree.truncated {
        println!("  NOTE: {}", t.notice());
    }
    for entry in &hist {
        let tag = if entry.role == AxRole::Other {
            "UNMAPPED"
        } else {
            "mapped"
        };
        println!(
            "  {tag:>8}  x{:<5} role={:?} class={:?}",
            entry.count, entry.role, entry.raw_role
        );
    }
}

/// The components named by [`PROBE_APPS_VAR`], or `None` when the variable is unset or names
/// nothing — the caller prints what to set and returns without probing.
fn probe_targets() -> Option<Vec<String>> {
    let raw = std::env::var(PROBE_APPS_VAR).ok()?;
    let targets: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!targets.is_empty()).then_some(targets)
}

fn spec_for(component: &str, a11y: bool) -> AppSpec {
    AppSpec {
        build: None,
        run: vec![component.to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15_000,
        sandbox: SandboxLevel::Off,
        a11y,
    }
}

/// The node cap lifted, so a big app's tree is never truncated mid-probe. Depth and per-level
/// sibling rails keep their generous structural defaults regardless.
fn uncapped() -> WalkLimits {
    WalkLimits::from_max_nodes(Some(0))
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ADB, and GLASS_A11Y_PROBE_APPS naming components"]
fn uiautomator_role_histogram_probe() {
    let Some(targets) = probe_targets() else {
        println!(
            "skipped: set {PROBE_APPS_VAR} to a comma-separated list of package/activity \
             components to probe, e.g. {PROBE_APPS_VAR}=com.android.settings/.Settings"
        );
        return;
    };

    let agents = AgentRegistry::new();
    let mut platform =
        AndroidPlatform::from_env(&EmulatorRegistry::new(), &agents).expect("attach to a device");
    let mut violations = Vec::new();

    for component in &targets {
        let window = platform
            .start_app(&spec_for(component, true))
            .unwrap_or_else(|e| panic!("start_app({component}): {e}"));
        std::thread::sleep(STARTUP_SETTLE);

        let ctx = AxContext {
            pids: platform.app_pids(),
            window,
            window_handle: None,
            a11y_bus_addr: None,
            limits: uncapped(),
        };
        let mut a11y = AndroidA11y::new();
        let mut tree = a11y
            .snapshot(&ctx)
            .unwrap_or_else(|e| panic!("snapshot({component}): {e}"));
        tree.assign_ids();
        print_role_histogram(component, &tree);
        violations.extend(mapped_class_violations(component, &tree));

        platform.stop_app().expect("stop_app");
    }

    drop(platform); // close the platform's agent connection (if any) before the registry
    agents.shutdown(); // never leak a launched agent
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK, and GLASS_A11Y_PROBE_APPS"]
fn service_role_histogram_probe() {
    let Some(targets) = probe_targets() else {
        println!(
            "skipped: set {PROBE_APPS_VAR} to a comma-separated list of package/activity \
             components to probe, e.g. {PROBE_APPS_VAR}=com.android.settings/.Settings"
        );
        return;
    };
    let Ok(apk) = std::env::var("GLASS_ANDROID_A11Y_APK") else {
        println!("skipped: set GLASS_ANDROID_A11Y_APK to the built accessibility-service APK");
        return;
    };

    let agents = AgentRegistry::new();
    let mut platform =
        AndroidPlatform::from_env(&EmulatorRegistry::new(), &agents).expect("attach to a device");
    let registry = A11yServiceRegistry::new();
    let mut violations = Vec::new();

    for component in &targets {
        let window = platform
            .start_app(&spec_for(component, false))
            .unwrap_or_else(|e| panic!("start_app({component}): {e}"));
        std::thread::sleep(STARTUP_SETTLE);

        let client = registry
            .ensure(&platform.resolved_adb(), &apk)
            .expect("install + enable + connect the accessibility service");
        // The service reports the *active* window's tree, so the package filter stays empty
        // and whichever activity was just launched is what gets walked.
        let mut a11y = ServiceA11y::new(client, String::new());
        let ctx = AxContext {
            pids: vec![],
            window,
            window_handle: None,
            a11y_bus_addr: None,
            limits: uncapped(),
        };
        let mut tree = a11y
            .snapshot(&ctx)
            .unwrap_or_else(|e| panic!("service snapshot({component}): {e}"));
        tree.assign_ids();
        let label = format!("{component} (service)");
        print_role_histogram(&label, &tree);
        violations.extend(mapped_class_violations(&label, &tree));

        platform.stop_app().expect("stop_app");
    }

    drop(platform);
    agents.shutdown();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
