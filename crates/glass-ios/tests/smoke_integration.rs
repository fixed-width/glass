//! On-box smoke test for the iOS Simulator backend: launches a real `.app` on a booted
//! Simulator and drives it through `glass_core::Platform`'s core surface — start, capture,
//! clipboard round-trip, stop — exactly the sequence `glass_start`/`glass_screenshot`/
//! `glass_clipboard_*`/`glass_stop` exercise over MCP.
//!
//! `#[ignore]`d so a plain `cargo test` (Linux dev host, CI) skips it; the backend shells out
//! to `xcrun simctl`, which only exists on macOS with Xcode installed, and this test additionally
//! needs a booted Simulator and a real app to launch. Run explicitly on a macOS host with a
//! Simulator already booted:
//!
//! ```sh
//! GLASS_IOS_APP=/path/to/YourApp.app cargo test -p glass-ios --test smoke_integration -- --ignored
//! ```
//!
//! `GLASS_IOS_APP` must be a `.app` bundle path (not a bare bundle id) so `start_app` installs
//! it itself — no separate `simctl install` step is required. `GLASS_IOS_UDID` / `GLASS_IOS_DEVICE`
//! / `GLASS_SIMULATOR_KEEP` (all optional) select and manage the Simulator the same way they do
//! for `glass-mcp`; see `docs/how-to/setup-ios.md`.

use glass_core::{AppSpec, Platform, SandboxLevel};
use glass_ios::{IosPlatform, SimulatorRegistry};

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + a booted iOS Simulator, and GLASS_IOS_APP \
            pointing at a built .app to launch"]
fn smoke_launch_capture_clipboard_stop() {
    let app = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must be set to a built .app path for this test to launch");

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

    let geometry = platform
        .start_app(&spec)
        .expect("start_app must install (if needed), launch, and report the device's geometry");
    assert!(
        geometry.width > 0 && geometry.height > 0,
        "launched app geometry must be non-zero, got {geometry:?}"
    );

    // Give the freshly launched app a moment to actually render before capturing, mirroring the
    // settle used by the sibling on-box integration tests (glass-windows, glass-macos).
    std::thread::sleep(std::time::Duration::from_millis(1000));

    let frame = platform
        .capture_frame(None)
        .expect("capture_frame(None) must screenshot the whole device");
    assert_eq!(
        (frame.width, frame.height),
        (geometry.width, geometry.height),
        "captured frame dimensions must match the geometry reported by start_app"
    );
    assert_eq!(
        frame.pixels.len(),
        (frame.width as usize) * (frame.height as usize) * 4,
        "RGBA buffer must be exactly width*height*4 bytes"
    );

    // Non-ASCII to exercise the same UTF-8 round-trip the sibling backends' clipboard tests do.
    const SENTINEL: &str = "glass-ios-smoke-\u{2713}-\u{e9}";
    platform
        .set_clipboard(SENTINEL)
        .expect("set_clipboard (simctl pbcopy)");
    assert_eq!(
        platform
            .get_clipboard()
            .expect("get_clipboard (simctl pbpaste)"),
        SENTINEL,
        "clipboard round-trip must return exactly what was set"
    );

    platform
        .stop_app()
        .expect("stop_app must terminate the launched app");
}
