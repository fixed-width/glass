//! Live managed-AVD verification. Ignored by default; run with NO emulator running
//! and the SDK present:
//!   ANDROID_SDK_ROOT=$HOME/android-sdk GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     GLASS_AVD=glass cargo test -p glass-android --test managed_avd -- --ignored --nocapture
//!
//! Asserts: boot when none online; reuse on a second resolve (no 2nd boot); cleanup kills it.

use glass_android::EmulatorRegistry;
use std::process::Command;

fn adb() -> String {
    std::env::var("GLASS_ADB").unwrap_or_else(|_| "adb".into())
}

fn online_count() -> usize {
    let out = Command::new(adb()).arg("devices").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("\tdevice"))
        .count()
}

#[test]
#[ignore = "requires NO running emulator + the Android SDK (GLASS_AVD)"]
fn boots_reuses_and_cleans_up() {
    assert_eq!(online_count(), 0, "start this test with no emulator running");
    let registry = EmulatorRegistry::new();

    // First resolve boots the AVD.
    let mut p1 = glass_android::AndroidPlatform::from_env(&registry).expect("boot+attach");
    assert_eq!(online_count(), 1, "expected one emulator after boot");
    let _ = &mut p1;

    // Second resolve attaches to the same emulator — no second boot.
    let _p2 = glass_android::AndroidPlatform::from_env(&registry).expect("attach reuse");
    assert_eq!(online_count(), 1, "reuse must not boot a second emulator");

    // Cleanup stops the glass-booted emulator.
    registry.kill_all();
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(online_count(), 0, "kill_all should stop the booted emulator");
}
