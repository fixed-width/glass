//! Captures the iOS Simulator screen as an RGBA `Frame`.
//!
//! `simctl io screenshot` only writes to a file, so a full-screen capture round-trips
//! through a temp PNG file that is then decoded to raw RGBA. Callers crop to a region
//! with [`glass_core::Frame::crop`].

use glass_core::{Deadline, Frame, GlassError, Region, Result, Whose};

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
    run_simctl_screenshot_with(simctl, udid, &path, deadline, || {})?;
    let bytes = read_screenshot_with(&path, deadline, || {})?;
    decode_png_with(&bytes, deadline, || {})
}

fn read_screenshot_with(
    path: &str,
    deadline: Deadline,
    after_read: impl FnOnce(),
) -> Result<Vec<u8>> {
    require_capture_time(deadline, true)?;
    let result =
        std::fs::read(path).map_err(|e| GlassError::CaptureFailed(format!("read screenshot: {e}")));
    after_read();
    finish_capture(deadline, true, result)
}

fn run_simctl_screenshot_with(
    simctl: &Simctl,
    udid: &str,
    path: &str,
    deadline: Deadline,
    after_run: impl FnOnce(),
) -> Result<()> {
    require_capture_time(deadline, false)?;
    let result = simctl
        .run_until(&["io", udid, "screenshot", "--type", "png", path], deadline)
        .map(|_| ());
    after_run();
    finish_capture(deadline, true, result)
}

fn decode_png_with(bytes: &[u8], deadline: Deadline, after_decode: impl FnOnce()) -> Result<Frame> {
    require_capture_time(deadline, true)?;
    let result = image::load_from_memory(bytes)
        .map_err(|e| GlassError::CaptureFailed(format!("decode PNG: {e}")))
        .and_then(|img| {
            let img = img.to_rgba8();
            Frame::new(img.width(), img.height(), img.into_raw())
        });
    after_decode();
    finish_capture(deadline, true, result)
}

fn require_capture_time(deadline: Deadline, dispatched: bool) -> Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    Err(if dispatched {
        GlassError::caller_deadline_elapsed("iOS capture")
    } else {
        GlassError::deadline_not_started("iOS capture")
    })
}

fn finish_capture<T>(deadline: Deadline, dispatched: bool, result: Result<T>) -> Result<T> {
    // Subprocess bounds resolve ownership before kill/reap, whose cleanup may outlive the caller.
    if result
        .as_ref()
        .is_err_and(|error| error.bound_owner() == Some(Whose::Callee))
    {
        return result;
    }
    require_capture_time(deadline, dispatched)?;
    result
}

pub(crate) fn crop_frame_by(
    frame: Frame,
    region: Option<&Region>,
    deadline: Deadline,
) -> Result<Frame> {
    crop_frame_with(frame, region, deadline, || {})
}

fn crop_frame_with(
    frame: Frame,
    region: Option<&Region>,
    deadline: Deadline,
    after_crop: impl FnOnce(),
) -> Result<Frame> {
    require_capture_time(deadline, true)?;
    let result = match region {
        None => Ok(frame),
        Some(region) => frame.crop(region),
    };
    after_crop();
    finish_capture(deadline, true, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simctl::FakeSimctl;
    use std::time::{Duration, Instant};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode a PNG");
        bytes
    }

    #[test]
    fn png_decode_finishing_after_the_caller_deadline_is_not_success() {
        let deadline = Deadline::from_millis(100);
        let decoded = std::cell::Cell::new(false);

        let error = decode_png_with(&png(2, 2), deadline, || {
            decoded.set(true);
            while !deadline.has_passed() {
                std::thread::yield_now();
            }
        })
        .expect_err("a decoded frame observed after the absolute deadline must fail");

        assert!(decoded.get(), "decode did not finish before the deadline");
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn png_decode_error_observed_after_the_caller_deadline_is_a_caller_timeout() {
        let deadline = Deadline::from_millis(100);
        let decoded = std::cell::Cell::new(false);

        let error = decode_png_with(b"not a PNG", deadline, || {
            decoded.set(true);
            while !deadline.has_passed() {
                std::thread::yield_now();
            }
        })
        .expect_err("an ordinary decode error observed late must yield to the caller deadline");

        assert!(decoded.get(), "the decode result was not finalized");
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn crop_finishing_after_the_caller_deadline_is_not_success() {
        let frame = Frame::new(2, 2, vec![0; 16]).expect("valid frame");
        let region = Region {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let deadline = Deadline::from_millis(100);
        let cropped = std::cell::Cell::new(false);

        let error = crop_frame_with(frame, Some(&region), deadline, || {
            cropped.set(true);
            while !deadline.has_passed() {
                std::thread::yield_now();
            }
        })
        .expect_err("a cropped frame observed after the absolute deadline must fail");

        assert!(cropped.get(), "crop did not finish before the deadline");
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn simctl_success_observed_after_the_caller_deadline_starts_no_file_read() {
        let fake = FakeSimctl::new();
        let simctl = Simctl::at(fake.program());
        let output = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("create screenshot destination");
        let path = output.path().to_string_lossy().into_owned();
        let deadline = Deadline::from_millis(200);
        let after_run = std::cell::Cell::new(false);

        let error = run_simctl_screenshot_with(&simctl, "test-udid", &path, deadline, || {
            after_run.set(true);
            while !deadline.has_passed() {
                std::thread::yield_now();
            }
        })
        .expect_err("a simctl answer observed after the absolute deadline must fail");

        assert!(
            after_run.get(),
            "simctl itself did not answer before expiry"
        );
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn backend_screenshot_timeout_survives_caller_expiry_before_finalization() {
        let mut screenshot = std::process::Command::new("/bin/sleep");
        screenshot.arg("30");
        let result = glass_core::run_bounded(
            &mut screenshot,
            Duration::from_millis(5),
            "simctl:io screenshot",
        )
        .map(|_| ());
        let backend_error = result
            .as_ref()
            .expect_err("the screenshot stand-in must reach its backend ceiling");
        assert_eq!(backend_error.bound_owner(), Some(glass_core::Whose::Callee));

        let deadline = Deadline::from_millis(1);
        while !deadline.has_passed() {
            std::thread::yield_now();
        }
        let error = finish_capture(deadline, true, result)
            .expect_err("caller expiry must not replace a resolved backend timeout");

        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Callee));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn screenshot_read_finishing_after_the_caller_deadline_starts_no_decode() {
        let file = tempfile::NamedTempFile::new().expect("create screenshot file");
        std::fs::write(file.path(), png(2, 2)).expect("write screenshot bytes");
        let path = file.path().to_string_lossy().into_owned();
        let deadline = Deadline::from_millis(100);
        let read = std::cell::Cell::new(false);

        let error = read_screenshot_with(&path, deadline, || {
            read.set(true);
            while !deadline.has_passed() {
                std::thread::yield_now();
            }
        })
        .expect_err("a completed file read observed after the absolute deadline must fail");

        assert!(read.get(), "file read did not finish before the deadline");
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn ios_screenshot_uses_simctl_caller_deadline() {
        let fake = FakeSimctl::new();
        let simctl = Simctl::at(fake.program());
        fake.slow("io", 30);

        let at = Instant::now();
        let err = screenshot_by(
            &simctl,
            "test-udid",
            Deadline::at(Instant::now() + Duration::from_millis(300)),
        )
        .expect_err("a live caller deadline must bound simctl");

        assert!(
            at.elapsed() < Duration::from_secs(2),
            "waited {:?}: {err}",
            at.elapsed()
        );
        assert_eq!(err.bound(), Some(glass_core::BoundKind::TimedOut));
        assert!(fake.called("io test-udid screenshot"), "{:?}", fake.calls());
    }
}
