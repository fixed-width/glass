//! On-box test that the iOS Simulator backend degrades gracefully when `idb_companion`
//! is unavailable: capture / logs / clipboard keep working (observe-only), while input
//! and the accessibility tree report a clear `Unsupported` rather than a hard start-up
//! failure.
//!
//! Its own test binary so pointing `GLASS_IDB_COMPANION` at a nonexistent path (to force
//! the no-companion path regardless of the host) can't leak into the sibling on-box tests
//! that DO want a real companion.
//!
//! `#[ignore]`d so a plain `cargo test` skips it; it still needs a booted Simulator and the
//! fixture `.app`. Run explicitly on a macOS host with a Simulator booted:
//!
//! ```sh
//! GLASS_IOS_APP=/path/to/GlassFixture.app \
//!   cargo test -p glass-ios --test observe_only_integration -- --ignored --nocapture
//! ```

use glass_core::{
    AppSpec, GlassError, KeyEvent, MouseButton, Platform, PointerEvent, SandboxLevel,
};
use glass_ios::{IosPlatform, SimulatorRegistry};

fn tap(x: i32, y: i32) -> PointerEvent {
    PointerEvent::Click {
        x,
        y,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    }
}

#[test]
#[ignore = "on-box only: needs a macOS host with Xcode + a booted iOS Simulator and \
            GLASS_IOS_APP pointing at the GlassFixture .app; forces the no-companion path"]
fn observe_only_survives_without_a_companion() {
    let app = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must be set to the GlassFixture .app path");

    // Force the no-companion path regardless of what's installed on the host: an
    // unresolvable binary makes the driver fail to start, so the backend degrades.
    std::env::set_var(
        "GLASS_IDB_COMPANION",
        "/nonexistent/definitely-not-idb_companion",
    );

    let spec = AppSpec {
        build: None,
        run: vec![app],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        a11y: true,
    };

    let reg = SimulatorRegistry::new();
    // from_env must NOT fail just because the companion is unavailable — this is the
    // regression the fix targets.
    let mut platform = IosPlatform::from_env(&reg)
        .expect("from_env must succeed observe-only when idb_companion is unavailable");

    // start_app must succeed (it reports geometry from the screenshot; scale discovery is
    // skipped without a driver).
    let window = platform
        .start_app(&spec)
        .expect("start_app must succeed observe-only (no scale discovery without a driver)");
    assert!(
        window.width > 0 && window.height > 0,
        "observe-only geometry must be non-zero, got {window:?}"
    );

    // Observe-only surface still works: capture and a clipboard round-trip.
    let frame = platform
        .capture_frame(None)
        .expect("capture_frame must work observe-only (simctl screenshot)");
    assert_eq!((frame.width, frame.height), (window.width, window.height));

    const SENTINEL: &str = "glass-ios-observe-only-\u{2713}";
    platform
        .set_clipboard(SENTINEL)
        .expect("set_clipboard must work observe-only (simctl pbcopy)");
    assert_eq!(
        platform
            .get_clipboard()
            .expect("get_clipboard must work observe-only (simctl pbpaste)"),
        SENTINEL,
        "clipboard round-trip must work observe-only"
    );

    // Input degrades to a clear Unsupported (not a hard failure, not a silent drop).
    assert!(
        matches!(
            platform.send_pointer(&tap(10, 10)).unwrap_err(),
            GlassError::Unsupported(_)
        ),
        "send_pointer must report Unsupported without a companion"
    );
    assert!(
        matches!(
            platform.send_key(&KeyEvent::Text("hi".into())).unwrap_err(),
            GlassError::Unsupported(_)
        ),
        "send_key must report Unsupported without a companion"
    );

    // No accessibility reader without a companion — and no connect is attempted (Ok(None)).
    let reader = platform
        .accessibility()
        .expect("accessibility() must not error observe-only (no connect attempted)");
    assert!(
        reader.is_none(),
        "there must be no accessibility reader without a companion"
    );

    platform.stop_app().expect("stop_app");
}
