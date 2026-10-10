//! End-to-end proof that an app which dies on launch is reported as failed, not started.
//!
//! `simctl launch` exits 0 as soon as launchd has spawned the process, so a backend that trusts
//! that exit status reports a window for an app that is already gone — the screenshot behind that
//! geometry being the Simulator home screen. Only a real launch can show whether the check
//! catches it, and only a real launch can show whether it catches it *in time*: an aborting app
//! takes a beat to actually go, and a check that runs too early passes while proving nothing.
//! That is not hypothetical — the first draft of the check did exactly that, and this test is
//! what caught it.
//!
//! The fixture treats an unrecognized `--tab` as fatal, which is the shape being tested: an app
//! that rejects one of its launch arguments.
//!
//! Its own test binary, holding one test, like every sibling here: two launches of the same
//! fixture on the same Simulator would terminate each other's app if libtest ran them
//! concurrently — and a killed app looks exactly like an app that died on its own, so the test
//! would pass while proving nothing. The observe-only case lives in
//! `launch_liveness_observe_only_integration.rs` for the same reason.
//!
//! `#[ignore]`d so a plain `cargo test` skips it: it needs `xcrun simctl` (macOS + Xcode only), a
//! booted Simulator, and the RoleFixture app from `examples/ios-role-fixture/`. Run it with:
//!
//! ```sh
//! ./examples/ios-role-fixture/build.sh
//! GLASS_IOS_ROLE_FIXTURE="$PWD/examples/ios-role-fixture/build/RoleFixture.app" \
//!   cargo test -p glass-ios --test launch_liveness_integration -- --ignored --test-threads=1
//! ```

#![cfg(unix)]

use glass_core::{AppSpec, Platform, SandboxLevel};
use glass_ios::{IosPlatform, SimulatorRegistry};

/// A spec that launches the fixture with a `--tab` value it does not recognize, which the fixture
/// treats as fatal.
fn fatal_spec(app: &str) -> AppSpec {
    AppSpec {
        build: None,
        run: vec![app.to_string(), "--tab=no-such-screen".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    }
}

fn fixture_path() -> String {
    std::env::var("GLASS_IOS_ROLE_FIXTURE")
        .expect("GLASS_IOS_ROLE_FIXTURE must be set to the RoleFixture.app path")
}

/// Assert the launch failed *because the app died*, and that the message says so — not merely
/// that some error occurred, which any unrelated breakage would also satisfy.
fn assert_reported_as_died(result: glass_core::Result<glass_core::WindowGeometry>, mode: &str) {
    let error = result.err().unwrap_or_else(|| {
        panic!("{mode}: an app that aborted on launch must not report a window")
    });
    let message = error.to_string();
    assert!(
        message.contains("exited before its window"),
        "{mode}: the error must say the app died rather than blaming window discovery, got: \
         {message}"
    );
    // The app's own last words have to travel in the message: this return drops the log stream
    // and never registers a session, so `glass_logs` would answer "no active session" afterwards.
    // The fixture dies via `fatalError`, whose message goes to stderr, so this is the specific
    // thing the launch's `--stderr` capture exists to recover.
    assert!(
        message.contains("Fatal error") && message.contains("no-such-screen"),
        "{mode}: the error must carry what the app said on the way out, not the device's log. \
         Got: {message}"
    );
}

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + a booted iOS Simulator, and \
            GLASS_IOS_ROLE_FIXTURE pointing at the examples/ios-role-fixture .app"]
fn an_app_that_dies_on_a_bad_argument_is_reported_as_failed_rather_than_started() {
    let app = fixture_path();
    let reg = SimulatorRegistry::new();
    let mut platform =
        IosPlatform::from_env(&reg).expect("from_env: resolve/boot a Simulator and open idb");

    let result = platform.start_app(&fatal_spec(&app));
    let _ = platform.stop_app();

    assert_reported_as_died(result, "with a driver");
}
