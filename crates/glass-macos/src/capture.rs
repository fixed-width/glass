//! ScreenCaptureKit per-window capture → RGBA8 `Frame`.
//!
//! Re-resolves the target window fresh on every call — a `Retained<SCWindow>` can't be
//! held across calls or across the completion-handler's thread boundary (see
//! `scwindow.rs`'s module doc) — via the nested async flow proven end-to-end in the objc2
//! spike (`.superpowers/sdd/objc2-spike-report.md` Part A):
//! `SCShareableContent` → [`crate::scwindow::find_on_screen_window`] →
//! `SCContentFilter::initWithDesktopIndependentWindow` → `SCStreamConfiguration` →
//! `SCScreenshotManager::captureImageWithFilter_configuration_completionHandler` →
//! `CGImage`, drawn into a tightly-packed RGBA8 bitmap context inside the innermost
//! completion block. Per `ffi.rs`'s async-bridge convention, only plain owned/`Send` data
//! (the finished [`Frame`]) ever crosses back out of a completion handler — never a
//! `Retained<_>`/`CGImage` pointer.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};

use glass_core::frame::{Frame, Region};
use glass_core::{GlassError, Result};

use crate::permissions::Permission;
use crate::scwindow::find_on_screen_window;

/// Max wait for one capture round trip (`SCShareableContent` + `SCScreenshotManager`
/// both completing). Generous relative to the spike's sub-second observations — this
/// covers a wedged completion handler, not normal latency.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// `SCStreamErrorDomain`'s code for a declined Screen Recording TCC grant — observed
/// verbatim in the spike's TCC-declined run (`.superpowers/sdd/objc2-spike-report.md`).
const TCC_DECLINE_CODE: isize = -3801;

/// Capture the first on-screen window owned by one of `pids` as an RGBA8 [`Frame`],
/// optionally cropped to a window-relative `region`. Returns
/// [`GlassError::WindowNotFound`] if no on-screen window is owned by `pids`,
/// [`GlassError::PermissionDenied`] on a Screen Recording TCC decline, or
/// [`GlassError::CaptureFailed`] for any other ScreenCaptureKit failure.
// Not yet called: `MacosPlatform::capture_frame` still `unimplemented!()` until a later
// task wires `start_app`/`capture_frame` to `process::spawn` + this function. Kept
// `pub(crate)` + allowed here rather than deleted, mirroring `ffi.rs`'s `app_kit_init`
// and `scwindow.rs`'s `find_window_for_pids` convention, so the capture logic lands in
// one place instead of being reintroduced per call site.
#[allow(dead_code)]
pub(crate) fn capture_window(pids: &[i32], region: Option<&Region>) -> Result<Frame> {
    crate::ffi::app_kit_init();

    let (tx, rx) = mpsc::channel::<CaptureReply>();
    let pids_owned: Vec<i32> = pids.to_vec();
    let region_owned = region.copied();

    let content_block = RcBlock::new(
        move |content_ptr: *mut SCShareableContent, err_ptr: *mut NSError| {
            if content_ptr.is_null() {
                let err = classify_null_result(
                    err_ptr,
                    "SCShareableContent completion handler returned null content and null error",
                );
                let _ = tx.send(CaptureReply::Err(err));
                return;
            }
            // SAFETY: `content_ptr` was just checked non-null; the framework guarantees
            // it points to a live `SCShareableContent` for the duration of this callback.
            let content: &SCShareableContent = unsafe { &*content_ptr };

            let Some((window, _pid)) = find_on_screen_window(content, &pids_owned) else {
                let _ = tx.send(CaptureReply::Err(GlassError::WindowNotFound));
                return;
            };

            // SAFETY: `window` is a live `SCWindow` just resolved above;
            // `initWithDesktopIndependentWindow:` has no other preconditions.
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(
                    SCContentFilter::alloc(),
                    &window,
                )
            };
            // SAFETY: plain no-arg initializer; no preconditions.
            let config = unsafe { SCStreamConfiguration::new() };
            // SAFETY: plain property getters on the freshly constructed `filter`.
            let (scale, content_rect) =
                unsafe { (filter.pointPixelScale() as f64, filter.contentRect()) };
            let width = ((content_rect.size.width * scale) as usize).max(1);
            let height = ((content_rect.size.height * scale) as usize).max(1);
            // SAFETY: plain property setters on the freshly constructed, uniquely-owned
            // `config`; no other preconditions.
            unsafe {
                config.setWidth(width);
                config.setHeight(height);
                config.setShowsCursor(false);
            }

            let tx_img = tx.clone();
            let image_block =
                RcBlock::new(move |image_ptr: *mut CGImage, err_ptr: *mut NSError| {
                    if image_ptr.is_null() {
                        let err = classify_null_result(
                            err_ptr,
                            "SCScreenshotManager.captureImage returned null image and null error",
                        );
                        let _ = tx_img.send(CaptureReply::Err(err));
                        return;
                    }
                    // SAFETY: `image_ptr` was just checked non-null; the framework
                    // guarantees it points to a live `CGImage` for the duration of this
                    // callback. All work on it happens synchronously right here — the
                    // `CGImage` itself never leaves this block.
                    let image: &CGImage = unsafe { &*image_ptr };
                    let result = rgba_frame_from_cgimage(image)
                        .and_then(|frame| crop_to_region(frame, region_owned.as_ref()));
                    let _ = tx_img.send(match result {
                        Ok(frame) => CaptureReply::Ok(frame),
                        Err(e) => CaptureReply::Err(e),
                    });
                });

            // SAFETY: `image_block` matches
            // `captureImageWithFilter:configuration:completionHandler:`'s documented
            // signature (`*mut CGImage, *mut NSError`, per the generated binding) — the
            // exact sequence the spike proved end-to-end. The call itself has no other
            // preconditions.
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &config,
                    Some(&image_block),
                );
            }
        },
    );

    // SAFETY: `content_block` matches
    // `getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:`'s
    // documented signature (`*mut SCShareableContent, *mut NSError`) — the same call
    // `scwindow.rs`'s `query_once` proved. The call itself has no other preconditions.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &content_block,
        );
    }

    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(CaptureReply::Ok(frame)) => Ok(frame),
        Ok(CaptureReply::Err(e)) => Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(GlassError::CaptureFailed(
            "ScreenCaptureKit capture timed out waiting for the completion handler".into(),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(GlassError::Backend(
            "ScreenCaptureKit completion handler was dropped without replying".into(),
        )),
    }
}

/// A capture round trip's outcome, funneled out of the innermost completion block as
/// plain owned data (per `ffi.rs`'s async-bridge convention: never a `Retained<T>`/raw
/// objc2 object — the finished `Frame`'s bytes are built entirely inside the callback).
enum CaptureReply {
    Ok(Frame),
    Err(GlassError),
}

/// Crop `frame` to `region` (window-relative), clamping the region to the captured frame
/// first via [`crate::coords::clamp_region`] (defense in depth — the session layer should
/// already validate the region against the window before it reaches the backend, per
/// `glass_core::frame::Region::check_fits`'s doc). `None` returns `frame` unchanged.
fn crop_to_region(frame: Frame, region: Option<&Region>) -> Result<Frame> {
    let Some(r) = region else { return Ok(frame) };
    let clamped = crate::coords::clamp_region(
        r.x as i32,
        r.y as i32,
        r.width,
        r.height,
        frame.width,
        frame.height,
    );
    frame.crop(&clamped)
}

/// Draw a captured `CGImage` into a tightly-packed RGBA8 (premultiplied-last, host byte
/// order) bitmap context and hand back the raw bytes as a [`Frame`] — the spike's
/// `analyze_and_write` bitmap path, minus the luma-sampling/PNG-writing (capture only
/// needs the pixels). `CGContextDrawImage`'s internal colorspace conversion means this
/// yields tightly-packed RGBA bytes directly regardless of the source image's own pixel
/// format (BGRA for SDR captures, per `SCScreenshotManager`'s docs) — no separate swizzle
/// needed.
fn rgba_frame_from_cgimage(image: &CGImage) -> Result<Frame> {
    let w = CGImage::width(Some(image));
    let h = CGImage::height(Some(image));
    if w == 0 || h == 0 {
        return Err(GlassError::CaptureFailed(format!(
            "captured image has zero dimensions ({w}x{h})"
        )));
    }

    let bytes_per_row = w * 4;
    let mut buf = vec![0u8; bytes_per_row * h];
    let color_space = CGColorSpace::new_device_rgb()
        .ok_or_else(|| GlassError::CaptureFailed("CGColorSpaceCreateDeviceRGB failed".into()))?;
    // kCGImageAlphaPremultipliedLast, host byte order (0) — matches the spike's proven
    // config (glass never depends on the alpha channel; opaque windows read back 255).
    let bitmap_info = CGImageAlphaInfo::PremultipliedLast.0;
    // SAFETY: `buf` is a valid, uniquely-owned `bytes_per_row * h`-byte buffer, sized
    // exactly for `w`/`h`/`bytes_per_row`; `CGBitmapContextCreate` writes directly into
    // it rather than copying, so `buf` holds the drawn pixels once `draw_image` returns
    // below. `buf` is Rust-owned heap memory, not freed by `ctx`'s `CFRelease` — its
    // validity doesn't depend on `ctx`'s lifetime. `color_space` is a live `CGColorSpace`.
    let ctx = unsafe {
        CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut _,
            w,
            h,
            8,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
        )
    }
    .ok_or_else(|| GlassError::CaptureFailed("CGBitmapContextCreate failed".into()))?;

    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize { width: w as f64, height: h as f64 },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(image));

    Frame::new(w as u32, h as u32, buf)
}

/// Classify a `null` ScreenCaptureKit result's paired `NSError`: [`GlassError::PermissionDenied`]
/// for a Screen Recording TCC decline (domain `SCStreamErrorDomain`, code `-3801`, and/or
/// a "declined" description — the spike observed all three together, but any one is
/// treated as authoritative since Apple doesn't document which fields are stable across
/// OS versions), [`GlassError::CaptureFailed`] otherwise. `fallback_msg` covers the
/// (framework-contract-violating, but defensively handled) case where both the result and
/// the error came back null.
fn classify_null_result(err_ptr: *mut NSError, fallback_msg: &str) -> GlassError {
    if err_ptr.is_null() {
        return GlassError::CaptureFailed(fallback_msg.to_string());
    }
    // SAFETY: the framework guarantees a non-null, valid `NSError` whenever it hands back
    // a null content/image — proven in the spike's TCC-declined run (see `scwindow.rs`'s
    // identical precondition).
    let err: &NSError = unsafe { &*err_ptr };
    let domain = err.domain().to_string();
    let code = err.code();
    let description = err.localizedDescription().to_string();
    let detail = format!("{domain} (code {code}): {description}");

    let is_tcc_decline = domain.contains("SCStreamErrorDomain")
        || code == TCC_DECLINE_CODE
        || description.to_lowercase().contains("declined");
    if is_tcc_decline {
        Permission::ScreenRecording.denied_with_detail(detail)
    } else {
        GlassError::CaptureFailed(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_reply_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CaptureReply>();
    }
}
