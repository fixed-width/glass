//! The macOS `Platform` backend for glass (ScreenCaptureKit + CGEvent + AXUIElement,
//! rendered onto a `CGVirtualDisplay`).
//!
//! Like `glass-windows`, the pure logic ([`keymap`], [`coords`]) is crate-level and
//! unit-tested on the Linux dev box; the OS-touching modules and the `MacosPlatform`
//! impl are gated `#[cfg(target_os = "macos")]`. Off macOS the crate exposes only the
//! pure modules.

pub mod coords; // pure window-relative <-> global math — cross-platform, host-tested
pub mod keymap; // pure ASCII -> (keycode, shift) US map — cross-platform, host-tested

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod permissions;
#[cfg(target_os = "macos")]
mod scwindow;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
mod backend;
#[cfg(target_os = "macos")]
pub use backend::MacosPlatform;
