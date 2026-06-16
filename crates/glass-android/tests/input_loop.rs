//! Live input verification against a running AVD. Ignored by default; run with a
//! booted emulator:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test input_loop -- --ignored --nocapture
//!
//! Drives com.android.settings and asserts (via frame diff) that a scroll and a
//! tap each change the screen — i.e. injection reaches the device.

use glass_core::{AppSpec, MouseButton, Platform, PointerEvent, SandboxLevel};

fn settings_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["com.android.settings/.Settings".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15_000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    }
}

fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(800));
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn scroll_and_tap_change_the_screen() {
    let mut p = glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &glass_android::AgentRegistry::new())
        .expect("attach to emulator");
    let geo = p.start_app(&settings_spec()).expect("launch settings");
    settle();

    let (cx, cy) = (geo.width as i32 / 2, geo.height as i32 / 2);

    // Scroll down the Settings list — the screen should change.
    let before = p.capture_frame(None).expect("frame before scroll");
    p.send_pointer(&PointerEvent::Scroll { x: cx, y: cy, dx: 0, dy: 3, modifiers: vec![] })
        .expect("scroll");
    settle();
    let after = p.capture_frame(None).expect("frame after scroll");
    let d = glass_core::diff(&before, &after, 10).expect("diff");
    assert!(d.changed_pct > 1.0, "scroll should change the screen, got {}%", d.changed_pct);

    // Tap a list row near the top — navigating should change the screen too.
    let before = p.capture_frame(None).expect("frame before tap");
    p.send_pointer(&PointerEvent::Click {
        x: cx,
        y: geo.height as i32 / 6,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    })
    .expect("tap");
    settle();
    let after = p.capture_frame(None).expect("frame after tap");
    let d = glass_core::diff(&before, &after, 10).expect("diff");
    assert!(d.changed_pct > 1.0, "tap should change the screen, got {}%", d.changed_pct);

    p.stop_app().expect("stop");
}
