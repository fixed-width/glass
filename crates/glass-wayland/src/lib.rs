//! glass-wayland: the Linux/Wayland `Platform` backend (wlroots protocols,
//! per-session headless `sway` compositor).

use glass_core::capability::{CapabilityMap, CapabilityStatus};

pub mod clipboard;
pub mod command;
pub mod doctor;
pub mod globals;
pub mod input;
pub mod keyboard;
pub mod pixels;
pub mod platform;
pub mod swayipc;

pub use platform::WaylandPlatform;

/// This backend's canonical name (matches the `glass_capabilities` / `GLASS_BACKEND` value).
pub const BACKEND: &str = "wayland";

/// This backend's capability map. All cells are code-constant here (desktop
/// accessibility is reported Supported when the backend ships an a11y reader; per-OS
/// grants — macOS TCC, Linux AT-SPI — are surfaced by `glass_doctor`).
pub fn capabilities() -> CapabilityMap {
    CapabilityMap {
        input: CapabilityStatus::supported(),
        multi_touch: CapabilityStatus::unsupported(None),
        clipboard: CapabilityStatus::supported(),
        accessibility: CapabilityStatus::supported(),
        window_move_resize: CapabilityStatus::supported(),
    }
}

/// The `Unsupported` error this backend returns for a multi-touch gesture — one source
/// for the call site (`send_pointer`'s `Gesture` arm) and its test.
pub(crate) fn unsupported_multi_touch() -> glass_core::GlassError {
    glass_core::GlassError::unsupported("multi_touch", BACKEND, capabilities().multi_touch.note)
}

#[cfg(test)]
mod capability_tests {
    use super::capabilities;
    use glass_core::capability::Support;

    #[test]
    fn desktop_constant_capability_map() {
        let c = capabilities();
        assert_eq!(c.input.status, Support::Supported);
        assert_eq!(c.multi_touch.status, Support::Unsupported);
        assert_eq!(c.clipboard.status, Support::Supported);
        assert_eq!(c.accessibility.status, Support::Supported);
        assert_eq!(c.window_move_resize.status, Support::Supported);
    }

    #[test]
    fn multi_touch_unsupported_message_names_this_backend_not_android() {
        let msg = crate::unsupported_multi_touch().to_string();
        assert!(msg.contains("wayland backend"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
        assert!(!msg.contains("android"), "{msg}");
    }
}
