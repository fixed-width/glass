//! glass-x11: the Linux/X11 `glass_core::Platform` backend.

// Modules are added task-by-task.

use glass_core::capability::{CapabilityMap, CapabilityStatus};

pub mod clipboard;
pub mod command;
pub mod coords;
pub mod doctor;
pub mod pixels;
pub mod platform;
pub mod xvfb;
pub use platform::X11Platform;
pub use xvfb::Xvfb;

/// This backend's canonical name (matches the `glass_capabilities` / `GLASS_BACKEND` value).
pub const BACKEND: &str = "x11";

/// This backend's capability map. Every cell but `accessibility` is code-constant; the
/// `accessibility` cell is live — gated on whether the AT-SPI bus launcher is installed
/// (`glass_a11y_linux::doctor::accessibility_launcher_present`), the same signal
/// `glass_doctor` reads, so an uninstalled AT-SPI stack reports `requires_setup` rather
/// than a confident `supported`.
pub fn capabilities() -> CapabilityMap {
    capabilities_with(glass_a11y_linux::doctor::accessibility_launcher_present())
}

fn capabilities_with(a11y_launcher_present: bool) -> CapabilityMap {
    CapabilityMap {
        input: CapabilityStatus::supported(),
        multi_touch: MULTI_TOUCH,
        clipboard: CapabilityStatus::supported(),
        accessibility: glass_a11y_linux::doctor::accessibility_capability(a11y_launcher_present),
        window_move_resize: CapabilityStatus::supported(),
    }
}

/// Multi-touch is a code-constant `Unsupported` on this desktop backend. One source for both
/// the capability map and the gesture-rejection error ([`unsupported_multi_touch`]) — so the
/// error path reads a `const` note instead of routing through the now-live `capabilities()`,
/// which probes the a11y stack.
const MULTI_TOUCH: CapabilityStatus = CapabilityStatus::unsupported(None);

/// The `Unsupported` error this backend returns for a multi-touch gesture — one source
/// for the call site (`send_pointer`'s `Gesture` arm) and its test.
pub(crate) fn unsupported_multi_touch() -> glass_core::GlassError {
    glass_core::GlassError::unsupported("multi_touch", BACKEND, MULTI_TOUCH.note)
}

#[cfg(test)]
mod capability_tests {
    use super::{capabilities, capabilities_with};
    use glass_core::capability::Support;

    #[test]
    fn constant_capability_cells() {
        // Everything but `accessibility` is code-constant on this desktop backend.
        let c = capabilities_with(true);
        assert_eq!(c.input.status, Support::Supported);
        assert_eq!(c.multi_touch.status, Support::Unsupported);
        assert_eq!(c.clipboard.status, Support::Supported);
        assert_eq!(c.window_move_resize.status, Support::Supported);
    }

    #[test]
    fn accessibility_is_live_on_the_at_spi_launcher() {
        assert_eq!(
            capabilities_with(true).accessibility.status,
            Support::Supported
        );
        let absent = capabilities_with(false).accessibility;
        assert_eq!(absent.status, Support::RequiresSetup);
        assert!(absent.note.unwrap().contains("at-spi2-core"));
    }

    #[test]
    fn public_capabilities_reads_the_live_launcher_signal() {
        // No parallel probe: the public map's `accessibility` cell is exactly what the
        // shared launcher-present signal produces.
        assert_eq!(
            capabilities().accessibility,
            glass_a11y_linux::doctor::accessibility_capability(
                glass_a11y_linux::doctor::accessibility_launcher_present()
            )
        );
    }

    #[test]
    fn multi_touch_unsupported_message_names_this_backend_not_android() {
        let msg = crate::unsupported_multi_touch().to_string();
        assert!(msg.contains("x11 backend"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
        assert!(!msg.contains("android"), "{msg}");
    }
}
