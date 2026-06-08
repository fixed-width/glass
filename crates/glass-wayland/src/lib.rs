//! glass-wayland: the Linux/Wayland `Platform` backend (wlroots protocols,
//! per-session headless `sway` compositor).

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
