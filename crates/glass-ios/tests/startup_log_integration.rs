//! On-box proof for glass-ios #136: a log line an app emits at launch — before its first
//! frame — is captured.
//!
//! Before the fix, the unified-log stream (`xcrun simctl spawn <udid> log stream`) attached
//! only *after* `simctl launch` returned. `log stream` is a live tail with no backlog, so an
//! `applicationDidFinishLaunching` / `App.init` `os_log` was emitted before the subscription
//! existed and was lost; `glass_wait_for_log` on it timed out. The backend now starts the
//! stream before launch and gates the launch on a confirmed-live subscription, so the line is
//! buffered by the time `start_app` returns. This test drives the real `Platform` path and
//! asserts the launch-time marker reaches `drain_logs()`.
//!
//! `#[ignore]`d so a plain `cargo test` (Linux dev host, CI) skips it: the backend shells out
//! to `xcrun simctl` (macOS + Xcode only) and needs a booted Simulator plus an app that logs a
//! known line at launch. Point it at such an app and tell it the exact line to expect:
//!
//! ```sh
//! GLASS_IOS_APP=/path/to/StartupLogger.app \
//! GLASS_IOS_STARTUP_MARKER='GLASS_IOS_STARTUP_MARKER' \
//!   cargo test -p glass-ios --test startup_log_integration -- --ignored --nocapture
//! ```
//!
//! The marker MUST be emitted at the app's earliest launch point (not a delayed `onAppear`),
//! so it exercises the race the fix closes. `GLASS_IOS_UDID` / `GLASS_IOS_DEVICE` select the
//! Simulator the same way they do for `glass-mcp`; see `docs/how-to/setup-ios.md`.

use std::time::Duration;

use glass_core::{AppSpec, Platform, SandboxLevel};
use glass_ios::{IosPlatform, SimulatorRegistry};

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + a booted iOS Simulator, GLASS_IOS_APP \
            pointing at a built .app that emits GLASS_IOS_STARTUP_MARKER at launch"]
fn launch_time_log_line_is_captured() {
    let app = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must point at a built .app that logs at launch");
    let marker = std::env::var("GLASS_IOS_STARTUP_MARKER")
        .expect("GLASS_IOS_STARTUP_MARKER must be the exact line the app emits at launch");

    let spec = AppSpec {
        build: None,
        run: vec![app],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    };

    let reg = SimulatorRegistry::new();
    let mut platform = IosPlatform::from_env(&reg)
        .expect("IosPlatform::from_env (resolve/boot a Simulator per GLASS_IOS_* env)");

    platform
        .start_app(&spec)
        .expect("start_app must install (if needed), launch, and report geometry");

    // With the stream confirmed live before launch, the marker is already buffered when
    // start_app returns. Poll a few times with a short settle regardless: the unified log can
    // deliver a launch line a beat after `simctl launch` returns, and draining is destructive
    // so each batch is inspected as it arrives.
    let mut captured = false;
    'poll: for _ in 0..20 {
        for (_, line) in platform.drain_logs() {
            if line.contains(&marker) {
                captured = true;
                break 'poll;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // Stop before asserting so a failure still tears the app down.
    platform
        .stop_app()
        .expect("stop_app must terminate the launched app");

    assert!(
        captured,
        "launch-time log line {marker:?} must be captured after start_app (glass-ios #136); \
         it was never seen in the drained logs"
    );
}
