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
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{SCShareableContent, SCWindow};

use glass_core::platform::WindowGeometry;
use glass_core::{poll_until, GlassError, Result};

use crate::permissions::Permission;

/// A discovered on-screen window: enough to re-find or capture it later without holding
/// a live `Retained<SCWindow>` across the completion handler's thread boundary (see
/// module doc).
// Fields aren't read yet: no call site consumes `find_window_for_pids`'s result until a
// later task wires it into `MacosPlatform` (capture / `start_app`). Allowed here rather
// than deleted so the type and its fields land in one place instead of being
// reintroduced per call site — same convention as `ffi.rs`'s `app_kit_init`.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowMatch {
    /// The owning process's pid — one of the `pids` passed to `find_window_for_pids`.
    pub(crate) pid: i32,
    /// `SCWindow.windowID()` (`CGWindowID`, a `u32`) — stable for the window's lifetime;
    /// re-findable via a fresh `SCShareableContent` query
    /// (`content.windows().iter().find(|w| w.windowID() == id)`).
    pub(crate) window_id: u32,
    pub(crate) geometry: WindowGeometry,
}

/// Poll `SCShareableContent` roughly every 100ms for the first on-screen window whose
/// `owningApplication().processID()` is in `pids`, until found or `timeout` elapses.
/// Multi-window selection (picking among several matches for the same app) is Plan 4;
/// this returns the first on-screen match.
///
/// Calls [`crate::ffi::app_kit_init`] first to establish the window-server connection —
/// required before any ScreenCaptureKit call from a bare CLI process (see `ffi.rs`).
/// Returns [`GlassError::PermissionDenied`] immediately (no point polling — the grant
/// isn't going to appear mid-poll) if `SCShareableContent` itself reports a TCC decline,
/// or [`GlassError::Timeout`] if no matching window appears before `timeout` elapses.
// Not yet called: a later task wires this into `MacosPlatform::start_app`/capture (per
// the task brief, Task 3/5 re-discovers the window per capture). Kept `pub(crate)` +
// allowed here rather than deleted, mirroring `ffi.rs`'s `app_kit_init` convention, so
// the discovery logic and its tests land in one place instead of being reintroduced per
// call site.
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

/// One `SCShareableContent` round trip via the `RcBlock` -> `mpsc` bridge (`ffi.rs`'s
/// documented pattern): `Ok(Some(_))` on a match, `Ok(None)` if no matching on-screen
/// window exists yet (the outer poll should retry), `Err` if `SCShareableContent` itself
/// failed (e.g. a TCC decline — not worth retrying).
fn query_once(pids: &[i32]) -> Result<Option<WindowMatch>> {
    let (tx, rx) = mpsc::channel::<QueryReply>();
    let pids_owned: Vec<i32> = pids.to_vec();

    // The completion handler does the whole match-or-not decision synchronously inside
    // the callback (per ffi.rs's async-bridge pattern) and only ever sends `QueryReply`
    // — plain owned data, `Send` regardless of what ObjC objects were touched to build
    // it — never a `Retained<SCWindow>` (see module doc).
    let block = RcBlock::new(move |content_ptr: *mut SCShareableContent, err_ptr: *mut NSError| {
        if content_ptr.is_null() {
            let msg = if err_ptr.is_null() {
                "SCShareableContent completion handler returned null content and null error"
                    .to_string()
            } else {
                // SAFETY: the framework guarantees a non-null, valid `NSError` whenever
                // it hands back null content (proven in the spike's TCC-declined run).
                let err: &NSError = unsafe { &*err_ptr };
                format!("{err:?}")
            };
            let _ = tx.send(QueryReply::Failed(msg));
            return;
        }
        // SAFETY: `content_ptr` was just checked non-null; the framework guarantees it
        // points to a live `SCShareableContent` for the duration of this callback.
        let content: &SCShareableContent = unsafe { &*content_ptr };

        let Some((w, pid)) = find_on_screen_window(content, &pids_owned) else {
            let _ = tx.send(QueryReply::NotFound);
            return;
        };
        // SAFETY: `w` was just resolved live from `content.windows()` above; these are
        // plain property getters with no other preconditions.
        let (window_id, frame) = unsafe { (w.windowID(), w.frame()) };
        let geometry = geometry_from_rect(
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        );
        let _ = tx.send(QueryReply::Found(WindowMatch { pid, window_id, geometry }));
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

    // A single query resolves in well under a second in the spike's observations; cap it
    // generously so a wedged completion handler can't block this attempt forever. The
    // outer `poll_until` in `find_window_for_pids` owns the real timeout budget and will
    // retry (or surface `GlassError::Timeout`) regardless of this cap.
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(QueryReply::Found(m)) => Ok(Some(m)),
        Ok(QueryReply::NotFound) => Ok(None),
        Ok(QueryReply::Failed(msg)) => Err(Permission::ScreenRecording.denied_with_detail(msg)),
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
    Failed(String),
}

/// Convert a window frame (points, from `SCWindow.frame()`) to the platform-agnostic
/// `WindowGeometry`. Pulled out as pure `f64` math (no `CGRect` dependency) so it can
/// carry its own unit test without needing a live window.
fn geometry_from_rect(x: f64, y: f64, width: f64, height: f64) -> WindowGeometry {
    WindowGeometry {
        x: x.round() as i32,
        y: y.round() as i32,
        width: width.round().max(0.0) as u32,
        height: height.round().max(0.0) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_from_rect_rounds_to_nearest() {
        assert_eq!(
            geometry_from_rect(10.4, 20.6, 300.49, 200.5),
            WindowGeometry { x: 10, y: 21, width: 300, height: 201 }
        );
    }

    #[test]
    fn geometry_from_rect_clamps_negative_size_to_zero() {
        // A real CGRect from SCWindow.frame() never has a negative size, but the
        // conversion must not panic or wrap on malformed input.
        assert_eq!(
            geometry_from_rect(0.0, 0.0, -1.0, -1.0),
            WindowGeometry { x: 0, y: 0, width: 0, height: 0 }
        );
    }

    #[test]
    fn geometry_from_rect_preserves_negative_origin() {
        // A window can sit left-of/above the primary display's origin in a multi-monitor
        // layout; x/y must stay signed rather than clamping like width/height do.
        assert_eq!(
            geometry_from_rect(-50.0, -10.0, 100.0, 80.0),
            WindowGeometry { x: -50, y: -10, width: 100, height: 80 }
        );
    }
}
