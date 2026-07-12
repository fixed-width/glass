//! The macOS `Platform` backend for glass (ScreenCaptureKit + CGEvent + AXUIElement,
//! rendered onto a `CGVirtualDisplay`).
//!
//! Like `glass-windows`, the pure logic ([`keymap`], [`coords`], [`clipboard_route`],
//! [`shim_path`]) is crate-level and unit-tested on the Linux dev box; the OS-touching
//! modules and the `MacosPlatform` impl are gated `#[cfg(target_os = "macos")]`. Off macOS
//! the crate exposes only the pure modules and the code-constant [`capabilities`] map.

// FFI backend: the OS-touching modules need `unsafe`, so this crate opts out of the workspace
// `unsafe_code = "deny"`; each site carries a `// SAFETY:` note (see CLAUDE.md). The pure
// modules below stay `unsafe`-free by convention.
#![allow(unsafe_code)]

use glass_core::capability::{CapabilityMap, CapabilityStatus};

pub mod bundle; // pure .app-bundle logic — cross-platform, host-tested
pub mod clipboard_route; // pure clipboard-routing policy — cross-platform, host-tested
pub mod coords; // pure window-relative <-> global math — cross-platform, host-tested
pub mod keymap; // pure ASCII -> (keycode, shift) US map — cross-platform, host-tested
pub mod shim_path; // pure clip-shim dylib path resolution — cross-platform, host-tested

#[cfg(target_os = "macos")]
mod axwindow;
#[cfg(target_os = "macos")]
mod backend;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod clipboard;
#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod input;
// The visible menu-bar app (`NSStatusItem`): `pub` because glass-mcp's `--menubar` mode
// drives it directly (unlike the `Platform`-seam backends above). macOS-only — it owns an
// AppKit run loop.
#[cfg(target_os = "macos")]
pub mod menubar;
// The first-run permission checklist window (`NSWindow`): `pub` because glass-mcp's
// onboarding mode drives it directly (like `menubar`, unlike the `Platform`-seam backends).
// macOS-only — it owns an AppKit run loop.
#[cfg(target_os = "macos")]
pub mod onboarding_window;
#[cfg(target_os = "macos")]
mod permissions;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
mod scwindow;
#[cfg(target_os = "macos")]
mod session;
#[cfg(target_os = "macos")]
pub use backend::MacosPlatform;
#[cfg(target_os = "macos")]
pub use ffi::init_main_thread;

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

// `doctor`-facing predicates (glass-mcp's doctor.rs): the two TCC grants (+ the exact
// remedy text `preflight`'s `PermissionDenied` error also uses, so the two can't drift)
// and the console session's three-way state (unlocked/locked/no-session-attached).
#[cfg(target_os = "macos")]
pub use permissions::{
    accessibility_granted, accessibility_remedy, screen_recording_granted, screen_recording_remedy,
};
// Guided-setup counterparts to the predicates above: pure pane-URL/open helpers (usable
// anywhere, including `doctor`'s `remedy_action`) and the prompting `request_*` pair
// (used only by the future `setup` command — never by `preflight`/`doctor`).
#[cfg(target_os = "macos")]
pub use permissions::{
    accessibility_pane_url, open_pane, request_accessibility, request_screen_recording,
    screen_recording_pane_url,
};
#[cfg(target_os = "macos")]
pub use session::{session_locked, session_state, SessionState};

/// This backend's canonical name (matches the `glass_capabilities` / `GLASS_BACKEND` value).
pub const BACKEND: &str = "macos";

// Kept last: a `#[cfg(test)]` module must not be followed by other items
// (clippy::items_after_test_module), and the macOS-gated `pub use`s above are absent off
// macOS — so this test module goes at the end of the file where nothing can follow it.
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
    fn multi_touch_unsupported_message_names_the_macos_backend() {
        let msg = glass_core::GlassError::unsupported(
            "multi_touch",
            crate::BACKEND,
            crate::capabilities().multi_touch.note,
        )
        .to_string();
        assert!(msg.contains("macos backend"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
    }
}
