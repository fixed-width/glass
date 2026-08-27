//! Captures the iOS Simulator screen as an RGBA `Frame`.
//!
//! `simctl io screenshot` only writes to a file, so a full-screen capture round-trips
//! through a temp PNG file that is then decoded to raw RGBA. Callers crop to a region
//! with [`glass_core::Frame::crop`].

use glass_core::{Deadline, Frame, GlassError, Result};

use crate::simctl::Simctl;

/// Capture the whole device screen as an RGBA `Frame` via `simctl io <udid> screenshot`.
///
/// Unlike the pure helpers in `device.rs`/`target.rs`, this needs a real simulator, so it
/// has no unit test — it is exercised by `IosPlatform` and covered by the on-simulator
/// integration suite instead.
pub fn screenshot(simctl: &Simctl, udid: &str) -> Result<Frame> {
    screenshot_by(simctl, udid, Deadline::UNBOUNDED)
}

/// Capture the whole device screen, bounding the `simctl` command by `deadline`.
pub fn screenshot_by(simctl: &Simctl, udid: &str, deadline: Deadline) -> Result<Frame> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("capture"));
    }
    let tmp = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(|e| GlassError::CaptureFailed(format!("temp file: {e}")))?;
    let path = tmp.path().to_string_lossy().into_owned();
    simctl.run_until(
        &["io", udid, "screenshot", "--type", "png", &path],
        deadline,
    )?;
    let bytes = std::fs::read(&path)
        .map_err(|e| GlassError::CaptureFailed(format!("read screenshot: {e}")))?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| GlassError::CaptureFailed(format!("decode PNG: {e}")))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    Frame::new(w, h, img.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simctl::FakeSimctl;
    use std::time::{Duration, Instant};

    #[test]
    fn ios_screenshot_uses_simctl_caller_deadline() {
        let fake = FakeSimctl::new();
        let simctl = Simctl::at(fake.program());

        let err = screenshot_by(
            &simctl,
            "test-udid",
            Deadline::at(Instant::now() - Duration::from_millis(1)),
        )
        .expect_err("a spent caller deadline must not start simctl");

        assert_eq!(err.bound(), Some(glass_core::BoundKind::NotStarted));
        assert!(fake.calls().is_empty(), "simctl ran despite the deadline");
    }
}
