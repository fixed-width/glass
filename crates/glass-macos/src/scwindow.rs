//! `SCShareableContent` window discovery by pid, and by `CGWindowID` (Plan 4's
//! active-window retargeting).
//!
//! Polls ScreenCaptureKit's `SCShareableContent` enumeration — the same async
//! completion-handler API proven end-to-end in the objc2 spike — for the first on-screen
//! window
//! owned by one of a set of pids ([`find_window_for_pids`], the launched app's process
//! set) or for the specific on-screen window with a given `CGWindowID` that is *also* owned
//! by one of that same pid set ([`find_window_by_id`], `MacosPlatform`'s active window once
//! `select_window` has run — the pid scoping closes a silent-wrong-target hole a bare
//! `CGWindowID` match would otherwise open, since window ids are not namespaced per app),
//! following `ffi.rs`'s documented async-bridge convention.
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
use objc2_screen_capture_kit::{
    SCContentFilter, SCRunningApplication, SCShareableContent, SCWindow,
};

use glass_core::platform::WindowGeometry;
use glass_core::{poll_until, GlassError, Result};

/// A discovered on-screen window: enough to re-find or capture it later without holding
/// a live `Retained<SCWindow>` across the completion handler's thread boundary (see
/// module doc).
// `geometry`/`scale`/`origin_pt` are read by `start_app` (via `backend.rs::discover_window`
// -> `query_once`) and by `send_pointer`/`capture_frame`/`send_key` (via `backend.rs`'s
// per-call `find_window_for_pids`/`find_window_by_id` resolution); `window_id` is read by
// `start_app` too (to seed `MacosPlatform::active_window`, Plan 4 Task 1). `pid` is read by
// `send_pointer`/`send_key` (via `resolve_active_window`'s per-call resolution) as the
// CGEvent focus/AX-scoping target, and by every `find_window_by_id` call site's
// pid-scoping check (Plan 4 final-review fix 1) — every field is read somewhere.
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

/// A discovered on-screen window, as returned by [`list_app_windows`] (Plan 4's
/// `list_windows`): the `CGWindowID`, pixel geometry, title, and owning application name —
/// everything `backend::list_windows` needs to build a `WindowInfo` per window. Same
/// can't-hold-a-live-`SCWindow`-across-the-completion-boundary rationale as [`WindowMatch`]
/// (see the module doc); `title`/`application_name` are read out as owned `String`s inside
/// the completion block for the same reason `WindowMatch`'s fields are plain owned data.
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
///
/// `MacosPlatform::start_app` still can't use this directly: it runs its own poll loop
/// (`backend.rs::discover_window`) that alternates a single `query_once` attempt with
/// `child.try_wait()` so a crashed launch fails fast with `AppExited`, and this function's
/// self-contained `poll_until` has no child handle to race against. But
/// `MacosPlatform::send_pointer` does call this directly on every invocation, to
/// re-resolve the window's current geometry/scale/origin fresh (the window may have moved
/// or resized since `start_app`, or since the previous `send_pointer` call) — there's no
/// child handle to race there either (the session is already established), so this
/// self-contained poll-until-found-or-timeout is exactly what it needs.
pub(crate) fn find_window_for_pids(pids: &[i32], timeout: Duration) -> Result<WindowMatch> {
    crate::ffi::app_kit_init();

    let timeout_ms = timeout.as_millis() as u64;
    let outcome = poll_until(100, timeout_ms, || query_once(pids))?;
    outcome.value.ok_or(GlassError::Timeout(timeout_ms))
}

/// Poll `SCShareableContent` roughly every 100ms for the on-screen window whose
/// `windowID() == window_id` AND `owningApplication().processID() ∈ pids`, until found or
/// `timeout` elapses. This is Plan 4's active-window retargeting lookup: `backend.rs` calls
/// it on every `capture_frame`/`send_pointer`/`send_key` once `select_window` has set an
/// active `CGWindowID`, in place of [`find_window_for_pids`]'s first-on-screen-by-pid
/// resolution — see its module doc for why re-resolving fresh per call (rather than caching
/// a `Retained<SCWindow>`) is the only safe option.
///
/// The `pids` filter (final-review fix 1) closes a silent-wrong-target hole: without it, a
/// stale/foreign `CGWindowID` — e.g. left over in `MacosPlatform::active_window` after the
/// windowing system recycles an id, or a bug that stores an id from a window that was never
/// actually confirmed to belong to this app — would happily match *any* on-screen window
/// system-wide, and every caller would silently capture/click/type into someone else's
/// window. Scoping the match to `pids` turns that into a loud [`GlassError::WindowNotFound`]
/// instead.
///
/// Unlike `find_window_for_pids`'s [`GlassError::Timeout`] (appropriate while waiting for a
/// brand-new window to first appear at launch), a `window_id` that never turns up here means
/// a *previously known* window is gone — closed, no longer owned by `pids`, or the id was
/// never valid — so this returns [`GlassError::WindowNotFound`] instead, matching the
/// `Platform` contract's `select_window`/window-op error (`glass_core::platform`'s doc) for
/// exactly that case.
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
        let Some(app) = owning_application else {
            continue;
        };
        // SAFETY: same as above — a plain property getter.
        let pid = unsafe { app.processID() };
        if pids.contains(&pid) {
            return Some((w, pid));
        }
    }
    None
}

/// Find the on-screen `SCWindow` in `content.windows()` whose `windowID() == window_id` AND
/// `owningApplication().processID() ∈ pids`, returning it alongside its owning pid. The
/// `find_window_by_id`-side counterpart of [`find_on_screen_window`] (which filters by
/// owning pid alone instead of a specific window + pid set): same on-screen filter, same
/// iteration, so the two lookups can't drift on what "on-screen" means. The `pids` check
/// (final-review fix 1) is load-bearing, not defensive: `windowID` alone is not scoped to
/// any particular app, so without it this would match *any* on-screen window system-wide,
/// letting a stale/foreign `CGWindowID` silently resolve to someone else's window — see
/// [`find_window_by_id`]'s doc. Used by [`query_once_by_id`] (which then builds a
/// [`WindowMatch`] snapshot via [`window_match_from`], the same builder [`query_once`] uses)
/// and by `capture::capture_window_by_id` (which, like `capture_window`, needs the live
/// `SCWindow` itself, still inside the same completion-handler callback, to build an
/// `SCContentFilter` — see [`find_on_screen_window`]'s doc).
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
        // getters with no other preconditions — see `find_on_screen_window`'s identical
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
/// it produces. Factored out so [`window_match_from`] and [`app_window_from`] can't drift
/// on how a window becomes a pixel geometry.
fn window_geometry_and_scale(w: &SCWindow) -> (WindowGeometry, f64, (f64, f64)) {
    // SAFETY: `w` is a live `SCWindow` passed in by a caller that just resolved it from a
    // live `SCShareableContent.windows()` array (see `find_on_screen_window`/
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
/// so [`query_once`] and [`query_once_by_id`] can't drift on how a match becomes a
/// `WindowMatch`.
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
/// `app` — [`list_app_windows`]'s per-window counterpart of [`window_match_from`], reading
/// out title and owning application name as owned `String`s alongside the same pixel
/// geometry derivation. Takes `app` rather than re-deriving it via `w.owningApplication()`
/// (final-review fix M3): `list_app_windows`'s loop has already called that getter once to
/// filter by pid, so a second call here would be redundant — and since that filter already
/// guarantees every `w` passed here has an owning application, `application_name` is always
/// `Some` (the field stays `Option<String>` to match `AppWindow`'s general shape, matching
/// `WindowInfo::class`'s own `Option<String>`).
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
/// query (the multi-window counterpart of [`find_window_for_pids`]'s first-match lookup —
/// Plan 4's `list_windows`). Unlike [`find_window_for_pids`]/[`find_window_by_id`], this does
/// not `poll_until` retry: it's a one-shot snapshot, and an app legitimately having zero
/// on-screen windows at some moment is a normal `Ok(vec![])`, not a `Timeout`/`WindowNotFound`
/// condition worth retrying.
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
                // getters with no other preconditions — see `find_on_screen_window`'s
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

            let Some((w, pid)) = find_on_screen_window(content, &pids_owned) else {
                let _ = tx.send(QueryReply::NotFound);
                return;
            };
            let _ = tx.send(QueryReply::Found(window_match_from(&w, pid)));
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

/// [`find_window_by_id`]'s per-attempt round trip — identical shape to [`query_once`] (same
/// `RcBlock` -> `mpsc` bridge, same `QUERY_TIMEOUT` cap, same error classification) but
/// matching on a specific `window_id` (scoped to `pids`, final-review fix 1) via
/// [`find_on_screen_window_by_id`] instead of an owning-pid set alone.
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
}
