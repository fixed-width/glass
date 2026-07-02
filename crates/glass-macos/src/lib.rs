//! The macOS `Platform` backend for glass (ScreenCaptureKit + CGEvent + AXUIElement,
//! rendered onto a `CGVirtualDisplay`).
//!
//! Like `glass-windows`, the pure logic ([`keymap`], [`coords`], [`clipboard_route`]) is
//! crate-level and unit-tested on the Linux dev box; the OS-touching modules and the
//! `MacosPlatform` impl are gated `#[cfg(target_os = "macos")]`. Off macOS the crate exposes
//! only the pure modules.

pub mod clipboard_route; // pure clipboard-routing policy — cross-platform, host-tested
pub mod coords; // pure window-relative <-> global math — cross-platform, host-tested
pub mod keymap; // pure ASCII -> (keycode, shift) US map — cross-platform, host-tested

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod permissions;
#[cfg(target_os = "macos")]
mod scwindow;
#[cfg(target_os = "macos")]
mod axwindow;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod session;
#[cfg(target_os = "macos")]
mod backend;
#[cfg(target_os = "macos")]
mod clipboard;
#[cfg(target_os = "macos")]
pub use backend::MacosPlatform;
#[cfg(target_os = "macos")]
pub use ffi::init_main_thread;
// `doctor`-facing predicates (glass-mcp's doctor.rs): the two TCC grants (+ the exact
// remedy text `preflight`'s `PermissionDenied` error also uses, so the two can't drift)
// and the console session's three-way state (unlocked/locked/no-session-attached).
#[cfg(target_os = "macos")]
pub use permissions::{accessibility_granted, accessibility_remedy, screen_recording_granted, screen_recording_remedy};
#[cfg(target_os = "macos")]
pub use session::{session_locked, session_state, SessionState};
