//! `SCShareableContent` window discovery by pid, and by `CGWindowID` (active-window
//! retargeting).
//!
//! Polls ScreenCaptureKit's `SCShareableContent` enumeration for the first on-screen window
//! owned by one of a set of pids ([`find_window_for_pids`], the launched app's process set) or
//! for the specific on-screen window with a given `CGWindowID` that is *also* owned by one of
//! that same pid set ([`find_window_by_id`]) — window ids are not namespaced per app, so the
//! pid scoping closes a silent-wrong-target hole a bare `CGWindowID` match would open. Follows
//! `ffi.rs`'s documented async-bridge convention.
//!
//! ## Why this returns [`WindowMatch`], not `Retained<SCWindow>`
//!
//! `SCShareableContent`'s completion handler fires on an internal ScreenCaptureKit queue, not
//! the calling thread, and `objc2`'s `Retained<T>` is only `Send`/`Sync` when `T: Send + Sync`
//! (`unsafe impl<T: ?Sized + Sync + Send> Send for Retained<T> {}` in `objc2`'s `rc::retained`
//! module). `SCWindow` is neither: `objc2-screen-capture-kit` declares it via `extern_class!`
//! with no such bound, and Apple never documents its methods as safe to call concurrently.
//! Smuggling one out of the completion block via a raw pointer + `unsafe impl Send` wrapper
//! compiles with no safety argument behind it — the gotcha `ffi.rs`'s module doc warns against
//! ("never send a `Retained<T>`/raw objc2 object across the channel").
//!
//! Instead, [`find_window_for_pids`] returns [`WindowMatch`]: the owning pid, the `CGWindowID`
//! (a plain `u32`, stable for the window's lifetime and re-findable via a fresh query), and the
//! geometry — everything a later capture call needs to re-resolve the exact window, which it
//! must do per-call anyway.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCRunningApplication, SCShareableContent, SCWindow,
};

use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result, poll_until};

use crate::adoption_log::CandidateWindow;

/// A discovered on-screen window: enough to re-find or capture it later without holding
/// a live `Retained<SCWindow>` across the completion handler's thread boundary (see
/// module doc).
// Every field is read: `geometry`/`scale`/`origin_pt` by `start_app` and the per-call window
// resolution, `window_id` to seed `MacosPlatform::active_window`, and `pid` as the CGEvent
// focus/AX-scoping target and for each `find_window_by_id` call site's pid-scoping check.
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

/// A discovered on-screen window, as returned by [`list_app_windows`]: the `CGWindowID`, pixel
/// geometry, title, and owning application name — everything `backend::list_windows` needs to
/// build a `WindowInfo` per window. `title`/`application_name` are read out as owned `String`s
/// inside the completion block, same rationale as [`WindowMatch`] (see the module doc).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AppWindow {
    /// `SCWindow.windowID()` (`CGWindowID`) — becomes `WindowInfo.id`.
    pub(crate) window_id: u32,
    /// Window geometry in backing PIXELS, same derivation as [`WindowMatch::geometry`].
    pub(crate) geometry: WindowGeometry,
    /// `SCWindow.title()` — `None` when the window has no title (e.g. a borderless
    /// utility window) or the title wasn't retrievable.
    pub(crate) title: Option<String>,
    /// `SCWindow.owningApplication().applicationName()` — becomes `WindowInfo.class`.
    /// `None` only if the window has no owning application by the time this is read
    /// (defensive; `list_app_windows` already filters to windows with an owning
    /// application, since that's how it matches on pid).
    pub(crate) application_name: Option<String>,
}

/// Poll `SCShareableContent` roughly every 100ms for the first on-screen window whose
/// `owningApplication().processID()` is in `pids`, until found or `timeout` elapses.
///
/// Calls [`crate::ffi::app_kit_init`] first to establish the window-server connection —
/// required before any ScreenCaptureKit call from a bare CLI process (see `ffi.rs`).
/// Returns a classified error immediately (no point polling on a genuine
/// `SCShareableContent` failure — see [`crate::ffi::classify_null_result`]:
/// [`GlassError::PermissionDenied`] for a Screen Recording TCC decline,
/// [`GlassError::CaptureFailed`] for anything else) or [`GlassError::Timeout`] if no
/// matching window appears before `timeout` elapses.
///
/// `MacosPlatform::start_app` can't use this: it runs its own poll loop
/// (`backend.rs::discover_window`) alternating a single `query_once_with_candidates` attempt
/// with `child.try_wait()`, and this function's self-contained `poll_until` has no child handle
/// to race against. `MacosPlatform::send_pointer` does call it directly on every invocation, to
/// re-resolve the window's current geometry/scale/origin fresh.
pub(crate) fn find_window_for_pids(pids: &[i32], timeout: Duration) -> Result<WindowMatch> {
    crate::ffi::app_kit_init();

    let timeout_ms = timeout.as_millis() as u64;
    let outcome = poll_until(100, timeout_ms, || query_once(pids))?;
    outcome.value.ok_or(GlassError::Timeout(timeout_ms))
}

/// Poll `SCShareableContent` roughly every 100ms for the on-screen window whose
/// `windowID() == window_id` AND `owningApplication().processID() ∈ pids`, until found or
/// `timeout` elapses. The active-window retargeting lookup: `backend.rs` calls it on every
/// `capture_frame`/`send_pointer`/`send_key` once `select_window` has set an active
/// `CGWindowID`, in place of [`find_window_for_pids`]'s first-on-screen-by-pid resolution.
///
/// Do not drop the `pids` filter: `windowID` is not scoped to any app, so without it a
/// stale or foreign `CGWindowID` — one left over in `MacosPlatform::active_window` after the
/// windowing system recycles an id — matches *any* on-screen window system-wide, and callers
/// silently capture/click/type into someone else's window. Scoped, that becomes a loud
/// [`GlassError::WindowNotFound`].
///
/// Unlike `find_window_for_pids`'s [`GlassError::Timeout`] (waiting for a brand-new window at
/// launch), a `window_id` that never turns up here means a *previously known* window is gone —
/// closed, no longer owned by `pids`, or never valid — so this returns
/// [`GlassError::WindowNotFound`], matching the `Platform` contract's `select_window` error.
///
/// Returns a classified [`GlassError::PermissionDenied`]/[`GlassError::CaptureFailed`]
/// immediately on a genuine `SCShareableContent` failure, same as `find_window_for_pids`.
pub(crate) fn find_window_by_id(
    window_id: u32,
    pids: &[i32],
    timeout: Duration,
) -> Result<WindowMatch> {
    crate::ffi::app_kit_init();

    let timeout_ms = timeout.as_millis() as u64;
    let outcome = poll_until(100, timeout_ms, || query_once_by_id(window_id, pids))?;
    outcome.value.ok_or(GlassError::WindowNotFound)
}

/// Whether a scan should also collect a summary of every candidate it saw.
///
/// [`Candidates::Skip`] stops at the first match and allocates nothing — `capture_window`'s and
/// `find_window_for_pids`'s fallback, reached only while `active_window` is unset.
/// [`Candidates::Collect`] is the once-per-session adoption path, which pays for a full pass so
/// the adoption record can name what it chose between (#263).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Candidates {
    Skip,
    Collect,
}

/// Find the first on-screen `SCWindow` in `content.windows()` owned by one of `pids`,
/// returning it alongside its owning pid — and, when `collect` is [`Candidates::Collect`], a
/// summary of every such window in the same order, with the returned one marked adopted.
///
/// One loop for both modes so the hot lookup and the adoption record can never disagree about
/// what "the target window" means. [`find_on_screen_window_by_id`] keeps its own loop — it
/// matches by `window_id` + pid, not pid alone.
pub(crate) fn scan_on_screen_windows(
    content: &SCShareableContent,
    pids: &[i32],
    collect: Candidates,
) -> (Option<(Retained<SCWindow>, i32)>, Vec<CandidateWindow>) {
    // SAFETY: `windows` is a plain getter on a live `SCShareableContent`; no other
    // preconditions.
    let windows: Retained<NSArray<SCWindow>> = unsafe { content.windows() };

    let mut found: Option<(Retained<SCWindow>, i32)> = None;
    let mut candidates = Vec::new();
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
        let Some(app) = owning_application else {
            continue;
        };
        // SAFETY: same as above — a plain property getter.
        let pid = unsafe { app.processID() };
        if !pids.contains(&pid) {
            continue;
        }
        let adopted = found.is_none();
        if adopted {
            found = Some((w.clone(), pid));
        }
        if collect == Candidates::Skip {
            break;
        }
        // SAFETY: `w` is live; `title` is a plain property getter (same read
        // `app_window_from` performs).
        let title = unsafe { w.title() }.map(|t| t.to_string());
        let (geometry, _scale, _origin_pt) = window_geometry_and_scale(&w);
        candidates.push(CandidateWindow {
            // SAFETY: `w` is live; a plain property getter.
            window_id: unsafe { w.windowID() },
            title,
            geometry,
            adopted,
        });
    }
    (found, candidates)
}

/// [`scan_on_screen_windows`] without the candidate summary, for `capture::capture_window`'s
/// per-call lookup (its only caller). Returns the live `Retained<SCWindow>` itself, against
/// the module's normal "never a `Retained<SCWindow>`" rule (see the module doc):
/// `capture::capture_window` needs exactly that, still inside the same completion-handler
/// callback, to build an `SCContentFilter` from it.
pub(crate) fn find_on_screen_window(
    content: &SCShareableContent,
    pids: &[i32],
) -> Option<(Retained<SCWindow>, i32)> {
    scan_on_screen_windows(content, pids, Candidates::Skip).0
}

/// Find the on-screen `SCWindow` in `content.windows()` whose `windowID() == window_id` AND
/// `owningApplication().processID() ∈ pids`, returning it alongside its owning pid. The
/// `find_window_by_id`-side counterpart of [`find_on_screen_window`] (which filters by owning
/// pid alone). Carries its own copy of [`scan_on_screen_windows`]'s on-screen filter rather
/// than sharing it — keeping the two in sync on what "on-screen" means is a discipline, not
/// something enforced. The `pids` check is load-bearing, not defensive: see
/// [`find_window_by_id`]'s doc. Used by [`query_once_by_id`] and by
/// `capture::capture_window_by_id`, which needs the live `SCWindow` itself inside the
/// completion-handler callback to build an `SCContentFilter`.
pub(crate) fn find_on_screen_window_by_id(
    content: &SCShareableContent,
    window_id: u32,
    pids: &[i32],
) -> Option<(Retained<SCWindow>, i32)> {
    // SAFETY: `windows` is a plain getter on a live `SCShareableContent`; no other
    // preconditions.
    let windows: Retained<NSArray<SCWindow>> = unsafe { content.windows() };

    for w in windows.iter() {
        // SAFETY: `w` is a live `SCWindow` yielded by the array; these are plain property
        // getters with no other preconditions — see `scan_on_screen_windows`'s identical
        // SAFETY notes.
        if !unsafe { w.isOnScreen() } {
            continue;
        }
        if unsafe { w.windowID() } != window_id {
            continue;
        }
        // SAFETY: same as above — a plain property getter.
        let owning_application = unsafe { w.owningApplication() };
        let Some(app) = owning_application else {
            continue;
        };
        // SAFETY: same as above — a plain property getter.
        let pid = unsafe { app.processID() };
        if !pids.contains(&pid) {
            continue;
        }
        return Some((w, pid));
    }
    None
}

/// Derive a window's pixel geometry, `SCContentFilter` point-to-pixel scale, and POINT
/// origin from a live `SCWindow` — the `SCContentFilter`/`pointPixelScale`/`contentRect` ->
/// pixel-`WindowGeometry` conversion `capture::capture_window` also performs for the frame
/// it produces. Factored out so [`window_match_from`], [`app_window_from`], and
/// [`scan_on_screen_windows`] can't drift on how a window becomes a pixel geometry.
fn window_geometry_and_scale(w: &SCWindow) -> (WindowGeometry, f64, (f64, f64)) {
    // SAFETY: `w` is a live `SCWindow` passed in by a caller that just resolved it from a
    // live `SCShareableContent.windows()` array (see `scan_on_screen_windows`/
    // `find_on_screen_window_by_id`/[`list_app_windows`]); `capture.rs` uses this same
    // initializer on the same kind of live `SCWindow` — no other preconditions.
    let filter =
        unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), w) };
    // SAFETY: `filter` is live; these are plain property getters with no other
    // preconditions.
    let (scale, content_rect) = unsafe { (filter.pointPixelScale() as f64, filter.contentRect()) };
    let geometry = crate::coords::pixel_geometry_from_content_rect(
        content_rect.origin.x,
        content_rect.origin.y,
        content_rect.size.width,
        content_rect.size.height,
        scale,
    );
    let origin_pt = (content_rect.origin.x, content_rect.origin.y);
    (geometry, scale, origin_pt)
}

/// Build a [`WindowMatch`] snapshot from a live `SCWindow` + its owning `pid`. Factored out
/// so [`query_once`], [`query_once_with_candidates`], and [`query_once_by_id`] can't drift
/// on how a match becomes a `WindowMatch`.
fn window_match_from(w: &SCWindow, pid: i32) -> WindowMatch {
    // SAFETY: `w` is live (see `window_geometry_and_scale`'s identical note); a plain
    // property getter with no other preconditions.
    let window_id = unsafe { w.windowID() };
    let (geometry, scale, origin_pt) = window_geometry_and_scale(w);
    WindowMatch {
        pid,
        window_id,
        geometry,
        scale,
        origin_pt,
    }
}

/// Build an [`AppWindow`] snapshot from a live `SCWindow` + its already-resolved owning
/// `app` — [`list_app_windows`]'s per-window counterpart of [`window_match_from`], reading out
/// title and owning application name as owned `String`s alongside the same pixel geometry
/// derivation. Takes `app` rather than re-deriving it via `w.owningApplication()`, which
/// `list_app_windows`'s loop has already called to filter by pid. That filter also guarantees
/// every `w` here has an owning application, so `application_name` is always `Some`; the field
/// stays `Option<String>` to match `WindowInfo::class`.
fn app_window_from(w: &SCWindow, app: &SCRunningApplication) -> AppWindow {
    // SAFETY: `w` is live (see `window_geometry_and_scale`'s identical note); these are
    // plain property getters with no other preconditions.
    let window_id = unsafe { w.windowID() };
    // SAFETY: same as above.
    let title = unsafe { w.title() }.map(|t| t.to_string());
    // SAFETY: `app` is the live `SCRunningApplication` the caller already resolved via
    // `w.owningApplication()`; `applicationName` is a plain property getter with no other
    // preconditions.
    let application_name = Some(unsafe { app.applicationName() }.to_string());
    let (geometry, _scale, _origin_pt) = window_geometry_and_scale(w);
    AppWindow {
        window_id,
        geometry,
        title,
        application_name,
    }
}

/// Enumerate every on-screen window owned by one of `pids`, via a single `SCShareableContent`
/// query (the multi-window counterpart of [`find_window_for_pids`]'s first-match lookup).
/// Unlike [`find_window_for_pids`]/[`find_window_by_id`], this does not `poll_until` retry:
/// it's a one-shot snapshot, and an app legitimately having zero on-screen windows at some
/// moment is a normal `Ok(vec![])`.
///
/// Calls [`crate::ffi::app_kit_init`] first, same as `find_window_for_pids`. Returns a
/// classified error immediately on a genuine `SCShareableContent` failure (same
/// `PermissionDenied`/`CaptureFailed` classification as `query_once` — see
/// [`crate::ffi::classify_null_result`]). A completion handler that never replies within
/// [`QUERY_TIMEOUT`] is treated as a backend error here (unlike `query_once`'s
/// poll-loop-friendly `Ok(None)`): this function has no outer retry loop, so silently
/// returning an empty `Vec` on a wedged handler would be indistinguishable from "the app
/// really has no windows right now".
pub(crate) fn list_app_windows(pids: &[i32]) -> Result<Vec<AppWindow>> {
    crate::ffi::app_kit_init();

    let (tx, rx) = mpsc::channel::<ListReply>();
    let pids_owned: Vec<i32> = pids.to_vec();

    // The completion handler collects every matching window into owned `AppWindow`s
    // (plain data, `Send` regardless of what ObjC objects were touched to build it) and
    // sends the whole `Vec` at once — never a `Retained<SCWindow>` (see module doc).
    let block = RcBlock::new(
        move |content_ptr: *mut SCShareableContent, err_ptr: *mut NSError| {
            if content_ptr.is_null() {
                let err = crate::ffi::classify_null_result(
                    err_ptr,
                    "SCShareableContent completion handler returned null content and null error",
                );
                let _ = tx.send(ListReply::Failed(err));
                return;
            }
            // SAFETY: `content_ptr` was just checked non-null; the framework guarantees it
            // points to a live `SCShareableContent` for the duration of this callback.
            let content: &SCShareableContent = unsafe { &*content_ptr };
            // SAFETY: `windows` is a plain getter on a live `SCShareableContent`; no other
            // preconditions.
            let windows: Retained<NSArray<SCWindow>> = unsafe { content.windows() };

            let mut found = Vec::new();
            for w in windows.iter() {
                // SAFETY: `w` is a live `SCWindow` yielded by the array; plain property
                // getters with no other preconditions — see `scan_on_screen_windows`'s
                // identical notes.
                if !unsafe { w.isOnScreen() } {
                    continue;
                }
                // SAFETY: same as above.
                let owning_application = unsafe { w.owningApplication() };
                let Some(app) = owning_application else {
                    continue;
                };
                // SAFETY: same as above.
                let pid = unsafe { app.processID() };
                if !pids_owned.contains(&pid) {
                    continue;
                }
                found.push(app_window_from(&w, &app));
            }
            let _ = tx.send(ListReply::Found(found));
        },
    );

    // SAFETY: `block` matches `getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler`'s
    // documented signature (`*mut SCShareableContent, *mut NSError`, per the generated
    // binding) — same call `query_once` makes. The call itself has no other
    // preconditions.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }

    match rx.recv_timeout(QUERY_TIMEOUT) {
        Ok(ListReply::Found(v)) => Ok(v),
        Ok(ListReply::Failed(e)) => Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(GlassError::Backend(
            "SCShareableContent completion handler did not reply within the query timeout".into(),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(GlassError::Backend(
            "SCShareableContent completion handler was dropped without replying".into(),
        )),
    }
}

/// Cap on a single [`query_once`]/[`query_once_with_candidates`] attempt's wait for its
/// `SCShareableContent` completion handler. A query resolves in well under a second, so this is
/// a wedged-handler backstop, not normal latency. Kept small so it can't eat much of the outer
/// poll loop's deadline budget on a single bad tick — that budget belongs to the caller
/// (`find_window_for_pids`'s `poll_until`, or `backend.rs::discover_window`'s own loop).
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// The shared body of [`query_once`] and [`query_once_with_candidates`]: one
/// `SCShareableContent` round trip via the `RcBlock` -> `mpsc` bridge (`ffi.rs`'s documented
/// pattern). `Ok(Some(_))` on a match. `Ok(None)` only when the framework answered and no
/// matching on-screen window exists yet — a transient state the outer poll retries. `Err` in
/// two cases, and a poll loop must treat them differently from `Ok(None)`:
///
/// - `SCShareableContent` itself failed — classified via
///   [`crate::ffi::classify_null_result`] (TCC decline -> `PermissionDenied`, anything else
///   -> `CaptureFailed`), not assumed to always be a permission decline;
/// - the handler was wedged or dropped and sent no reply within [`QUERY_TIMEOUT`], classified by
///   the host-tested pure [`crate::shareable_receive::classify_receive`] helper.
///
/// Either way the `Err` aborts the caller's `poll_until` loop immediately (`Err` from a tick
/// stops polling, `Ok(None)` retries to the deadline), so a query that could not enumerate the
/// windows is never reported as "no window yet". `collect` is forwarded straight to
/// [`scan_on_screen_windows`]: [`Candidates::Skip`] (`query_once`) gets back an unallocated
/// empty `Vec`, so the hot path pays nothing for a candidate summary it never uses.
///
/// The adopted window's geometry is read once here, from the `WindowMatch` snapshot
/// ([`window_match_from`]) — not re-derived from the scan's own separately-read candidate entry
/// — so the printed record and the geometry glass goes on to use can't diverge (#263):
/// whichever candidate is `adopted` gets its `geometry` overwritten with the `WindowMatch`'s.
fn query_once_inner(
    pids: &[i32],
    collect: Candidates,
) -> Result<Option<(WindowMatch, Vec<CandidateWindow>)>> {
    let (tx, rx) = mpsc::channel::<CandidateQueryReply>();
    let pids_owned: Vec<i32> = pids.to_vec();

    // The completion handler does the whole match-or-not decision synchronously inside
    // the callback (per ffi.rs's async-bridge pattern) and only ever sends
    // `CandidateQueryReply` — plain owned data, `Send` regardless of what ObjC objects
    // were touched to build it — never a `Retained<SCWindow>` (see module doc).
    let block = RcBlock::new(
        move |content_ptr: *mut SCShareableContent, err_ptr: *mut NSError| {
            if content_ptr.is_null() {
                let err = crate::ffi::classify_null_result(
                    err_ptr,
                    "SCShareableContent completion handler returned null content and null error",
                );
                let _ = tx.send(CandidateQueryReply::Failed(err));
                return;
            }
            // SAFETY: `content_ptr` was just checked non-null; the framework guarantees it
            // points to a live `SCShareableContent` for the duration of this callback.
            let content: &SCShareableContent = unsafe { &*content_ptr };

            let (found, mut candidates) = scan_on_screen_windows(content, &pids_owned, collect);
            let Some((w, pid)) = found else {
                let _ = tx.send(CandidateQueryReply::NotFound);
                return;
            };
            let m = window_match_from(&w, pid);
            // Safe by construction: `scan_on_screen_windows` sets `adopted = found.is_none()`,
            // true for at most one candidate, so `.find` below can't correct the wrong one.
            if let Some(adopted) = candidates.iter_mut().find(|c| c.adopted) {
                adopted.geometry = m.geometry.clone();
            }
            let _ = tx.send(CandidateQueryReply::Found(m, candidates));
        },
    );

    // SAFETY: `block` matches `getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler`'s
    // documented signature (`*mut SCShareableContent, *mut NSError`, per the generated
    // binding) — the exact sequence the spike proved end-to-end. The call itself has no
    // other preconditions.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }

    // The callback's Objective-C object walk remains macOS integration-only. Once it reaches this
    // channel, reply/timeout/disconnection classification is pure and host-tested.
    match crate::shareable_receive::classify_receive(rx.recv_timeout(QUERY_TIMEOUT))? {
        CandidateQueryReply::Found(m, candidates) => Ok(Some((m, candidates))),
        CandidateQueryReply::NotFound => Ok(None),
        CandidateQueryReply::Failed(e) => Err(e),
    }
}

/// [`query_once_inner`] with [`Candidates::Skip`], discarding the (always-empty) candidate
/// summary — the per-call hot-path lookup [`find_window_for_pids`] polls.
pub(crate) fn query_once(pids: &[i32]) -> Result<Option<WindowMatch>> {
    query_once_inner(pids, Candidates::Skip).map(|opt| opt.map(|(m, _)| m))
}

/// [`query_once_inner`] with [`Candidates::Collect`]: the adoption path's round trip,
/// returning the candidate summary alongside the match so `backend::discover_window`/
/// `discover_window_pid` can record what they chose between (#263). No extra query — the
/// candidates come out of the content the match was already found in.
pub(crate) fn query_once_with_candidates(
    pids: &[i32],
) -> Result<Option<(WindowMatch, Vec<CandidateWindow>)>> {
    query_once_inner(pids, Candidates::Collect)
}

/// [`query_once_inner`]'s channel payload — like [`QueryReply`] but carrying the candidate
/// summary [`scan_on_screen_windows`] collected alongside the match.
enum CandidateQueryReply {
    Found(WindowMatch, Vec<CandidateWindow>),
    NotFound,
    Failed(GlassError),
}

/// [`find_window_by_id`]'s per-attempt round trip — identical shape to [`query_once`] (same
/// `RcBlock` -> `mpsc` bridge, same `QUERY_TIMEOUT` cap, same error classification) but
/// matching on a specific `window_id` (scoped to `pids`) via [`find_on_screen_window_by_id`]
/// instead of an owning-pid set alone.
fn query_once_by_id(window_id: u32, pids: &[i32]) -> Result<Option<WindowMatch>> {
    let (tx, rx) = mpsc::channel::<QueryReply>();
    let pids_owned: Vec<i32> = pids.to_vec();

    // Same completion-handler contract as `query_once`'s block: only ever sends the plain
    // owned `QueryReply`, never a `Retained<SCWindow>` (see module doc).
    let block = RcBlock::new(
        move |content_ptr: *mut SCShareableContent, err_ptr: *mut NSError| {
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

            let Some((w, pid)) = find_on_screen_window_by_id(content, window_id, &pids_owned)
            else {
                let _ = tx.send(QueryReply::NotFound);
                return;
            };
            let _ = tx.send(QueryReply::Found(window_match_from(&w, pid)));
        },
    );

    // SAFETY: same as `query_once`'s identical call — the documented signature, no other
    // preconditions.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }

    match rx.recv_timeout(QUERY_TIMEOUT) {
        Ok(QueryReply::Found(m)) => Ok(Some(m)),
        Ok(QueryReply::NotFound) => Ok(None),
        Ok(QueryReply::Failed(e)) => Err(e),
        // Same distinction as [`query_once_inner`]: a wedged handler is a failure, not `Ok(None)` (glass#467).
        Err(mpsc::RecvTimeoutError::Timeout) => Err(GlassError::Backend(
            "SCShareableContent completion handler did not reply within the query timeout".into(),
        )),
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

/// [`list_app_windows`]'s completion-block outcome — the multi-window counterpart of
/// [`QueryReply`], funneled out as the same kind of plain owned data (never a
/// `Retained<SCWindow>`).
enum ListReply {
    Found(Vec<AppWindow>),
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

    #[test]
    fn list_reply_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ListReply>();
    }

    #[test]
    fn candidate_query_reply_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CandidateQueryReply>();
    }
}
