//! Android (AVD emulator) backend for glass: drives native apps over `adb`.
//!
//! Host-OS-agnostic — this crate links nothing platform-specific; it shells out
//! to `adb`. The emulator's VM is the isolation boundary, so there is no
//! sandbox machinery here.

mod a11y;
mod a11y_service;
mod adb;
mod agent;
mod avd;
mod axmap;
mod build;
mod cmd;
mod conn;
pub mod doctor;
mod input;
mod logs;
mod parse;
mod platform;
mod screencap;
mod sdk;
mod target;

pub use a11y::AndroidA11y;
pub use a11y_service::{A11yServiceRegistry, ServiceA11y, a11y_apk};
pub use agent::{AgentClient, AgentRegistry};
pub use avd::{DEFAULT_BOOT_TIMEOUT_MS, EmulatorRegistry};
pub use platform::AndroidPlatform;
pub use target::{AdbTarget, AttachedDevice};

use glass_core::capability::{CapabilityMap, CapabilityStatus};

/// This backend's canonical name (matches the `glass_capabilities` / `GLASS_BACKEND` value).
pub const BACKEND: &str = "android";

/// This backend's live capability map. `input`, `multi_touch` and `clipboard` are gated on
/// [`agent::agent_enabled`]; `accessibility` on [`a11y_service::a11y_apk`].
///
/// Both gates answer whether a companion is *configured*, not whether it comes up: one that
/// fails to install or connect degrades at runtime with a note on stderr
/// ([`AndroidPlatform::from_env`] for the agent, [`AndroidPlatform::accessibility`] for the a11y
/// service). This map is computed without a device, so it cannot see that.
pub fn capabilities() -> CapabilityMap {
    let get = |k: &str| std::env::var(k).ok();
    capabilities_with(
        crate::agent::agent_enabled(&get),
        crate::a11y_service::a11y_apk(&get).is_some(),
    )
}

fn capabilities_with(agent: bool, a11y_apk: bool) -> CapabilityMap {
    let agent_gated = if agent {
        CapabilityStatus::supported()
    } else {
        CapabilityStatus::requires_setup("needs the on-device agent; set GLASS_ANDROID_AGENT_JAR")
    };
    CapabilityMap {
        input: if agent {
            CapabilityStatus::supported()
        } else {
            CapabilityStatus::degraded(
                "adb input only; set GLASS_ANDROID_AGENT_JAR for high-fidelity input",
            )
        },
        multi_touch: agent_gated,
        clipboard: agent_gated,
        accessibility: if a11y_apk {
            CapabilityStatus::supported()
        } else {
            CapabilityStatus::degraded(
                "basic uiautomator tree only; set GLASS_ANDROID_A11Y_APK for the Compose tree + \
                 high-fidelity set_value",
            )
        },
        window_move_resize: CapabilityStatus::unsupported(Some("apps are full-screen")),
    }
}

/// The `Unsupported` error this backend returns for window move/resize — one source for
/// the call site (`window()`'s `Resize`/`Move` arm) and its test.
pub(crate) fn unsupported_window_move_resize() -> glass_core::GlassError {
    glass_core::GlassError::unsupported(
        "window_move_resize",
        BACKEND,
        capabilities().window_move_resize.note,
    )
}

/// Give the device back what glass took: the accessibility companion it enabled, the `adb forward`
/// it opened, the emulator it booted — all by `deadline`.
///
/// `Glass::shutdown` keeps `TEARDOWN_HOOK_RESERVE` back from the sessions so these have time
/// (glass#422). Between themselves it is first-come-first-served, which makes the order
/// load-bearing: put the emulator last and a wedged device spends the whole deadline on the
/// settings restore, leaving a ~2GB VM that was never asked to stop — for the sake of a step
/// measured at 2ms.
///
/// One function rather than three calls in glass-mcp's hook, so a test can watch the deadline
/// reach all three.
pub fn hand_the_device_back(
    a11y: &A11yServiceRegistry,
    agents: &AgentRegistry,
    emulators: &EmulatorRegistry,
    deadline: glass_core::Deadline,
) {
    emulators.kill_all(deadline);
    a11y.shutdown(deadline);
    agents.shutdown(deadline);
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use glass_core::capability::Support;

    #[test]
    fn input_degrades_without_the_agent() {
        assert_eq!(
            capabilities_with(true, true).input.status,
            Support::Supported
        );
        let c = capabilities_with(false, true);
        assert_eq!(c.input.status, Support::Degraded);
        assert!(c.input.note.unwrap().contains("GLASS_ANDROID_AGENT_JAR"));
    }

    #[test]
    fn multi_touch_and_clipboard_require_the_agent() {
        let c = capabilities_with(false, true);
        assert_eq!(c.multi_touch.status, Support::RequiresSetup);
        assert_eq!(c.clipboard.status, Support::RequiresSetup);
        assert_eq!(
            capabilities_with(true, true).multi_touch.status,
            Support::Supported
        );
    }

    #[test]
    fn accessibility_degrades_without_the_a11y_apk() {
        assert_eq!(
            capabilities_with(true, true).accessibility.status,
            Support::Supported
        );
        let c = capabilities_with(true, false);
        assert_eq!(c.accessibility.status, Support::Degraded);
        assert!(
            c.accessibility
                .note
                .unwrap()
                .contains("GLASS_ANDROID_A11Y_APK")
        );
        assert_eq!(c.window_move_resize.status, Support::Unsupported);
    }

    #[test]
    fn window_move_resize_unsupported_message_names_the_android_backend() {
        let msg = crate::unsupported_window_move_resize().to_string();
        assert!(msg.contains("android backend"), "{msg}");
        assert!(msg.contains("full-screen"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
    }
}

#[cfg(test)]
#[cfg(unix)]
mod teardown_tests {
    use super::*;
    use crate::a11y_service::Active;
    use crate::adb::{Answer, FakeAdb};
    use glass_core::Deadline;
    use std::time::{Duration, Instant};

    /// A device that answers gets all three asked, in the order the deadline makes load-bearing.
    ///
    /// The wedged case below can only ever see the first, so without this the a11y restore — and
    /// the seam that plants something for it to restore — could do nothing and stay green.
    #[test]
    fn a_device_that_answers_is_asked_for_all_three_in_order() {
        let fake = FakeAdb::new(&[("*", Answer::Silent)]);
        let a11y = A11yServiceRegistry::new();
        a11y.set_active_for_test(Active::for_test(fake.adb().clone(), 1234));
        let agents = AgentRegistry::new();
        let emulators = EmulatorRegistry::new();
        emulators.register(fake.adb(), "emulator-5554".into());

        hand_the_device_back(&a11y, &agents, &emulators, Deadline::UNBOUNDED);

        let calls = fake.calls();
        assert_eq!(
            calls,
            vec![
                "-s emulator-5554 emu kill",
                "shell settings delete secure enabled_accessibility_services",
                "shell settings put secure accessibility_enabled 0",
                "forward --remove tcp:1234",
            ],
            "all three registries, emulator first"
        );
    }

    /// The join glass-mcp's shutdown hook makes. Each registry has its own wedged-device test;
    /// this one is about the deadline reaching all three, which nothing else can see — before the
    /// `shutdown`/`shutdown_until` pairs collapsed, dropping it meant calling a differently-named
    /// method (glass#422).
    #[test]
    fn a_device_that_stopped_answering_cannot_spend_more_than_the_deadline_on_all_three() {
        let fake = FakeAdb::new(&[("*", Answer::Lingers)]);
        let a11y = A11yServiceRegistry::new();
        a11y.set_active_for_test(Active::for_test(fake.adb().clone(), 1234));
        let agents = AgentRegistry::new();
        let emulators = EmulatorRegistry::new();
        emulators.register(fake.adb(), "emulator-5554".into());

        let started = Instant::now();
        hand_the_device_back(
            &a11y,
            &agents,
            &emulators,
            Deadline::at(Instant::now() + Duration::from_millis(300)),
        );

        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited {:?} handing the device back — one of the three is running unbounded, and \
             three AdbOp::Shell budgets is 30s against a 3s teardown",
            started.elapsed()
        );
        // The emulator is asked first, so a wedged device cannot leave a VM running that was
        // never even asked to stop. The two behind it are starved, which is the trade.
        assert!(
            fake.called("emu kill"),
            "the largest leak must be asked about before the deadline can be spent: {:?}",
            fake.calls()
        );
    }
}
