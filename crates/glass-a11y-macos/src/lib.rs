//! macOS accessibility-tree reader for glass (AXUIElement behind glass-core's Accessibility seam).

pub mod mapping; // pure AX->normalized mapping — cross-platform, unit-tested on the Linux dev box

// `ffi`/`reader` are added in Task 4 (the cfg(macos) AXUIElement reader itself); until then
// this crate only ships the pure mapping module above, which builds on every OS.
// #[cfg(target_os = "macos")]
// mod ffi;
// #[cfg(target_os = "macos")]
// mod reader;
// #[cfg(target_os = "macos")]
// pub use reader::MacosA11y;
