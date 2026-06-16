//! Live multi-window verification. Ignored by default; run with a booted AVD:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test window_loop -- --ignored --nocapture
//!
//! Asserts list_windows returns the app's window(s), select on a real id works, and a
//! bogus id is WindowNotFound.

use glass_core::{AppSpec, Platform, SandboxLevel, WindowId};

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

#[test]
#[ignore = "requires a booted AVD + GLASS_ANDROID_SERIAL/GLASS_ADB"]
fn lists_and_selects_app_windows() {
    let agents = glass_android::AgentRegistry::new();
    let mut p = glass_android::AndroidPlatform::from_env(&glass_android::EmulatorRegistry::new(), &agents)
        .expect("attach");
    let geo = p.start_app(&settings_spec()).expect("launch settings");
    std::thread::sleep(std::time::Duration::from_millis(800));

    let windows = p.list_windows().expect("list");
    println!("windows: {windows:#?}");
    assert!(!windows.is_empty(), "expected at least the activity window");
    let active = windows.iter().find(|w| w.active).expect("an active window");
    assert_eq!(active.geometry, geo, "active window matches start geometry");

    // Re-selecting the active window returns its geometry.
    let g = p.select_window(active.id).expect("select active");
    assert_eq!(g, geo);

    // A bogus id is not found.
    assert!(matches!(
        p.select_window(WindowId(0xdead_beef)),
        Err(glass_core::GlassError::WindowNotFound)
    ));

    p.stop_app().expect("stop");
    drop(p);            // close the platform's agent connection (if any) first
    agents.shutdown();  // tear down a launched agent — these tests must not leak it
}
