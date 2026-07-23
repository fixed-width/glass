//! Test-only helper: a private Xvfb for integration tests. Delegates to the
//! production `glass_x11::Xvfb` so there is one implementation; this wrapper just
//! supplies the test screen size and panics on failure (what tests expect).

#![allow(dead_code)]

use std::ops::Deref;

// Shared body for the X11/Wayland ignore-regions MCP end-to-end tests. Only pulled in by the
// test binaries that declare `mod common;` and actually call it
// (ignore_regions_e2e.rs / wayland_ignore_regions_e2e.rs); this file's `#![allow(dead_code)]`
// covers it for the other `mod common;` binaries (integration.rs, network.rs, harness_smoke.rs)
// that don't.
pub mod mcp_ignore;

// Verification-loop cost benchmark harness. Like `mcp_ignore` above, `pub mod mcp_cost;`
// compiles this into every test binary that declares `mod common;` (integration.rs,
// network.rs, harness_smoke.rs, ignore_regions_e2e.rs, wayland_ignore_regions_e2e.rs,
// verification_cost.rs) — only verification_cost.rs actually calls it, but its
// `#[cfg(test)]` unit tests run under `cargo test --workspace` regardless of which binary
// pulls them in; this file's `#![allow(dead_code)]` covers the runtime-unused public API
// for the other binaries.
pub mod mcp_cost;

pub struct Xvfb(glass_x11::Xvfb);

impl Xvfb {
    /// Start a private Xvfb for tests (panics on failure, failing the test).
    pub fn start() -> Xvfb {
        Xvfb(glass_x11::Xvfb::start("1024x768x24").expect("spawn Xvfb (is it installed?)"))
    }
}

impl Deref for Xvfb {
    type Target = glass_x11::Xvfb;
    fn deref(&self) -> &glass_x11::Xvfb {
        &self.0
    }
}
