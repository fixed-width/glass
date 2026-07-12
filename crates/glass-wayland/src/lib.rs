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

/// This backend's capability map. All cells are code-constant here (desktop
/// accessibility is reported Supported when the backend ships an a11y reader; per-OS
/// grants — macOS TCC, Linux AT-SPI — are surfaced by `glass_doctor`).
pub fn capabilities() -> CapabilityMap {
    CapabilityMap {
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
        assert_eq!(c.multi_touch.status, Support::Unsupported);
        assert_eq!(c.clipboard.status, Support::Supported);
        assert_eq!(c.accessibility.status, Support::Supported);
        assert_eq!(c.window_move_resize.status, Support::Supported);
    }
}
