//! The macOS `Platform` backend for glass (ScreenCaptureKit + CGEvent + AXUIElement,
//! rendered onto a `CGVirtualDisplay`).
//!
//! Like `glass-windows`, the pure logic ([`keymap`], [`coords`], [`clipboard_route`],
//! [`shim_path`]) is crate-level and unit-tested on the Linux dev box; the OS-touching
//! modules and the `MacosPlatform` impl are gated `#[cfg(target_os = "macos")]`. Off macOS
//! the crate exposes only the pure modules.

// FFI backend: the OS-touching modules need `unsafe`, so this crate opts out of the workspace
// `unsafe_code = "deny"`; each site carries a `// SAFETY:` note (see CLAUDE.md). The pure
// modules below stay `unsafe`-free by convention.
#![allow(unsafe_code)]

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
