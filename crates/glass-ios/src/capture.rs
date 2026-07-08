//! Captures the iOS Simulator screen as an RGBA `Frame`.
//!
//! `simctl io screenshot` only writes to a file, so a full-screen capture round-trips
//! through a temp PNG file that is then decoded to raw RGBA. Callers crop to a region
//! with [`glass_core::Frame::crop`].

use glass_core::{Frame, GlassError, Result};

use crate::simctl::Simctl;

/// Capture the whole device screen as an RGBA `Frame` via `simctl io <udid> screenshot`.
///
/// Not yet called outside tests (nothing wires a `Simctl`/UDID pair to it yet) and, unlike
/// the pure helpers in `device.rs`/`target.rs`, it needs a real simulator so it has no unit
/// test either — an unconditional `allow` is required rather than the `cfg_attr(not(test),
/// ...)` used elsewhere in this crate for items a unit test does exercise. `expect` (rather
/// than `allow`) keeps this self-removing: it will itself fail the lint gate once a caller
/// wires this in and the dead-code warning stops firing.
#[expect(
    dead_code,
    reason = "not wired in yet; no real simulator in unit tests"
)]
pub fn screenshot(simctl: &Simctl, udid: &str) -> Result<Frame> {
    let tmp = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(|e| GlassError::CaptureFailed(format!("temp file: {e}")))?;
    let path = tmp.path().to_string_lossy().into_owned();
    simctl.run(&["io", udid, "screenshot", "--type", "png", &path])?;
    let bytes = std::fs::read(&path)
        .map_err(|e| GlassError::CaptureFailed(format!("read screenshot: {e}")))?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| GlassError::CaptureFailed(format!("decode PNG: {e}")))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    Frame::new(w, h, img.into_raw())
}
