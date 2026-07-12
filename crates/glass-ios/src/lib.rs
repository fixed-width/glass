//! iOS Simulator backend for glass: drives native apps over `xcrun simctl`.
//!
//! macOS-only in practice (the tools are Apple's), but the code links nothing
//! platform-specific — it shells out. The Simulator is the isolation boundary,
//! so there is no sandbox machinery here. The backend drives input (tap, type,
//! swipe, scroll) and reads the accessibility tree via `idb_companion`;
//! multi-touch gestures are not yet supported.
#![forbid(unsafe_code)]

mod a11y;
mod axmap;
mod capture;
mod device;
pub mod doctor;
mod idb;
mod injector;
mod logs;
mod platform;
mod simctl;
mod target;

pub use a11y::IosA11y;
pub use platform::IosPlatform;
pub use simctl::Simctl;
pub use target::{SimTarget, SimulatorRegistry};

use glass_core::capability::{CapabilityMap, CapabilityStatus, Support};

/// This backend's live capability map. `input`/`accessibility` need `idb_companion` —
/// gated on [`doctor::companion_present`], the same presence signal the runtime spawn
/// resolves.
pub fn capabilities() -> CapabilityMap {
    capabilities_with(crate::doctor::companion_present())
}

fn capabilities_with(companion: bool) -> CapabilityMap {
    CapabilityMap {
        input: if companion {
            CapabilityStatus::degraded("US-ASCII input only; non-ASCII characters are unsupported")
        } else {
            CapabilityStatus::requires_setup("needs idb_companion (observe-only without it)")
        },
        multi_touch: CapabilityStatus::unsupported(Some("idb provides single-contact touch only")),
        clipboard: CapabilityStatus::new(
            Support::Supported,
            Some("paste needs on-screen consent (Allow Paste)"),
        ),
        accessibility: if companion {
            CapabilityStatus::supported()
        } else {
            CapabilityStatus::requires_setup("idb_companion not found (needed for accessibility)")
        },
        window_move_resize: CapabilityStatus::unsupported(Some("apps are full-screen")),
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use glass_core::capability::Support;

    #[test]
    fn input_is_degraded_with_companion_requires_setup_without() {
        let c = capabilities_with(true);
        assert_eq!(c.input.status, Support::Degraded);
        assert!(c.input.note.unwrap().contains("ASCII"));
        assert_eq!(
            capabilities_with(false).input.status,
            Support::RequiresSetup
        );
    }

    #[test]
    fn accessibility_gates_on_companion() {
        assert_eq!(
            capabilities_with(true).accessibility.status,
            Support::Supported
        );
        assert_eq!(
            capabilities_with(false).accessibility.status,
            Support::RequiresSetup
        );
    }

    #[test]
    fn constant_cells_are_fixed() {
        let c = capabilities_with(true);
        assert_eq!(c.multi_touch.status, Support::Unsupported);
        assert_eq!(
            c.multi_touch.note,
            Some("idb provides single-contact touch only")
        );
        assert_eq!(c.clipboard.status, Support::Supported);
        assert_eq!(
            c.clipboard.note,
            Some("paste needs on-screen consent (Allow Paste)")
        );
        assert_eq!(c.window_move_resize.status, Support::Unsupported);
    }
}
