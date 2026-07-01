//! `AXUIElement` window operations (position/size get+set, raise, main) and the
//! `CGWindowID` <-> `AXUIElement` correlation this crate needs to bridge two separate
//! window-identity worlds: ScreenCaptureKit/`SCShareableContent` (`scwindow.rs`) addresses
//! a window by its `CGWindowID` — the id `list_windows`/`select_window` hand back to an
//! agent — while every window *operation* (move, resize, raise, focus) only exists on the
//! Accessibility side, as an `AXUIElement`. There is no public API that maps one to the
//! other in either direction.
//!
//! ## The correlation: private `_AXUIElementGetWindow`, contained + geometry fallback
//!
//! [`_AXUIElementGetWindow`] is the well-known private symbol every AX-based window
//! manager (Rectangle, yabai, BetterDisplay, Hammerspoon) uses for exactly this: given an
//! `AXUIElementRef`, it writes out the `CGWindowID` that window belongs to. It is
//! undocumented, unversioned, and Apple could remove or rename it in any macOS release —
//! the same posture this codebase takes with other private APIs (e.g. the planned
//! `CGVirtualDisplay` provider): contained in one module, never the *only* path, and
//! flagged here so a later maintainer knows to re-validate it against each new macOS
//! major. [`ax_window_for_cgwindowid`] never trusts it exclusively: if the private call
//! errors on *every* enumerated window (the "symbol looks broken" signal — distinct from
//! "this particular window just isn't among them"), it falls back to matching the
//! `AXUIElement`'s own position/size (converted points -> pixels via
//! `coords::point_to_global_pixel`) against the target `CGWindowID`'s already-known
//! `SCShareableContent` geometry, within a small pixel tolerance, picking the closest match.
//!
//! ## CFType memory: every AX "Copy"/"Create" call returns a +1 ref
//!
//! `AXUIElementCopyAttributeValue`/`AXValueCreate` follow Core Foundation's ordinary
//! Copy/Create ownership rule — the caller owns the returned ref and must release it.
//! [`copy_attribute`] wraps every such raw `*const CFType` in a `CFRetained<CFType>` via
//! `CFRetained::from_raw` (no extra retain — matches `input.rs`'s `mouse_event`/
//! `keyboard_event` wrapping `CGEvent::new_*`'s already-owned results), so the ref is
//! dropped (released) automatically once it goes out of scope; nothing in this module
//! manually calls `CFRelease`.
//!
//! ## `objc2-application-services` gotcha: the `kAX*` string constants are NOT generated
//!
//! The crate ships `AXAttributeConstants`/`AXActionConstants`/`AXRoleConstants` Cargo
//! features, but they produce **empty** generated files — `header-translator` did not pick
//! up `AXAttributeConstants.h`/`AXActionConstants.h`'s plain `extern const CFStringRef`
//! declarations (confirmed by reading the generated source directly: each file is a
//! two-line stub with no symbols). This module does not depend on those features at all;
//! instead it builds each attribute/action name as a `CFString::from_str` literal
//! (`"AXWindows"`, `"AXPosition"`, `"AXRaise"`, ...) — the same fixed values the (separate,
//! unused-here) `accessibility-sys` crate hardcodes for the same reason, and the values
//! Apple documents as stable strings, not just opaque symbol names.
//!
//! Every fn here is `pub(crate)` per Plan 4 Task 3's interface list; `backend.rs`'s
//! `MacosPlatform::window` (Task 4) is what wires them in, calling
//! [`ax_window_for_cgwindowid`] plus every getter/setter/action below. Task 5
//! (`select_window`) reuses the same entry points rather than adding new ones.

use std::ptr::NonNull;

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{kCFBooleanTrue, CFArray, CFRetained, CFString, CFType, CGPoint, CGSize};

use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result};

use crate::coords::point_to_global_pixel;

// The private CGWindowID<->AXUIElement bridge (see module doc). Declared here, not
// available from `objc2-application-services` (it's undocumented and intentionally absent
// from Apple's public headers, so no binding crate exposes it) — the same contained
// `extern "C"` pattern `permissions.rs` uses for `AXIsProcessTrusted`/
// `CGPreflightScreenCaptureAccess`.
//
// VERSION FRAGILITY: undocumented and unversioned; Apple could rename or remove it in any
// macOS release without notice. Re-validate (does it still link? does it still return
// `.Success` for real windows?) whenever this crate bumps its minimum-supported macOS
// major. `ax_window_for_cgwindowid`'s geometry fallback exists specifically so a broken
// symbol degrades this crate's window ops rather than making them entirely non-functional.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn _AXUIElementGetWindow(element: &AXUIElement, out_wid: *mut u32) -> AXError;
}

/// Per-field pixel tolerance for the geometry-match fallback (each of x/y/width/height).
/// More generous than the proven `window_ops.swift` reference's 4px `close()` (which
/// compares a before/after read of the *same* AX API): this compares an `AXUIElement`'s
/// frame against `SCWindow`'s independently-reported geometry, where small window-manager
/// framing differences are more plausible. Unverified at runtime — this task is
/// compile-only; Task 6's granted multi-window test is the first real exercise of this
/// path and may need to retune this constant.
const FALLBACK_TOLERANCE_PX: i32 = 8;

/// Resolve the `AXUIElement` window for `pid` whose `CGWindowID` is `window_id`.
///
/// Enumerates `AXUIElementCreateApplication(pid)`'s `kAXWindows`, and for each candidate
/// calls the private [`_AXUIElementGetWindow`] to read its `CGWindowID`, returning the
/// first exact match. If that private call errors on *every* candidate (a broken/renamed
/// symbol — see the module doc), falls back to matching by geometry: `geometry_px`/`scale`
/// are the target window's already-known `SCShareableContent` pixel geometry and
/// point-per-pixel scale (from `scwindow::WindowMatch`), compared against each AX window's
/// own position/size. [`GlassError::WindowNotFound`] if neither path finds a match.
pub(crate) fn ax_window_for_cgwindowid(
    pid: i32,
    window_id: u32,
    geometry_px: WindowGeometry,
    scale: f64,
) -> Result<CFRetained<AXUIElement>> {
    // SAFETY: `AXUIElementCreateApplication` never returns NULL per Apple's documented
    // contract (the binding itself `.expect()`s on this); `pid` is a plain process id with
    // no aliasing/lifetime preconditions.
    let app = unsafe { AXUIElement::new_application(pid) };
    let windows = ax_windows(&app)?;

    let mut any_private_call_succeeded = false;
    for w in windows.iter() {
        let mut wid: u32 = 0;
        // SAFETY: `w` is a live `AXUIElement` window just yielded by `kAXWindows`; `wid`
        // is a valid local out-param. See the symbol's declaration above for the broader
        // private-API contract this call relies on.
        let err = unsafe { _AXUIElementGetWindow(&w, &mut wid) };
        if err == AXError::Success {
            any_private_call_succeeded = true;
            if wid == window_id {
                return Ok(w);
            }
        }
    }

    if !any_private_call_succeeded {
        eprintln!(
            "glass-macos: _AXUIElementGetWindow errored on every AX window for pid {pid}; \
             falling back to geometry match for CGWindowID {window_id}"
        );
        if let Some(w) = geometry_fallback(&windows, &geometry_px, scale) {
            return Ok(w);
        }
    }
    Err(GlassError::WindowNotFound)
}

/// Read `el`'s `AXPosition` (top-left, in points — Quartz's global screen space, same unit
/// `coords.rs`'s `global_pixel_to_point`/`point_to_global_pixel` convert to/from).
pub(crate) fn ax_position(el: &AXUIElement) -> Result<(f64, f64)> {
    let value = attribute_axvalue(el, "AXPosition")?;
    let mut point = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: `value` was just verified (via `downcast`) to be a real `AXValue`; `point`
    // is a valid local out-param whose type matches the requested `AXValueType::CGPoint` —
    // mirrors the proven reference's `AXValueGetValue(v, .cgPoint, &p)`.
    let ok = unsafe { value.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()) };
    if !ok {
        return Err(GlassError::Backend("AXValueGetValue(AXPosition, .cgPoint) returned false".into()));
    }
    Ok((point.x, point.y))
}

/// Read `el`'s `AXSize` (width/height, in points).
pub(crate) fn ax_size(el: &AXUIElement) -> Result<(f64, f64)> {
    let value = attribute_axvalue(el, "AXSize")?;
    let mut size = CGSize { width: 0.0, height: 0.0 };
    // SAFETY: same as `ax_position` above, with `AXValueType::CGSize`/`CGSize`.
    let ok = unsafe { value.value(AXValueType::CGSize, NonNull::from(&mut size).cast()) };
    if !ok {
        return Err(GlassError::Backend("AXValueGetValue(AXSize, .cgSize) returned false".into()));
    }
    Ok((size.width, size.height))
}

/// Set `el`'s `AXPosition` to `pos` (points). Errors if the AX call itself does not report
/// success; does not read back to verify the value actually took — that's `backend.rs`'s
/// `window(op)` (Task 4), which reads back every mutating op through `ax_position`/
/// `ax_size` and returns a structured error if the change didn't land.
pub(crate) fn ax_set_position(el: &AXUIElement, pos: (f64, f64)) -> Result<()> {
    set_axvalue(el, "AXPosition", AXValueType::CGPoint, &mut CGPoint { x: pos.0, y: pos.1 })
}

/// Set `el`'s `AXSize` to `size` (points). Same no-read-back-verify contract as
/// [`ax_set_position`].
pub(crate) fn ax_set_size(el: &AXUIElement, size: (f64, f64)) -> Result<()> {
    set_axvalue(el, "AXSize", AXValueType::CGSize, &mut CGSize { width: size.0, height: size.1 })
}

/// Raise `el` to the front of its application's window list (`kAXRaiseAction`).
pub(crate) fn ax_raise(el: &AXUIElement) -> Result<()> {
    let action = CFString::from_str("AXRaise");
    // SAFETY: `el` is a live `AXUIElement`; matches `AXUIElementPerformAction`'s
    // documented contract (element + action name, no other preconditions).
    let err = unsafe { el.perform_action(&action) };
    if err != AXError::Success {
        return Err(ax_backend_err("AXRaise", err));
    }
    Ok(())
}

/// Mark `el` as its application's main window (`kAXMainAttribute` = true).
pub(crate) fn ax_set_main(el: &AXUIElement) -> Result<()> {
    let attr = CFString::from_str("AXMain");
    // SAFETY: `kCFBooleanTrue` is a framework-owned singleton that is always live for the
    // process's lifetime; reading the extern static is a plain global read.
    let value = unsafe { kCFBooleanTrue }.ok_or_else(|| GlassError::Backend("kCFBooleanTrue unavailable".into()))?;
    // SAFETY: `el` is live; `value` derefs to `&CFType` (see the module doc's CFType-memory
    // section); matches `AXUIElementSetAttributeValue`'s documented contract.
    let err = unsafe { el.set_attribute_value(&attr, value) };
    if err != AXError::Success {
        return Err(ax_backend_err("AXMain", err));
    }
    Ok(())
}

/// Copy `app`'s `kAXWindows` attribute and reinterpret it as a typed `CFArray<AXUIElement>`.
fn ax_windows(app: &AXUIElement) -> Result<CFRetained<CFArray<AXUIElement>>> {
    let cftype = copy_attribute(app, "AXWindows")?;
    let arr = cftype
        .downcast::<CFArray>()
        .map_err(|_| GlassError::Backend("kAXWindowsAttribute did not return a CFArray".into()))?;
    // SAFETY: `kAXWindowsAttribute` is documented by Apple to hold `AXUIElementRef`s; this
    // only attaches compile-time element-type information (no runtime effect) — the same
    // technique this crate's own `CFArray::from_objects`/`from_retained_objects`
    // constructors use internally (`CFRetained::cast_unchecked::<Self>(array)`).
    Ok(unsafe { CFRetained::cast_unchecked(arr) })
}

/// Copy `el`'s `attr_name` attribute value, downcast-checked to a concrete `AXValue`
/// (position/size are always `AXValue`-wrapped `CGPoint`/`CGSize`, never a bare `CFType`).
fn attribute_axvalue(el: &AXUIElement, attr_name: &str) -> Result<CFRetained<AXValue>> {
    let cftype = copy_attribute(el, attr_name)?;
    cftype.downcast::<AXValue>().map_err(|_| GlassError::Backend(format!("{attr_name} did not return an AXValue")))
}

/// `AXUIElementCopyAttributeValue(el, attr_name, ...)`, wrapping the already-retained (+1,
/// per Core Foundation's Copy/Create rule — see the module doc) raw result in a
/// `CFRetained<CFType>` so it's released automatically when dropped.
fn copy_attribute(el: &AXUIElement, attr_name: &str) -> Result<CFRetained<CFType>> {
    let attr = CFString::from_str(attr_name);
    let mut raw: *const CFType = std::ptr::null();
    // SAFETY: `el` is a live `AXUIElement`; `raw` is a valid local out-param slot matching
    // `AXUIElementCopyAttributeValue`'s documented signature. No other preconditions.
    let err = unsafe { el.copy_attribute_value(&attr, NonNull::from(&mut raw)) };
    if err != AXError::Success {
        return Err(ax_backend_err(attr_name, err));
    }
    let nn = NonNull::new(raw.cast_mut())
        .ok_or_else(|| GlassError::Backend(format!("{attr_name}: AX reported success but returned a null value")))?;
    // SAFETY: `AXUIElementCopyAttributeValue` follows Core Foundation's Copy/Create
    // ownership rule — an already-retained (+1) `CFTypeRef` on success — so
    // `CFRetained::from_raw` takes ownership without an extra retain (see the module doc).
    Ok(unsafe { CFRetained::from_raw(nn) })
}

/// `AXValueCreate(value_type, value)` + `AXUIElementSetAttributeValue(el, attr_name, ...)`
/// — the shared body of [`ax_set_position`]/[`ax_set_size`].
fn set_axvalue<T>(el: &AXUIElement, attr_name: &str, value_type: AXValueType, value: &mut T) -> Result<()> {
    // SAFETY: `AXValueCreate` copies the bytes pointed to by `value` internally (the
    // pointer only needs to be valid for the duration of this call); `T`'s layout matches
    // `value_type` for every caller of this private helper (`ax_set_position`/
    // `ax_set_size` always pair `CGPoint`/`AXValueType::CGPoint` and
    // `CGSize`/`AXValueType::CGSize`).
    let ax_value = unsafe { AXValue::new(value_type, NonNull::from(value).cast()) }
        .ok_or_else(|| GlassError::Backend(format!("AXValueCreate({attr_name}) failed")))?;
    let attr = CFString::from_str(attr_name);
    // SAFETY: `el` is live; `ax_value` derefs to `&CFType` (see the module doc); matches
    // `AXUIElementSetAttributeValue`'s documented contract.
    let err = unsafe { el.set_attribute_value(&attr, &ax_value) };
    if err != AXError::Success {
        return Err(ax_backend_err(attr_name, err));
    }
    Ok(())
}

/// Build a [`GlassError::Backend`] naming both the failing AX attribute/action and the raw
/// `AXError` code, so a diagnostic doesn't collapse every AX failure into one opaque
/// message.
fn ax_backend_err(context: &str, err: AXError) -> GlassError {
    GlassError::Backend(format!("{context}: AX call failed (AXError {})", err.0))
}

/// The geometry-match fallback for [`ax_window_for_cgwindowid`]: find the `AXUIElement` in
/// `windows` whose pixel geometry (position/size converted via `scale`, same conversion
/// `coords::point_to_global_pixel` performs for window ops) is closest to `target_px`,
/// among those within [`FALLBACK_TOLERANCE_PX`] on every field. `None` if no candidate is
/// within tolerance (including if reading any candidate's geometry itself fails — a window
/// this fallback can't measure is not a usable match).
fn geometry_fallback(
    windows: &CFArray<AXUIElement>,
    target_px: &WindowGeometry,
    scale: f64,
) -> Option<CFRetained<AXUIElement>> {
    let mut best: Option<(i64, CFRetained<AXUIElement>)> = None;
    for w in windows.iter() {
        let Ok(geom) = ax_geometry_px(&w, scale) else { continue };
        if !within_tolerance(&geom, target_px) {
            continue;
        }
        let score = geometry_distance(&geom, target_px);
        let better = match &best {
            Some((best_score, _)) => score < *best_score,
            None => true,
        };
        if better {
            best = Some((score, w));
        }
    }
    best.map(|(_, w)| w)
}

/// Read `el`'s position/size (points) and convert to pixel `WindowGeometry` via `scale` —
/// the same `point_to_global_pixel` conversion `coords.rs` documents for window ops.
fn ax_geometry_px(el: &AXUIElement, scale: f64) -> Result<WindowGeometry> {
    let (x, y) = point_to_global_pixel(ax_position(el)?, scale);
    let (w, h) = point_to_global_pixel(ax_size(el)?, scale);
    Ok(WindowGeometry { x, y, width: w.max(0) as u32, height: h.max(0) as u32 })
}

/// True if every field of `a`/`b` is within [`FALLBACK_TOLERANCE_PX`] of the other.
fn within_tolerance(a: &WindowGeometry, b: &WindowGeometry) -> bool {
    (a.x - b.x).abs() <= FALLBACK_TOLERANCE_PX
        && (a.y - b.y).abs() <= FALLBACK_TOLERANCE_PX
        && (a.width as i32 - b.width as i32).abs() <= FALLBACK_TOLERANCE_PX
        && (a.height as i32 - b.height as i32).abs() <= FALLBACK_TOLERANCE_PX
}

/// Sum of per-field absolute pixel differences between `a`/`b` — used only to rank
/// candidates that already passed [`within_tolerance`], smallest-first.
fn geometry_distance(a: &WindowGeometry, b: &WindowGeometry) -> i64 {
    i64::from((a.x - b.x).abs())
        + i64::from((a.y - b.y).abs())
        + i64::from((a.width as i32 - b.width as i32).abs())
        + i64::from((a.height as i32 - b.height as i32).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(x: i32, y: i32, width: u32, height: u32) -> WindowGeometry {
        WindowGeometry { x, y, width, height }
    }

    #[test]
    fn within_tolerance_accepts_exact_and_small_diffs_rejects_large() {
        let target = geom(100, 200, 640, 480);
        assert!(within_tolerance(&target, &target));
        assert!(within_tolerance(&geom(104, 196, 636, 484), &target));
        assert!(!within_tolerance(&geom(120, 200, 640, 480), &target));
        assert!(!within_tolerance(&geom(100, 200, 700, 480), &target));
    }

    #[test]
    fn geometry_distance_sums_per_field_abs_diffs() {
        let a = geom(100, 200, 640, 480);
        let b = geom(103, 197, 645, 475);
        // |100-103| + |200-197| + |640-645| + |480-475| = 3+3+5+5 = 16
        assert_eq!(geometry_distance(&a, &b), 16);
        assert_eq!(geometry_distance(&a, &a), 0);
    }

    #[test]
    fn geometry_fallback_picks_the_closest_candidate() {
        // Two windows within tolerance of the target — the closer one must win, not the
        // first one enumerated. Exercised directly against `within_tolerance`/
        // `geometry_distance` rather than `geometry_fallback` itself, which needs a live
        // `CFArray<AXUIElement>` only obtainable on a granted macOS run (Task 6).
        let target = geom(100, 200, 640, 480);
        let near = geom(102, 199, 641, 481);
        let far = geom(106, 194, 645, 475);
        assert!(within_tolerance(&near, &target) && within_tolerance(&far, &target));
        assert!(geometry_distance(&near, &target) < geometry_distance(&far, &target));
    }
}
