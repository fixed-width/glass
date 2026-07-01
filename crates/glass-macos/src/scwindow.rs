//! `SCShareableContent` window discovery by pid.
//!
//! Polls ScreenCaptureKit's `SCShareableContent` enumeration — the same async
//! completion-handler API proven end-to-end in
//! `.superpowers/sdd/objc2-spike-report.md` Part A — for the first on-screen window
//! owned by one of a set of pids (the launched app's process set), following `ffi.rs`'s
//! documented async-bridge convention.
//!
//! ## Why this returns [`WindowMatch`], not `Retained<SCWindow>`
//!
//! The natural signature would return `(Retained<SCWindow>, WindowGeometry)`. That isn't
//! achievable safely: `SCShareableContent`'s completion handler fires on an internal
//! ScreenCaptureKit queue, not this function's calling thread, and `objc2`'s
//! `Retained<T>` is only `Send`/`Sync` when `T: Send + Sync`
//! (`unsafe impl<T: ?Sized + Sync + Send> Send for Retained<T> {}` in `objc2`'s
//! `rc::retained` module) — `SCWindow` (an `extern_class!`-declared binding with no such
//! bound, confirmed by reading the generated source: no `unsafe impl Send`/`Sync` for it
//! anywhere in `objc2-screen-capture-kit`) is neither. Apple never documents `SCWindow`'s
//! methods as safe to call concurrently from multiple threads, so `objc2` doesn't assert
//! it either. Smuggling a `Retained<SCWindow>` out of the completion block via a raw
//! pointer + `unsafe impl Send` wrapper would compile, but there'd be no real safety
//! argument backing it — exactly the gotcha `ffi.rs`'s module doc warns against ("never
//! send a `Retained<T>`/raw objc2 object across the channel").
//!
//! Instead, [`find_window_for_pids`] returns [`WindowMatch`]: the owning pid, the
//! `CGWindowID` (a plain `u32`, stable for the window's lifetime and re-findable via a
//! fresh query), and the geometry. That's everything a later capture call needs to
//! re-resolve the exact window — which it must do per-call anyway, since a
//! `Retained<SCWindow>` can't be cached across the same thread-crossing boundary.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{SCContentFilter, SCShareableContent, SCWindow};

use glass_core::platform::WindowGeometry;
use glass_core::{poll_until, GlassError, Result};

/// A discovered on-screen window: enough to re-find or capture it later without holding
/// a live `Retained<SCWindow>` across the completion handler's thread boundary (see
/// module doc).
// `geometry`/`scale`/`origin_pt` are read by `start_app` (via `backend.rs::discover_window`
// -> `query_once`); `pid`/`window_id` stay unread until Plan 4's list/select_window.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowMatch {
    /// The owning process's pid — one of the `pids` passed to `find_window_for_pids`.
    pub(crate) pid: i32,
    /// `SCWindow.windowID()` (`CGWindowID`, a `u32`) — stable for the window's lifetime;
    /// re-findable via a fresh `SCShareableContent` query
    /// (`content.windows().iter().find(|w| w.windowID() == id)`).
    pub(crate) window_id: u32,
    /// Window geometry in backing PIXELS (`contentRect.size * scale`, matching the frame
    /// `capture::capture_window` produces for this window) — the tool boundary's unit;
    /// see `coords.rs`'s module doc.
    pub(crate) geometry: WindowGeometry,
    /// `SCContentFilter.pointPixelScale()` for this window (`1.0` on a 1x display, `2.0`
    /// on 2x Retina) — carried alongside `geometry` so the backend can later map a PIXEL
    /// click coordinate back to a global POINT via `coords::pixel_to_global_point`.
    pub(crate) scale: f64,
    /// `contentRect.origin`, in POINTS (Quartz's global screen space) — the window origin
    /// `coords::pixel_to_global_point` adds a scaled pixel offset to.
    pub(crate) origin_pt: (f64, f64),
}

/// Poll `SCShareableContent` roughly every 100ms for the first on-screen window whose
/// `owningApplication().processID()` is in `pids`, until found or `timeout` elapses.
/// Multi-window selection (picking among several matches for the same app) is Plan 4;
/// this returns the first on-screen match.
///
/// Calls [`crate::ffi::app_kit_init`] first to establish the window-server connection —
/// required before any ScreenCaptureKit call from a bare CLI process (see `ffi.rs`).
/// Returns a classified error immediately (no point polling on a genuine
/// `SCShareableContent` failure — see [`crate::ffi::classify_null_result`]:
/// [`GlassError::PermissionDenied`] for a Screen Recording TCC decline,
/// [`GlassError::CaptureFailed`] for anything else) or [`GlassError::Timeout`] if no
/// matching window appears before `timeout` elapses.
// No production caller: `MacosPlatform::start_app` runs its own poll loop
// (`backend.rs::discover_window`) that alternates a single `query_once` attempt with
// `child.try_wait()` so a crashed launch fails fast with `AppExited` — this function's
// self-contained `poll_until` has no child handle to race against, so `start_app` can't
// delegate its whole timeout budget to it. Kept `pub(crate)` + allowed here (not
// deleted) as a plain "poll until found or timeout" primitive for a future call site
// with no child to check (e.g. Plan 4 window rediscovery).
#[allow(dead_code)]
pub(crate) fn find_window_for_pids(pids: &[i32], timeout: Duration) -> Result<WindowMatch> {
    crate::ffi::app_kit_init();

    let timeout_ms = timeout.as_millis() as u64;
    let outcome = poll_until(100, timeout_ms, || query_once(pids))?;
    outcome.value.ok_or(GlassError::Timeout(timeout_ms))
}

/// Find the first on-screen `SCWindow` in `content.windows()` owned by one of `pids`,
/// returning it alongside its owning pid. Shared by [`query_once`] (which extracts a
/// [`WindowMatch`] snapshot from the match, since the window itself can't survive the
/// completion handler's thread boundary — see the module doc) and
/// `capture::capture_window` (which needs the live `SCWindow` itself, still inside the
/// same completion-handler callback, to build an `SCContentFilter` from it). Keeping the
/// filter loop here means the two call sites can't drift apart on what "the target
/// window" means.
pub(crate) fn find_on_screen_window(
    content: &SCShareableContent,
    pids: &[i32],
) -> Option<(Retained<SCWindow>, i32)> {
    // SAFETY: `windows` is a plain getter on a live `SCShareableContent`; no other
    // preconditions.
    let windows: Retained<NSArray<SCWindow>> = unsafe { content.windows() };

    for w in windows.iter() {
        // SAFETY: `w` is a live `SCWindow` yielded by the array (`NSArray::iter` hands
        // out a fresh, owned `Retained<SCWindow>` per element — see `ffi.rs`'s gotcha
        // notes); this and the getters below have no preconditions beyond a valid
        // receiver.
        if !unsafe { w.isOnScreen() } {
            continue;
        }
        // SAFETY: same as above — a plain property getter.
        let owning_application = unsafe { w.owningApplication() };
        let Some(app) = owning_application else { continue };
        // SAFETY: same as above — a plain property getter.
        let pid = unsafe { app.processID() };
        if pids.contains(&pid) {
            return Some((w, pid));
        }
    }
    None
}

/// Cap on a single [`query_once`] attempt's wait for its `SCShareableContent` completion
/// handler. A query resolves in well under a second in the spike's observations, so this
/// is a wedged-handler backstop, not normal latency; kept small (rather than the old 5s)
/// so it can't itself eat much of the outer poll loop's `timeout_ms`/deadline budget on a
/// single bad tick — that budget is owned by the caller (`find_window_for_pids`'s
/// `poll_until`, or `backend.rs::discover_window`'s own loop), which retries (or times
/// out) regardless of this cap.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// One `SCShareableContent` round trip via the `RcBlock` -> `mpsc` bridge (`ffi.rs`'s
/// documented pattern): `Ok(Some(_))` on a match, `Ok(None)` if no matching on-screen
/// window exists yet (the outer poll should retry), `Err` if `SCShareableContent` itself
/// failed — classified via [`crate::ffi::classify_null_result`] (TCC decline ->
/// `PermissionDenied`, anything else -> `CaptureFailed`) rather than assumed to always be
/// a permission decline; not worth retrying either way.
pub(crate) fn query_once(pids: &[i32]) -> Result<Option<WindowMatch>> {
    let (tx, rx) = mpsc::channel::<QueryReply>();
    let pids_owned: Vec<i32> = pids.to_vec();

    // The completion handler does the whole match-or-not decision synchronously inside
    // the callback (per ffi.rs's async-bridge pattern) and only ever sends `QueryReply`
    // — plain owned data, `Send` regardless of what ObjC objects were touched to build
    // it — never a `Retained<SCWindow>` (see module doc).
    let block = RcBlock::new(move |content_ptr: *mut SCShareableContent, err_ptr: *mut NSError| {
        if content_ptr.is_null() {
            let err = crate::ffi::classify_null_result(
                err_ptr,
                "SCShareableContent completion handler returned null content and null error",
            );
            let _ = tx.send(QueryReply::Failed(err));
            return;
        }
        // SAFETY: `content_ptr` was just checked non-null; the framework guarantees it
        // points to a live `SCShareableContent` for the duration of this callback.
        let content: &SCShareableContent = unsafe { &*content_ptr };

        let Some((w, pid)) = find_on_screen_window(content, &pids_owned) else {
            let _ = tx.send(QueryReply::NotFound);
            return;
        };
        // SAFETY: `w` is a live `SCWindow` just resolved above; `capture.rs` uses this
        // same initializer on the same kind of live `SCWindow` — no other preconditions.
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w)
        };
        // SAFETY: `w`/`filter` are live; these are plain property getters with no other
        // preconditions.
        let (window_id, scale, content_rect) =
            unsafe { (w.windowID(), filter.pointPixelScale() as f64, filter.contentRect()) };
        let geometry = crate::coords::pixel_geometry_from_content_rect(
            content_rect.origin.x,
            content_rect.origin.y,
            content_rect.size.width,
            content_rect.size.height,
            scale,
        );
        let origin_pt = (content_rect.origin.x, content_rect.origin.y);
        let _ = tx.send(QueryReply::Found(WindowMatch { pid, window_id, geometry, scale, origin_pt }));
    });

    // SAFETY: `block` matches `getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler`'s
    // documented signature (`*mut SCShareableContent, *mut NSError`, per the generated
    // binding) — the exact sequence the spike proved end-to-end. The call itself has no
    // other preconditions.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }

    match rx.recv_timeout(QUERY_TIMEOUT) {
        Ok(QueryReply::Found(m)) => Ok(Some(m)),
        Ok(QueryReply::NotFound) => Ok(None),
        Ok(QueryReply::Failed(e)) => Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(GlassError::Backend(
            "SCShareableContent completion handler was dropped without replying".into(),
        )),
    }
}

/// One `SCShareableContent` query's outcome, funneled out of the completion block as
/// plain owned data (see module doc: never a `Retained<SCWindow>`).
enum QueryReply {
    Found(WindowMatch),
    NotFound,
    Failed(GlassError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_reply_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<QueryReply>();
    }
}
