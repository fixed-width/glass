//! Live input verification against a running AVD. Ignored by default; run with a
//! booted emulator:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test input_loop -- --ignored --nocapture
//!
//! Drives com.android.settings and asserts (via frame diff) that a scroll and a
//! tap each change the screen — i.e. injection reaches the device.

use glass_core::{
    AppSpec, Frame, IgnoreMask, MouseButton, Platform, PointerEvent, Region, SandboxLevel,
    StabilityTracker,
};
use std::time::{Duration, Instant};

mod common;

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
    std::thread::sleep(Duration::from_millis(800));
}

// A fixed settle() before the diff (800ms, tuned on a warm dev box) proved too tight on a cold
// CI emulator: 2 failures in 9 runs, e.g. "tap should change the screen, got 0.05320216%" — the
// frame was grabbed mid-transition. Polling instead must not weaken it: a screen that only
// flashes still has to fail, hence the confirming sample below.

// Trims the tail of a capture-and-diff cycle, which costs several times this on an AVD — that
// cycle sets the loop's cadence, not this interval.
const DIFF_POLL_INTERVAL: Duration = Duration::from_millis(150);
// Several times the 800ms that wasn't enough.
const DIFF_POLL_DEADLINE: Duration = Duration::from_secs(6);
// Android draws touch feedback — a pressed-state ripple, an overscroll bounce — well over
// CHANGE_THRESHOLD_PCT and gone a moment later, so a crossing counts only once a later sample
// still clears the threshold. A full second, not the ~300ms the animation runs: confirming
// sooner caught the tail of an overscroll on an AVD.
const CHANGE_CONFIRM_DELAY: Duration = Duration::from_secs(1);
const CHANGE_THRESHOLD_PCT: f32 = 1.0;
// Shared by the settle comparison and the change assertions so both judge "different" alike.
const DIFF_TOLERANCE: u8 = 10;
const SETTLE_FRAMES: u32 = 2;
const SETTLE_DEADLINE: Duration = Duration::from_secs(6);

/// The status bar, whose clock ticks on its own: excluded from the settle comparison so an
/// otherwise-still screen can actually read as settled.
fn status_bar_mask(frame: &Frame) -> IgnoreMask {
    let bar = Region {
        x: 0,
        y: 0,
        width: frame.width,
        height: (frame.height / 20).max(1),
    };
    IgnoreMask::new(&[bar], frame.width, frame.height).expect("status-bar mask")
}

/// Captures until [`StabilityTracker`] reports the screen has stopped moving, returning that
/// settled frame. Panics if it is still moving at `SETTLE_DEADLINE`: a baseline taken mid-motion
/// lets the next assertion pass on leftover motion instead of on the action under test.
fn settled_frame(p: &mut glass_android::AndroidPlatform, what: &str) -> Frame {
    let start = Instant::now();
    let mut tracker: Option<StabilityTracker> = None;
    loop {
        let frame = p.capture_frame(None).expect("frame while settling");
        let t = tracker.get_or_insert_with(|| {
            StabilityTracker::with_mask(SETTLE_FRAMES, DIFF_TOLERANCE, status_bar_mask(&frame))
        });
        if t.observe(frame).expect("settle diff") {
            return t.last().cloned().expect("a frame was just observed");
        }
        assert!(
            start.elapsed() < SETTLE_DEADLINE,
            "screen never settled {what}, so no trustworthy baseline: still moving after {}ms",
            start.elapsed().as_millis()
        );
    }
}

/// Re-diffs `before` against fresh captures and asserts the screen changed and *stayed* changed:
/// the first sample over `CHANGE_THRESHOLD_PCT` is confirmed by another `CHANGE_CONFIRM_DELAY`
/// later, so a momentary flash does not satisfy it. On timeout the panic reports the largest
/// change seen, not the last.
fn assert_screen_changed(p: &mut glass_android::AndroidPlatform, before: &Frame, what: &str) {
    let start = Instant::now();
    let mut peak_pct = 0.0f32;
    let mut confirming = false;
    loop {
        let after = p.capture_frame(None).expect("frame after action");
        let pct = glass_core::diff(before, &after, DIFF_TOLERANCE)
            .expect("diff")
            .changed_pct;
        peak_pct = peak_pct.max(pct);
        if pct > CHANGE_THRESHOLD_PCT {
            if confirming {
                return;
            }
            confirming = true;
            std::thread::sleep(CHANGE_CONFIRM_DELAY);
            continue;
        }
        confirming = false;
        let elapsed = start.elapsed();
        assert!(
            elapsed < DIFF_POLL_DEADLINE,
            "{what} should change the screen and keep it changed, peaked at {peak_pct}% over {}ms",
            elapsed.as_millis()
        );
        std::thread::sleep(DIFF_POLL_INTERVAL);
    }
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn scroll_and_tap_change_the_screen() {
    let agents = glass_android::AgentRegistry::new();
    // Before the platform — see `common` for why the order matters (glass#423).
    let _stop_agent = common::StopAgent(&agents);
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
    assert_screen_changed(&mut p, &before, "scroll");

    // Tap a list row near the top — navigating should change the screen too. The baseline waits
    // for the fling to stop: Scroll is a 300ms swipe the list keeps moving after, and a mid-fling
    // baseline would let this leg pass on that motion.
    let before = settled_frame(&mut p, "after the scroll");
    p.send_pointer(&PointerEvent::Click {
        x: cx,
        y: geo.height as i32 / 6,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    })
    .expect("tap");
    assert_screen_changed(&mut p, &before, "tap");

    p.stop_app().expect("stop");
}
