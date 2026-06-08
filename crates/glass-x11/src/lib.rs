#![feature(portable_simd)]
//! glass-x11: the Linux/X11 `glass_core::Platform` backend.

// Modules are added task-by-task.

pub mod clipboard;
pub mod command;
pub mod coords;
pub mod doctor;
pub mod pixels;
pub mod platform;
pub mod xvfb;
pub use platform::X11Platform;
pub use xvfb::Xvfb;
