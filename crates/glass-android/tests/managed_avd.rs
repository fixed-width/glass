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

// kill_all only sends `adb emu kill` — a fire-and-forget console command, not a wait for
// exit — so checking online_count() right after it returns races the shutdown.
const KILL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
// Tearing down a QEMU process and its GPU renderer is heavier than a UI transition and can
// run long on a cold CI runner; 30s gives that room while still bounding a genuine failure.
const KILL_POLL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Polls `online_count()` until it reaches 0 or `KILL_POLL_DEADLINE` expires. Returns the
/// last count and elapsed wait either way, so a timed-out call still has a diagnostic.
fn await_offline() -> (usize, std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        let n = online_count();
        let elapsed = start.elapsed();
        if n == 0 || elapsed >= KILL_POLL_DEADLINE {
            return (n, elapsed);
        }
        std::thread::sleep(KILL_POLL_INTERVAL);
    }
}

#[test]
#[ignore = "requires NO running emulator + the Android SDK (GLASS_AVD)"]
fn boots_reuses_and_cleans_up() {
    assert_eq!(
        online_count(),
        0,
        "start this test with no emulator running"
    );
    let registry = EmulatorRegistry::new();

    // First resolve boots the AVD.
    let agents1 = glass_android::AgentRegistry::new();
    let mut p1 =
        glass_android::AndroidPlatform::from_env(&registry, &agents1).expect("boot+attach");
    assert_eq!(online_count(), 1, "expected one emulator after boot");
    let _ = &mut p1;

    // Second resolve attaches to the same emulator — no second boot.
    let agents2 = glass_android::AgentRegistry::new();
    let _p2 = glass_android::AndroidPlatform::from_env(&registry, &agents2).expect("attach reuse");
    assert_eq!(online_count(), 1, "reuse must not boot a second emulator");

    // Cleanup stops the glass-booted emulator.
    drop(p1);
    drop(_p2);
    agents1.shutdown(); // tear down any launched agent — these tests must not leak it
    agents2.shutdown();
    registry.kill_all();
    let (n, elapsed) = await_offline();
    assert_eq!(
        n,
        0,
        "kill_all should stop the booted emulator; still {n} online after {}ms",
        elapsed.as_millis()
    );
}
