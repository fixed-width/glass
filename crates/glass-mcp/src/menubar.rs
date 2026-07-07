//! macOS menu-bar mode: GlassMcp.app as a visible NSStatusItem that serves MCP
//! on a background task. The daemon is never hidden — the thing serving is the
//! thing showing `glass ●` in the menu bar, with the endpoint and Quit. The
//! macOS implementation lands in a later task; this is the entry point.
//!
//! `#[cfg(feature = "network")]` at the module boundary (see `lib.rs`): `run` takes an
//! already-resolved [`ServeConfig`], which only exists when the network transport is
//! compiled in — matches `crate::serve`, which is gated the same way.
use crate::serve::config::ServeConfig;

#[cfg(target_os = "macos")]
pub fn run(_cfg: ServeConfig) -> anyhow::Result<()> {
    // Implemented in the menu-bar app task.
    anyhow::bail!("menu-bar app not yet implemented")
}

#[cfg(not(target_os = "macos"))]
pub fn run(_cfg: ServeConfig) -> anyhow::Result<()> {
    anyhow::bail!("the menu-bar app is macOS-only")
}
