//! Window discovery, list/select, and window ops for the Windows backend.
//!
//! Built on the verbatim `util.rs` enumeration helpers (the validated probe's
//! window walk + DWM-geometry) and the probe's `focus()`, but the discovery
//! *ladder* and the list/select/op wiring are new code that mirrors the x11
//! backend's semantics: app windows are the launched app's process-tree windows
//! that pass the app-window filter, with a title/class-hint fallback; `active`
//! means "the window glass currently targets" (not the OS foreground window).
//!
//! Move/Resize go through `SetWindowPos`, which works in `GetWindowRect` space
//! (the legacy rect that *includes* the invisible resize border on Win10/11).
//! Our geometry is the DWM `extended_frame_bounds` (the visible frame), so we
//! correct for that border delta so the visible frame lands where asked.

use glass_core::platform::{WindowGeometry, WindowHint};
use glass_core::{GlassError, Result};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetWindowThreadProcessId, IsIconic, SetForegroundWindow, SetWindowPos,
    ShowWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
};

use crate::util::{
    enum_top_windows, extended_frame_bounds, find_by_title, window_rect, WinInfo,
};

/// A DWM frame `RECT` (left/top/right/bottom, physical pixels) as window geometry.
pub(crate) fn rect_to_geometry(r: RECT) -> WindowGeometry {
    WindowGeometry {
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
    }
}

/// The launched app's windows: every top-level window that passes the app-window
/// filter AND belongs to the app's process set (`pids` — the authoritative Job PID
/// list unioned with the Toolhelp descendant walk; Electron/Java hand their UI to
/// child processes, which the Job list captures even after the launcher exits).
pub(crate) fn app_window_infos(pids: &[u32]) -> Vec<WinInfo> {
    enum_top_windows()
        .into_iter()
        .filter(|w| w.looks_like_app_window() && pids.contains(&w.pid))
        .collect()
}

/// One scan implementing the discovery ladder:
/// 1. the app's own process-set windows (the common case) — first match;
/// 2. else, if `hint` has a title, an app-like window whose title contains it
///    (handles an app that hands its UI to an unrelated process the pid-set misses);
/// 3. else, if `hint` has a class, an app-like window whose class equals it.
///
/// `pids` is the app's process set (Job list ∪ Toolhelp walk); rungs 2/3 don't use it.
/// Returns the first match or `None`.
pub(crate) fn find_app_window(pids: &[u32], hint: Option<&WindowHint>) -> Option<WinInfo> {
    if let Some(w) = app_window_infos(pids).into_iter().next() {
        return Some(w);
    }
    // Hint fallbacks (only reached when the pid-set rung misses — e.g. the app handed its UI
    // to an unrelated process). Note: rung 2 matches the title as a case-insensitive SUBSTRING
    // (find_by_title), intentionally looser than the x11 backend's exact-title match, since real
    // window titles carry dynamic prefixes/suffixes. Rung 3 matches the class exactly.
    if let Some(hint) = hint {
        if let Some(title) = &hint.title {
            if let Some(w) = find_by_title(title).into_iter().next() {
                return Some(w);
            }
        }
        if let Some(class) = &hint.class {
            if let Some(w) = enum_top_windows()
                .into_iter()
                .find(|w| w.looks_like_app_window() && w.class == *class)
            {
                return Some(w);
            }
        }
    }
    None
}

/// Best-effort raise + focus, ported from the validated probe's `focus()`:
/// restore if minimized, `SetForegroundWindow`; on failure (foreground lock),
/// attach to the target thread's input, retry `SetForegroundWindow` +
/// `BringWindowToTop`, then detach. Individual steps are ignored (the probe does
/// the same); we only ever return `Ok(())`.
pub(crate) fn focus_window(hwnd: HWND) -> Result<()> {
    // SAFETY: hwnd is a window handle we just resolved; each call is a standard
    // focus primitive whose individual failure is non-fatal (foreground lock).
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return Ok(());
        }
        // Foreground-lock blocked it: attach to the target's thread input and retry.
        let me = GetCurrentThreadId();
        let target = GetWindowThreadProcessId(hwnd, None);
        let _ = AttachThreadInput(me, target, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        let _ = AttachThreadInput(me, target, false);
    }
    Ok(())
}

/// Move so the *visible* (DWM) frame's top-left lands at `(x, y)`. `SetWindowPos`
/// works in `GetWindowRect` space, which includes the invisible resize border, so
/// shift by the border delta (legacy-rect origin − DWM-frame origin, <= 0).
pub(crate) fn move_window(hwnd: HWND, x: i32, y: i32) -> Result<()> {
    let efb =
        extended_frame_bounds(hwnd).ok_or_else(|| GlassError::Backend("no DWM frame bounds".into()))?;
    let wr = window_rect(hwnd).ok_or_else(|| GlassError::Backend("no window rect".into()))?;
    // Border deltas: for a normal window GetWindowRect sits a few px outside the visible
    // DWM frame, so dx,dy <= 0. Maximized/borderless windows are degenerate here (the legacy
    // rect and the DWM frame relate differently) — moving them is a known v1 limitation.
    let (dx, dy) = (wr.left - efb.left, wr.top - efb.top);
    // SAFETY: SetWindowPos on a valid HWND; SWP flags below make it move-only.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            x + dx,
            y + dy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .map_err(|e| GlassError::Backend(format!("SetWindowPos move: {e}")))?;
    Ok(())
}

/// Resize so the *visible* (DWM) frame is `width` x `height`. The legacy rect is
/// larger by the invisible resize border, so add that extra to the requested size.
pub(crate) fn resize_window(hwnd: HWND, width: u32, height: u32) -> Result<()> {
    let efb =
        extended_frame_bounds(hwnd).ok_or_else(|| GlassError::Backend("no DWM frame bounds".into()))?;
    let wr = window_rect(hwnd).ok_or_else(|| GlassError::Backend("no window rect".into()))?;
    let extra_w = (wr.right - wr.left) - (efb.right - efb.left);
    let extra_h = (wr.bottom - wr.top) - (efb.bottom - efb.top);
    // SAFETY: SetWindowPos on a valid HWND; SWP flags below make it resize-only.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width as i32 + extra_w,
            height as i32 + extra_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .map_err(|e| GlassError::Backend(format!("SetWindowPos resize: {e}")))?;
    Ok(())
}

/// The DWM frame geometry of `hwnd`, or a `Backend` error if it has no bounds.
pub(crate) fn geometry_of(hwnd: HWND) -> Result<WindowGeometry> {
    extended_frame_bounds(hwnd)
        .map(rect_to_geometry)
        .ok_or_else(|| GlassError::Backend("no DWM frame bounds".into()))
}
