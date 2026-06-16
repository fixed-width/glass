//! Live doctor probe. Ignored by default; run with a booted AVD:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     cargo test -p glass-android --test doctor_loop -- --ignored --nocapture
//!
//! Asserts the deep doctor reports a healthy Android environment. The core
//! capabilities (adb, an online device, capture, a11y dump) must be `Ok`. The
//! `emulator` check only needs to be non-`Fail`: attaching to an already-running
//! emulator is healthy without a resolvable emulator binary (that binary is only
//! needed for managed boot). To make `emulator` `Ok` too, also export
//! `ANDROID_SDK_ROOT=$HOME/android-sdk` (or `GLASS_EMULATOR`) so the binary resolves.

use glass_core::CheckStatus;

#[test]
#[ignore = "requires a booted AVD + GLASS_ADB"]
fn doctor_reports_healthy_android() {
    let checks = glass_android::doctor::checks(true);
    for c in &checks {
        println!("{:?} {}: {}", c.status, c.name, c.detail);
    }
    let by = |n: &str| checks.iter().find(|c| c.name == n).expect("check present");
    assert_eq!(by("adb").status, CheckStatus::Ok);
    assert_eq!(by("device").status, CheckStatus::Ok);
    assert_eq!(by("screencap").status, CheckStatus::Ok);
    assert_eq!(by("uiautomator").status, CheckStatus::Ok);
    // Healthy whether managed-boot-capable (Ok) or attach-only (Warn); never Fail.
    assert_ne!(
        by("emulator").status,
        CheckStatus::Fail,
        "emulator should resolve or at least allow attach"
    );
}
