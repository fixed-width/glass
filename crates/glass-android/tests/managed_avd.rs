//! Live managed-AVD verification. Ignored by default; run with NO emulator running
//! and the SDK present:
//!   ANDROID_SDK_ROOT=$HOME/android-sdk GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!     GLASS_AVD=glass cargo test -p glass-android --test managed_avd -- --ignored --nocapture
//!
//! Asserts: boot when none online; reuse on a second resolve (no 2nd boot); cleanup kills it.

use glass_android::EmulatorRegistry;
use glass_core::Deadline;
use std::process::Command;
use std::time::{Duration, Instant};

mod common;

fn adb() -> String {
    std::env::var("GLASS_ADB").unwrap_or_else(|_| "adb".into())
}

/// Emulators reporting `device` to `adb devices`. Panics rather than returning 0 when adb
/// itself fails: a server restart, an `offline`/`unauthorized` transport and a plain error all
/// print no `\tdevice` line, so a silent 0 would read as "cleanly shut down" on exactly the
/// runs where nothing is known.
fn online_count() -> usize {
    let out = Command::new(adb())
        .arg("devices")
        .output()
        .unwrap_or_else(|e| panic!("run `{} devices`: {e}", adb()));
    assert!(
        out.status.success(),
        "`adb devices` exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("\tdevice"))
        .count()
}

// online_count() shells out to adb, so re-reading faster would mostly measure process spawns.
const KILL_POLL_INTERVAL: Duration = Duration::from_secs(1);
// A headless AVD went offline ~2s after kill_all on a warm dev box; 30s is headroom for a
// loaded CI runner.
const KILL_POLL_DEADLINE: Duration = Duration::from_secs(30);

/// What [`await_offline`] observed. Only `confirmed` may be read as a successful shutdown;
/// `last_count` is a diagnostic.
struct OfflineWait {
    confirmed: bool,
    last_count: usize,
    elapsed: Duration,
}

/// Polls `online_count()` until it reads 0 twice running, or `KILL_POLL_DEADLINE` expires.
/// Polls at all because kill_all only sends `adb emu kill`, a fire-and-forget console command
/// that does not wait for exit. Twice running because an emulator that stays up but flickers
/// offline under load produces a lone 0.
fn await_offline() -> OfflineWait {
    let start = Instant::now();
    let mut zeros_running = 0u32;
    loop {
        let last_count = online_count();
        zeros_running = if last_count == 0 {
            zeros_running + 1
        } else {
            0
        };
        let elapsed = start.elapsed();
        let confirmed = zeros_running >= 2;
        if confirmed || elapsed >= KILL_POLL_DEADLINE {
            return OfflineWait {
                confirmed,
                last_count,
                elapsed,
            };
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
    // `kill_all` below is what this test asserts on, so it stays explicit; this only covers the
    // path where an assertion between here and there panics, which would otherwise leave a
    // booted emulator behind. Idempotent — `kill_all` takes the list.
    let _kill_emulators = common::KillBootedEmulators(&registry);

    // First resolve boots the AVD.
    let agents1 = glass_android::AgentRegistry::new();
    let _stop_agent1 = common::StopAgent(&agents1);
    let mut p1 =
        glass_android::AndroidPlatform::from_env(&registry, &agents1).expect("boot+attach");
    assert_eq!(online_count(), 1, "expected one emulator after boot");
    let _ = &mut p1;

    // Second resolve attaches to the same emulator — no second boot.
    let agents2 = glass_android::AgentRegistry::new();
    let _stop_agent2 = common::StopAgent(&agents2);
    let _p2 = glass_android::AndroidPlatform::from_env(&registry, &agents2).expect("attach reuse");
    assert_eq!(online_count(), 1, "reuse must not boot a second emulator");

    // Cleanup stops the glass-booted emulator.
    drop(p1);
    drop(_p2);
    // Explicit, and before `kill_all`, because this test goes on to assert the emulator went
    // offline: the guards above only repeat this on the panicking path.
    agents1.shutdown(Deadline::UNBOUNDED);
    agents2.shutdown(Deadline::UNBOUNDED);
    registry.kill_all(Deadline::UNBOUNDED);
    let w = await_offline();
    assert!(
        w.confirmed,
        "kill_all should stop the booted emulator; no two consecutive zero readings in {}ms, last read {} online",
        w.elapsed.as_millis(),
        w.last_count
    );
}
