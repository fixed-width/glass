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
        let msg = glass_core::GlassError::unsupported(
            "multi_touch",
            crate::BACKEND,
            crate::capabilities().multi_touch.note,
        )
        .to_string();
        assert!(msg.contains("x11 backend"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
        assert!(!msg.contains("android"), "{msg}");
    }
}
