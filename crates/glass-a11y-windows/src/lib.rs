//! Windows accessibility backend for glass: reads the active app's UI Automation tree
//! and produces the platform-agnostic `glass_core::AxTree`. Implements the per-OS
//! `Accessibility` seam (orthogonal to the display `Platform` seam). Mirrors
//! `glass-a11y-linux`.

pub mod doctor;
pub mod mapping; // pure UIA->normalized mapping — cross-platform, host-tested

#[cfg(windows)]
mod events;

#[cfg(windows)]
mod reader;

#[cfg(windows)]
pub use reader::WindowsA11y;
