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
pub use a11y_service::{a11y_apk, A11yServiceRegistry, ServiceA11y};
pub use agent::{AgentClient, AgentRegistry};
pub use avd::EmulatorRegistry;
pub use platform::AndroidPlatform;
pub use target::{AdbTarget, AttachedDevice};

use glass_core::capability::{CapabilityMap, CapabilityStatus};

/// This backend's canonical name (matches the `glass_capabilities` / `GLASS_BACKEND` value).
pub const BACKEND: &str = "android";

/// This backend's live capability map. `input` degrades and `multi_touch`/`clipboard`
/// need the on-device agent — gated on [`agent::agent_enabled`], the same predicate the
/// runtime uses to pick `AgentInjector` vs `ShellInjector`, so this can't disagree with
/// real behavior. `accessibility` degrades without the a11y APK — gated on
/// [`a11y_service::a11y_apk`], the same predicate the runtime uses to pick the Compose-rich
/// reader vs the basic `uiautomator` one.
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
        assert!(c
            .accessibility
            .note
            .unwrap()
            .contains("GLASS_ANDROID_A11Y_APK"));
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
