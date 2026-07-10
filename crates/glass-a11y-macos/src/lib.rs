//! macOS accessibility-tree reader for glass (AXUIElement behind glass-core's Accessibility seam).

// FFI backend: the AXUIElement reader needs `unsafe`, so this crate opts out of the workspace
// `unsafe_code = "deny"`; each site carries a `// SAFETY:` note (see CLAUDE.md). The pure
// `mapping` module stays `unsafe`-free by convention.
#![allow(unsafe_code)]

pub mod mapping; // pure AX->normalized mapping — cross-platform, unit-tested on the Linux dev box

// The cfg(macos) AXUIElement reader: `ffi` holds every `unsafe` AX read primitive, `reader`
// the `unsafe`-free root selection + pre-order walk behind glass-core's `Accessibility`
// seam. Both are gated off non-macOS, where only the pure `mapping` module above compiles.
#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod reader;
#[cfg(target_os = "macos")]
pub use reader::MacosA11y;
