//! The launch-liveness check on the observe-only path — no `idb_companion`, so no scale
//! discovery, so the least work between the launch and the check.
//!
//! That makes this the thin case: if the check ever goes back to relying on the incidental cost
//! of neighbouring work rather than on its own settle, this is the configuration that breaks
//! first, and it is the one a user is most likely to be in while debugging a launch.
//!
//! Split from `launch_liveness_integration.rs` because this binary sets an environment variable
//! (see the `unsafe` note below) and because two launches of the same fixture would terminate
//! each other if libtest ran them concurrently.
//!
//! `#[ignore]`d so a plain `cargo test` skips it: needs `xcrun simctl` (macOS + Xcode only), a
//! booted Simulator, and the RoleFixture app from `examples/ios-role-fixture/`:
//!
//! ```sh
//! GLASS_IOS_ROLE_FIXTURE="$PWD/examples/ios-role-fixture/build/RoleFixture.app" \
//!   cargo test -p glass-ios --test launch_liveness_observe_only_integration -- --ignored
//! ```

#![cfg(unix)]
// `env::set_var` is `unsafe` from edition 2024 on (it races concurrent env readers). This binary
// holds exactly one test, so nothing else in the process reads the environment while it is set.
// The site carries a `// SAFETY:` note; the file opts out of `unsafe_code = "deny"`. Mirrors
// `observe_only_integration.rs`, which forces the same path the same way.
#![allow(unsafe_code)]

use glass_core::{AppSpec, Platform, SandboxLevel};
use glass_ios::{IosPlatform, SimulatorRegistry};

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + a booted iOS Simulator, and \
            GLASS_IOS_ROLE_FIXTURE pointing at the examples/ios-role-fixture .app; forces the \
            no-companion path"]
fn an_app_that_dies_on_a_bad_argument_is_caught_without_a_companion() {
    let app = std::env::var("GLASS_IOS_ROLE_FIXTURE")
        .expect("GLASS_IOS_ROLE_FIXTURE must be set to the RoleFixture.app path");

    // Force the no-companion path regardless of what is installed on the host: an unresolvable
    // binary makes the driver fail to start, so the backend degrades to observe-only.
    // SAFETY: the only test in this binary (see the header) — no concurrent env reader.
    unsafe {
        std::env::set_var(
            "GLASS_IDB_COMPANION",
            "/nonexistent/definitely-not-idb_companion",
        )
    };

    let spec = AppSpec {
        build: None,
        // The fixture treats an unrecognized `--tab` as fatal.
        run: vec![app, "--tab=no-such-screen".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    };

    let reg = SimulatorRegistry::new();
    let mut platform =
        IosPlatform::from_env(&reg).expect("from_env degrades to observe-only without a companion");
    let result = platform.start_app(&spec);
    let _ = platform.stop_app();

    let message = result
        .expect_err("an app that aborted on launch must not report a window, driver or not")
        .to_string();
    assert!(
        message.contains("exited before its window"),
        "the error must say the app died rather than blaming window discovery, got: {message}"
    );
}
