//! End-to-end proof that `AppSpec::run`'s tail reaches the launched app.
//!
//! `run[1..]` is documented as the program's arguments, and every desktop backend spawns them
//! directly. This backend has to route them through `simctl launch`, which is a different
//! mechanism, so the unit test over the argv builder cannot say whether the app ever sees them —
//! only a launch can. It asserts the app *acted* on the argument rather than merely receiving it:
//! the role fixture selects its screen from `--tab=<name>`, and the selected screen is
//! identifiable in the tree.
//!
//! `#[ignore]`d so a plain `cargo test` (Linux dev host, CI) skips it: the backend needs
//! `xcrun simctl` + `idb_companion` (macOS + Xcode only), a booted Simulator, and the RoleFixture
//! app from `examples/ios-role-fixture/`. Run explicitly on such a host:
//!
//! ```sh
//! ./examples/ios-role-fixture/build.sh
//! GLASS_IOS_ROLE_FIXTURE="$PWD/examples/ios-role-fixture/build/RoleFixture.app" \
//!   cargo test -p glass-ios --test launch_args_integration -- --ignored --nocapture
//! ```
//!
//! A separate variable from `GLASS_IOS_APP` on purpose: the sibling integration tests drive
//! `examples/ios-fixture/`, which has no launch-argument surface to check.

use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxTree, WalkLimits};
use glass_core::{AppSpec, Platform, SandboxLevel};
use glass_ios::{IosPlatform, SimulatorRegistry};

/// Whether any node in the tree is identified as `name` — a snapshot's `name` carries an
/// element's `AXUniqueId`.
///
/// The collection and SwiftUI screens identify themselves (`screen-collection`,
/// `screen-swiftui`); the Controls screen's identifier sits on a `UIStackView`, which UIKit does
/// not expose as an element, so that screen is recognized by a control only it has.
fn has_identifier(node: &AxNode, name: &str) -> bool {
    node.name.as_deref() == Some(name) || node.children.iter().any(|c| has_identifier(c, name))
}

/// How many times to re-read before giving up on a screen appearing, and how long to wait
/// between: the sibling on-box tests settle for about a second and a half in total.
const SETTLE_ATTEMPTS: usize = 6;
const SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

fn spec_with_args(app: &str, args: &[&str]) -> AppSpec {
    let mut run = vec![app.to_string()];
    run.extend(args.iter().map(|a| (*a).to_string()));
    AppSpec {
        build: None,
        run,
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    }
}

fn screen_of(spec: &AppSpec) -> AxTree {
    let reg = SimulatorRegistry::new();
    let mut platform =
        IosPlatform::from_env(&reg).expect("from_env: resolve/boot a Simulator and open idb");
    let window = platform
        .start_app(spec)
        .expect("start_app: install, launch, report geometry");
    let mut a11y = platform
        .accessibility()
        .expect("accessibility(): connect a second idb client to the same companion socket")
        .expect("companion present on this on-box run, so a reader is available");
    let ctx = AxContext {
        pids: vec![],
        window: window.clone(),
        window_handle: None,
        a11y_bus_addr: None,
        limits: WalkLimits::DEFAULT,
    };
    // UIKit keeps building the hierarchy for a beat after the app is up, so a tree read straight
    // after `start_app` can be half-drawn — which would fail here as "the argument did not
    // arrive", the most misleading diagnosis available. Retry until the screen is identifiable,
    // matching the settle the sibling on-box tests use.
    let mut tree = a11y.snapshot(&ctx).expect("snapshot");
    for _ in 0..SETTLE_ATTEMPTS {
        if has_identifier(&tree.root, "the-alert-button")
            || has_identifier(&tree.root, "screen-collection")
        {
            break;
        }
        std::thread::sleep(SETTLE_DELAY);
        tree = a11y.snapshot(&ctx).expect("snapshot");
    }
    platform.stop_app().expect("stop_app");
    tree
}

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + idb_companion + a booted iOS \
            Simulator, and GLASS_IOS_ROLE_FIXTURE pointing at the examples/ios-role-fixture .app"]
fn launch_arguments_reach_the_app_in_both_forms() {
    // One test rather than three: each leg launches the same fixture on the same Simulator
    // through the same companion, so as separate `#[test]`s libtest would run them concurrently
    // and they would fight over it — the failure looking like "the argument did not arrive".
    let app = std::env::var("GLASS_IOS_ROLE_FIXTURE")
        .expect("GLASS_IOS_ROLE_FIXTURE must be set to the RoleFixture.app path");

    // Without arguments the fixture opens on Controls: the baseline, so a pass below cannot be
    // explained by the app having shown the collection screen regardless.
    let plain = screen_of(&spec_with_args(&app, &[]));
    assert!(
        has_identifier(&plain.root, "the-alert-button"),
        "no arguments must leave the fixture on its first screen (Controls)"
    );
    assert!(
        !has_identifier(&plain.root, "screen-collection"),
        "the baseline must not already be the screen the argument is supposed to select"
    );

    let joined = screen_of(&spec_with_args(&app, &["--tab=collection"]));
    assert!(
        has_identifier(&joined.root, "screen-collection"),
        "--tab=collection in AppSpec::run must reach the app and select the collection screen"
    );
    assert!(
        !has_identifier(&joined.root, "the-alert-button"),
        "the collection screen must have replaced Controls, not merely appeared alongside it"
    );

    // The separated form is what the desktop backends deliver intact, and `simctl launch` was
    // measured to forward it the same way; pinned here so the claim rests on a test rather than
    // on one probe run.
    let separated = screen_of(&spec_with_args(&app, &["--tab", "collection"]));
    assert!(
        has_identifier(&separated.root, "screen-collection"),
        "a separated --tab collection must reach the app with its value intact"
    );
}
