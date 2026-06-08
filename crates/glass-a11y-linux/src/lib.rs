//! Linux accessibility backend for glass: reads the target app's AT-SPI tree over
//! D-Bus and produces the platform-agnostic `glass_core::AxTree`. Implements the
//! per-OS `Accessibility` seam (orthogonal to the display `Platform` seam); the
//! same impl serves both the X11 and Wayland display backends.

mod mapping;
mod reader;

pub mod doctor;

pub use reader::LinuxA11y;
