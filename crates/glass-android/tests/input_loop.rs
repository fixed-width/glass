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

// A fixed settle() here (800ms, tuned on a warm dev box) proved too tight on a cold CI
// emulator: 2 failures in 9 runs, e.g. "tap should change the screen, got 0.05320216%" — the
// screen had started changing but the frame was grabbed mid-transition. Poll instead: a
// screen that never changes still fails, with the last percentage and elapsed wait reported.

// 150ms re-checks promptly without busy-looping against a capture-and-diff cycle.
const DIFF_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
// Several times the 800ms that wasn't enough, but still short enough to bound a genuine
// failure.
const DIFF_POLL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(6);

/// Re-diffs `before` against a fresh capture until `changed_pct` clears `threshold` or the
/// deadline expires. Returns the last diff and elapsed wait either way, so a timed-out call
/// still has a diagnostic to report.
fn diff_until_changed(
    p: &mut glass_android::AndroidPlatform,
    before: &glass_core::Frame,
    threshold: f32,
) -> (glass_core::DiffResult, std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        let after = p.capture_frame(None).expect("frame after action");
        let d = glass_core::diff(before, &after, 10).expect("diff");
        let elapsed = start.elapsed();
        if d.changed_pct > threshold || elapsed >= DIFF_POLL_DEADLINE {
            return (d, elapsed);
        }
        std::thread::sleep(DIFF_POLL_INTERVAL);
    }
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn scroll_and_tap_change_the_screen() {
    let agents = glass_android::AgentRegistry::new();
    let mut p =
        glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &agents)
            .expect("attach to emulator");
    let geo = p.start_app(&settings_spec()).expect("launch settings");
    settle();

    let (cx, cy) = (geo.width as i32 / 2, geo.height as i32 / 2);

    // Scroll down the Settings list — the screen should change.
    let before = p.capture_frame(None).expect("frame before scroll");
    p.send_pointer(&PointerEvent::Scroll {
        x: cx,
        y: cy,
        dx: 0,
        dy: 3,
        modifiers: vec![],
    })
    .expect("scroll");
    let (d, elapsed) = diff_until_changed(&mut p, &before, 1.0);
    assert!(
        d.changed_pct > 1.0,
        "scroll should change the screen, got {}% after {}ms",
        d.changed_pct,
        elapsed.as_millis()
    );

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
    let (d, elapsed) = diff_until_changed(&mut p, &before, 1.0);
    assert!(
        d.changed_pct > 1.0,
        "tap should change the screen, got {}% after {}ms",
        d.changed_pct,
        elapsed.as_millis()
    );

    p.stop_app().expect("stop");
    drop(p); // close the platform's agent connection (if any) first
    agents.shutdown(); // tear down a launched agent — these tests must not leak it
}
