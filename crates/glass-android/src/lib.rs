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

/// This backend's live capability map. `multi_touch`/`clipboard` need the on-device
/// agent — gated on [`agent::agent_enabled`], the same predicate the runtime uses to
/// pick `AgentInjector` vs `ShellInjector`, so this can't disagree with real behavior.
pub fn capabilities() -> CapabilityMap {
    capabilities_with(crate::agent::agent_enabled(&|k| std::env::var(k).ok()))
}

fn capabilities_with(agent_enabled: bool) -> CapabilityMap {
    let gated = if agent_enabled {
        CapabilityStatus::supported()
    } else {
        CapabilityStatus::requires_setup(
            "on-device agent not detected; set GLASS_ANDROID_AGENT_JAR",
        )
    };
    CapabilityMap {
        multi_touch: gated,
        clipboard: gated,
        accessibility: CapabilityStatus::supported(),
        window_move_resize: CapabilityStatus::unsupported(Some("apps are full-screen")),
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use glass_core::capability::Support;

    #[test]
    fn agent_present_makes_multi_touch_and_clipboard_supported() {
        let c = capabilities_with(true);
        assert_eq!(c.multi_touch.status, Support::Supported);
        assert_eq!(c.clipboard.status, Support::Supported);
        assert!(c.multi_touch.note.is_none());
    }

    #[test]
    fn agent_absent_makes_them_requires_setup_with_env_hint() {
        let c = capabilities_with(false);
        assert_eq!(c.multi_touch.status, Support::RequiresSetup);
        assert_eq!(c.clipboard.status, Support::RequiresSetup);
        assert!(c
            .multi_touch
            .note
            .unwrap()
            .contains("GLASS_ANDROID_AGENT_JAR"));
    }

    #[test]
    fn constant_cells_do_not_depend_on_the_agent() {
        for signal in [true, false] {
            let c = capabilities_with(signal);
            assert_eq!(c.accessibility.status, Support::Supported);
            assert_eq!(c.window_move_resize.status, Support::Unsupported);
            assert_eq!(c.window_move_resize.note, Some("apps are full-screen"));
        }
    }
}
