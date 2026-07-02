//! macOS process containment (Seatbelt) for glass, implementing `SandboxLevel::Default`/
//! `Strict`. The pure [`build_profile`] SBPL generator ([`profile`]) is cross-platform and
//! unit-tested on the Linux dev box; the `sandbox_init` FFI lands in a later task and is
//! macOS-only. Mirrors `glass-sandbox-linux`'s split (pure `wrap_argv` + OS mechanism).

pub mod profile;

pub use profile::{build_profile, ProfileOpts};
