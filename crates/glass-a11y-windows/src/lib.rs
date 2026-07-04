//! Windows accessibility backend for glass: reads the active app's UI Automation tree
//! and produces the platform-agnostic `glass_core::AxTree`. Implements the per-OS
//! `Accessibility` seam (orthogonal to the display `Platform` seam). Mirrors
//! `glass-a11y-linux`.

pub mod doctor;
pub mod mapping; // pure UIA->normalized mapping — cross-platform, unit-tested on the Linux dev box // checks()/a11y_checks() are OS-free (probe_uia is cfg-split), so its tests run on Linux

#[cfg(windows)]
mod reader;

#[cfg(windows)]
pub use reader::WindowsA11y;
