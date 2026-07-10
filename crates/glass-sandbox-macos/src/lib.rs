//! macOS process containment (Seatbelt) for glass, implementing `SandboxLevel::Default`/
//! `Strict`. The pure [`build_profile`] SBPL generator ([`profile`]) is cross-platform and
//! unit-tested on the Linux dev box; the `sandbox_init` FFI ([`ffi`]) is macOS-only
//! mechanism. [`doctor`] is cross-platform: it reports `Ok` on macOS and `Unavailable`
//! elsewhere, so `glass doctor` can report on this crate from any host. Mirrors
//! `glass-sandbox-linux`'s split (pure `wrap_argv` + OS mechanism).

// FFI backend: the `sandbox_init` mechanism needs `unsafe`, so this crate opts out of the
// workspace `unsafe_code = "deny"`; each site carries a `// SAFETY:` note (see CLAUDE.md). The
// pure `profile`/`doctor` modules stay `unsafe`-free by convention.
#![allow(unsafe_code)]

pub mod doctor;
pub mod profile;

pub use doctor::{availability, checks, Availability};
pub use profile::{build_profile, ProfileOpts};

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
pub use ffi::apply_cstr;
