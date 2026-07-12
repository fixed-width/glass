//! The macOS `Platform` backend for glass (ScreenCaptureKit + CGEvent + AXUIElement,
//! rendered onto a `CGVirtualDisplay`).
//!
//! Like `glass-windows`, the pure logic ([`keymap`], [`coords`], [`clipboard_route`],
//! [`shim_path`]) is crate-level and unit-tested on the Linux dev box; the OS-touching
//! modules and the `MacosPlatform` impl are gated `#[cfg(target_os = "macos")]`. Off macOS
//! the crate exposes only the pure modules and the [`capabilities`] map (whose `accessibility`
//! cell is live; every other cell is constant).

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

/// This backend's capability map. Every cell but `accessibility` is code-constant; the
/// `accessibility` cell is live — gated on the macOS Accessibility TCC grant
/// (`AXIsProcessTrusted`, via [`accessibility_granted`]), the same predicate `glass_doctor`
/// reads, so an ungranted process reports `requires_setup` rather than a confident
/// `supported`.
pub fn capabilities() -> CapabilityMap {
    capabilities_with(accessibility_capability_live())
}

/// The live Accessibility-grant signal feeding the capability map. On macOS it is the TCC
/// grant ([`accessibility_granted`]). Off macOS this backend is never dispatched
/// (`capabilities_for` gates on `target_os`), so this is a compile-only stub — the map still
/// compiles for the host unit tests, which drive [`capabilities_with`] directly.
#[cfg(target_os = "macos")]
fn accessibility_capability_live() -> bool {
    permissions::accessibility_granted()
}
#[cfg(not(target_os = "macos"))]
fn accessibility_capability_live() -> bool {
    false
}

/// Multi-touch is a code-constant `Unsupported` on this desktop backend. One source for both
/// the capability map and the gesture-rejection error (`input::send_pointer`'s `Gesture`
/// arm) — so that error path reads a `const` note instead of routing through the now-live
/// `capabilities()`, which calls the `AXIsProcessTrusted` FFI.
pub(crate) const MULTI_TOUCH: CapabilityStatus = CapabilityStatus::unsupported(None);

fn capabilities_with(a11y_granted: bool) -> CapabilityMap {
    CapabilityMap {
        input: CapabilityStatus::supported(),
        multi_touch: MULTI_TOUCH,
        clipboard: CapabilityStatus::supported(),
        accessibility: if a11y_granted {
            CapabilityStatus::supported()
        } else {
            CapabilityStatus::requires_setup(
                "Accessibility grant not held; enable glass in System Settings > Privacy & Security > Accessibility",
            )
        },
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
    use super::capabilities_with;
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
    fn accessibility_is_live_on_the_tcc_grant() {
        assert_eq!(
            capabilities_with(true).accessibility.status,
            Support::Supported
        );
        let ungranted = capabilities_with(false).accessibility;
        assert_eq!(ungranted.status, Support::RequiresSetup);
        assert!(ungranted.note.unwrap().contains("Accessibility"));
    }

    // The live signal is the macOS TCC grant (`AXIsProcessTrusted`), so it can only be read
    // on macOS; off-macOS the backend is never dispatched. Tie the public map to the same
    // predicate `glass_doctor` reads, proving no parallel probe.
    #[cfg(target_os = "macos")]
    #[test]
    fn public_capabilities_reads_the_live_tcc_grant() {
        assert_eq!(
            super::capabilities().accessibility,
            capabilities_with(super::accessibility_granted()).accessibility
        );
    }

    #[test]
    fn multi_touch_unsupported_message_names_the_macos_backend() {
        let msg = glass_core::GlassError::unsupported(
            "multi_touch",
            crate::BACKEND,
            crate::MULTI_TOUCH.note,
        )
        .to_string();
        assert!(msg.contains("macos backend"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
        assert!(!msg.contains("android"), "{msg}");
    }
}
