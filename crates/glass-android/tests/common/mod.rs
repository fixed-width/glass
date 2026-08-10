//! Teardown guards for the device suites.
//!
//! Both registries put state on the device that outlives the process — an `adb forward` each, and
//! for the companion the secure settings that enable it as an accessibility service — that only
//! their `shutdown` takes back off. Neither has a `Drop`, so a test panicking past a trailing
//! `shutdown()` hands an enabled companion to every later reader on that device (glass#423), and
//! `scripts/test-android.sh` fails a run that ends with any of it still on.
//!
//! Declare a guard BEFORE the platform it serves: locals drop in reverse, so the platform's agent
//! connection closes before the registry that owns the agent.

#![allow(dead_code)]

use glass_android::{A11yServiceRegistry, AgentRegistry, EmulatorRegistry};
use glass_core::Deadline;

/// Kills the launched agent and removes its `adb forward` when it drops.
pub struct StopAgent<'a>(pub &'a AgentRegistry);

impl Drop for StopAgent<'_> {
    fn drop(&mut self) {
        // Reached while a panic unwinds, where a second panic aborts the process — nothing here
        // may assert.
        self.0.shutdown(Deadline::UNBOUNDED);
    }
}

/// Restores the device's accessibility settings — `enabled_accessibility_services`,
/// `accessibility_enabled` and the `adb forward` — when it drops.
///
/// Covers a `shutdown` a panic would skip, not one with nothing to undo: `ensure` enables the
/// service before it records anything in `state`, so a failure in that window leaves the
/// companion on and this a no-op (glass#425).
pub struct RestoreServiceState<'a>(pub &'a A11yServiceRegistry);

impl Drop for RestoreServiceState<'_> {
    fn drop(&mut self) {
        self.0.shutdown(Deadline::UNBOUNDED);
    }
}

/// Runs its closure when it drops — for device cleanup with no registry to hang off, like the
/// fixture APK a test installs, which the residue check cannot see. A closure, not a typed guard:
/// `glass_android`'s `Adb` is not exported, so nothing here can name what it calls.
pub struct OnDrop<F: FnMut()>(pub F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Stops any emulator glass booted itself when it drops — a no-op when it attached to a running
/// one, and idempotent with an explicit `kill_all`, which takes the list. The leak it catches is
/// a whole ~2GB VM.
pub struct KillBootedEmulators<'a>(pub &'a EmulatorRegistry);

impl Drop for KillBootedEmulators<'_> {
    fn drop(&mut self) {
        self.0.kill_all(Deadline::UNBOUNDED);
    }
}
