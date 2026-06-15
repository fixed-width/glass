//! Live "see" loop against a running AVD. Ignored by default; run with a booted
//! emulator and `GLASS_ANDROID_SERIAL` set:
//!   cargo test -p glass-android --test see_loop -- --ignored --nocapture
//!
//! Uses `com.android.settings` (present on every system image) so no fixture APK
//! is needed at this phase.

use glass_core::{AppSpec, Platform, SandboxLevel};

fn settings_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["com.android.settings/.Settings".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 10_000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    }
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL"]
fn see_loop_launches_and_captures_settings() {
    let mut p = glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new())
        .expect("attach to emulator");
    let geo = p.start_app(&settings_spec()).expect("launch settings");
    assert!(geo.width > 0 && geo.height > 0, "non-empty window geometry");

    let frame = p.capture_frame(None).expect("screenshot");
    assert_eq!(frame.pixels.len() as u32, frame.width * frame.height * 4);
    assert_eq!((frame.width, frame.height), (geo.width, geo.height));

    // logcat should have produced at least one line by now.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let logs = p.drain_logs();
    assert!(!logs.is_empty(), "expected some logcat output");

    p.stop_app().expect("stop");
}
