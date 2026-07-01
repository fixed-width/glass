//! The macOS `AXUIElement` read primitives the accessibility snapshot needs, and the
//! Accessibility-grant predicate that gates it.
//!
//! This crate is deliberately standalone — it does **not** depend on `glass-macos` — so the
//! small subset of AX read wrappers it needs are re-implemented here rather than imported.
//! Every objc2 call shape is ported verbatim from `glass-macos/src/axwindow.rs`
//! (window position/size/children reads) and `glass-macos/src/permissions.rs` (the
//! `AXIsProcessTrusted` grant gate); see those modules' docs for the fuller rationale behind
//! each idiom. All of this crate's `unsafe` lives in this module, each block carrying the
//! same `// SAFETY:` justification its `axwindow.rs`/`permissions.rs` counterpart does;
//! `reader.rs` stays `unsafe`-free.
//!
//! ## CFType memory: every AX "Copy" call returns a +1 ref
//!
//! `AXUIElementCopyAttributeValue` follows Core Foundation's Copy/Create ownership rule —
//! the caller owns the returned ref. [`copy_attribute`] wraps the raw `*const CFType` in a
//! `CFRetained<CFType>` via `CFRetained::from_raw` (no extra retain), so it is released
//! automatically on drop; nothing here calls `CFRelease` manually. Mirrors
//! `axwindow::copy_attribute`.
//!
//! ## `objc2-application-services`: the `kAX*` name constants are empty stubs
//!
//! `header-translator` produced empty generated files for the `AXAttributeConstants`/
//! `AXRoleConstants` features, so the `kAXRoleAttribute`/`kAXTitleAttribute`/... symbols do
//! not exist. Every attribute name here is built as a `CFString::from_str` literal
//! (`"AXRole"`, `"AXTitle"`, ...) — the stable documented strings — exactly as `axwindow.rs`
//! does.

use std::ptr::NonNull;

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType, CGPoint, CGSize};

use glass_core::{GlassError, Result};

// Plain C predicates/queries not surfaced (or not surfaced in the shape we want) by the
// objc2 bindings, declared with the same contained `extern "C"` pattern `permissions.rs`
// (`AXIsProcessTrusted`) and `axwindow.rs` (`_AXUIElementGetWindow`) use. Both are public,
// documented, stable AX APIs (unlike `axwindow`'s private symbol), so no version-fragility
// caveat applies.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    // Apple declares this `Boolean` (= `unsigned char`), NOT C99 `_Bool`. Binding it as
    // `u8` and comparing `!= 0` avoids Rust-`bool`'s validity invariant (only 0/1 are legal
    // bit patterns), matching `permissions.rs`.
    fn AXIsProcessTrusted() -> u8;
    // `AXError AXUIElementIsAttributeSettable(AXUIElementRef, CFStringRef, Boolean*)` — the
    // documented way to tell whether an attribute (e.g. `AXValue`) can be written, which is
    // how the reader derives `editable`/`focusable`. `Boolean` out-param is `u8` for the
    // same reason as above.
    fn AXUIElementIsAttributeSettable(
        element: &AXUIElement,
        attribute: &CFString,
        settable: *mut u8,
    ) -> AXError;
    // `AXError AXUIElementSetAttributeValue(AXUIElementRef, CFStringRef, CFTypeRef)` — the
    // documented way to write an attribute. [`set_string_value`] uses it directly with a
    // `CFString` value for `AXValue`: a plain string write, unlike `AXPosition`/`AXSize`
    // (which wrap a `CGPoint`/`CGSize` in an `AXValueCreate`-built `AXValue` — see
    // `axwindow::set_axvalue` — a step that only applies to those geometry types, not
    // strings).
    fn AXUIElementSetAttributeValue(
        element: &AXUIElement,
        attribute: &CFString,
        value: &CFType,
    ) -> AXError;
}

/// True if this process holds the Accessibility (AX) TCC grant. The snapshot gate calls
/// this first and fails closed (never returns a stub tree) when it is false.
pub(crate) fn accessibility_is_trusted() -> bool {
    // SAFETY: `AXIsProcessTrusted` is a no-argument C predicate over this process's trust
    // state; no preconditions, no side effects. Returns `Boolean` (u8); any nonzero value
    // means trusted (mirrors `permissions.rs::accessibility_granted`).
    unsafe { AXIsProcessTrusted() != 0 }
}

/// The application-level `AXUIElement` for `pid` — the root the window search starts from.
pub(crate) fn app_element(pid: i32) -> CFRetained<AXUIElement> {
    // SAFETY: `AXUIElementCreateApplication` never returns NULL per Apple's documented
    // contract (the binding itself `.expect()`s on this); `pid` is a plain process id with
    // no aliasing/lifetime preconditions (mirrors `axwindow::ax_window_for_cgwindowid`).
    unsafe { AXUIElement::new_application(pid) }
}

/// `AXUIElementCopyAttributeValue(el, attr_name, ...)`, wrapping the already-retained (+1,
/// per Core Foundation's Copy/Create rule) raw result in a `CFRetained<CFType>` so it is
/// released automatically when dropped. Errors (`AXError` != `Success`) become a structured
/// [`GlassError::Backend`] naming the attribute — an absent attribute is one such error, so
/// the value accessors below (`attribute_string`/`attribute_bool`) treat that as "no value"
/// rather than a hard failure.
pub(crate) fn copy_attribute(el: &AXUIElement, attr_name: &str) -> Result<CFRetained<CFType>> {
    let attr = CFString::from_str(attr_name);
    let mut raw: *const CFType = std::ptr::null();
    // SAFETY: `el` is a live `AXUIElement`; `raw` is a valid local out-param slot matching
    // `AXUIElementCopyAttributeValue`'s documented signature (mirrors
    // `axwindow::copy_attribute`).
    let err = unsafe { el.copy_attribute_value(&attr, NonNull::from(&mut raw)) };
    if err != AXError::Success {
        return Err(ax_err(attr_name, err));
    }
    let nn = NonNull::new(raw.cast_mut())
        .ok_or_else(|| GlassError::Backend(format!("{attr_name}: AX reported success but returned a null value")))?;
    // SAFETY: `AXUIElementCopyAttributeValue` follows Core Foundation's Copy/Create
    // ownership rule — an already-retained (+1) `CFTypeRef` on success — so
    // `CFRetained::from_raw` takes ownership without an extra retain (mirrors `axwindow`).
    Ok(unsafe { CFRetained::from_raw(nn) })
}

/// Read `el`'s `attr_name` as a `String`, or `None` when the attribute is absent, isn't a
/// `CFString`, or is the empty string. A missing attribute is a normal state (most nodes
/// lack most attributes), so this collapses to `None` rather than surfacing an error.
pub(crate) fn attribute_string(el: &AXUIElement, attr_name: &str) -> Option<String> {
    let value = copy_attribute(el, attr_name).ok()?;
    let text = value.downcast::<CFString>().ok()?.to_string();
    (!text.is_empty()).then_some(text)
}

/// Read `el`'s `attr_name` as a `bool` (`CFBoolean`), or `None` when the attribute is absent
/// or isn't a boolean.
pub(crate) fn attribute_bool(el: &AXUIElement, attr_name: &str) -> Option<bool> {
    let value = copy_attribute(el, attr_name).ok()?;
    value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
}

/// Whether `attr_name` is writable on `el` (`AXUIElementIsAttributeSettable`). `false` on
/// any AX error, including the attribute being absent — the reader uses this for `editable`
/// (settable `AXValue`) and `focusable` (settable `AXFocused`).
pub(crate) fn is_settable(el: &AXUIElement, attr_name: &str) -> bool {
    let attr = CFString::from_str(attr_name);
    let mut settable: u8 = 0;
    // SAFETY: `el` is a live `AXUIElement`; `attr` is a valid `CFString`; `settable` is a
    // valid local out-param matching `AXUIElementIsAttributeSettable`'s documented
    // `Boolean *` parameter.
    let err = unsafe { AXUIElementIsAttributeSettable(el, &attr, &mut settable) };
    err == AXError::Success && settable != 0
}

/// Write `text` as `el`'s `AXValue` (`AXUIElementSetAttributeValue`). The caller
/// (`reader::set_value`) gates this on [`is_settable`] first; this only performs the write —
/// it does not read back to verify the value actually took (that honesty check is the
/// caller's read-back poll, mirroring the Windows reader's `set_value` contract).
pub(crate) fn set_string_value(el: &AXUIElement, text: &str) -> Result<()> {
    let attr = CFString::from_str("AXValue");
    let value = CFString::from_str(text);
    // SAFETY: `el` is a live `AXUIElement`; `attr`/`value` are valid `CFString`s. `value`
    // deref-coerces `CFRetained<CFString>` -> `CFString` -> `CFType` (the same two-hop
    // coercion `axwindow::set_axvalue` already relies on for its `CFRetained<AXValue>`
    // argument), matching `AXUIElementSetAttributeValue`'s documented `CFTypeRef` parameter.
    let err = unsafe { AXUIElementSetAttributeValue(el, &attr, &value) };
    if err != AXError::Success {
        return Err(ax_err("AXValue", err));
    }
    Ok(())
}

/// Read `el`'s `AXPosition` (top-left, in points — Quartz's top-left-origin global screen
/// space, the same unit `AXSize` and `glass_core::coords` convert to/from pixels).
pub(crate) fn ax_position(el: &AXUIElement) -> Result<(f64, f64)> {
    let value = axvalue(el, "AXPosition")?;
    let mut point = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: `value` was just verified (via `downcast`) to be a real `AXValue`; `point` is
    // a valid local out-param whose type matches the requested `AXValueType::CGPoint`
    // (mirrors `axwindow::ax_position`).
    let ok = unsafe { value.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()) };
    if !ok {
        return Err(GlassError::Backend("AXValueGetValue(AXPosition, .cgPoint) returned false".into()));
    }
    Ok((point.x, point.y))
}

/// Read `el`'s `AXSize` (width/height, in points).
pub(crate) fn ax_size(el: &AXUIElement) -> Result<(f64, f64)> {
    let value = axvalue(el, "AXSize")?;
    let mut size = CGSize { width: 0.0, height: 0.0 };
    // SAFETY: same as `ax_position` above, with `AXValueType::CGSize`/`CGSize`.
    let ok = unsafe { value.value(AXValueType::CGSize, NonNull::from(&mut size).cast()) };
    if !ok {
        return Err(GlassError::Backend("AXValueGetValue(AXSize, .cgSize) returned false".into()));
    }
    Ok((size.width, size.height))
}

/// `el`'s `AXChildren` as a `Vec` of typed element refs (array order preserved). Errors if
/// the attribute copy fails; the reader treats that as "no children" (a leaf).
pub(crate) fn children(el: &AXUIElement) -> Result<Vec<CFRetained<AXUIElement>>> {
    array_of_elements(el, "AXChildren")
}

/// `app`'s `AXWindows` as a `Vec` of typed element refs.
pub(crate) fn app_windows(app: &AXUIElement) -> Result<Vec<CFRetained<AXUIElement>>> {
    array_of_elements(app, "AXWindows")
}

/// Shared body of [`children`]/[`app_windows`]: copy an element-array attribute, reinterpret
/// it as a typed `CFArray<AXUIElement>`, and collect its entries.
fn array_of_elements(el: &AXUIElement, attr_name: &str) -> Result<Vec<CFRetained<AXUIElement>>> {
    let value = copy_attribute(el, attr_name)?;
    let arr = value
        .downcast::<CFArray>()
        .map_err(|_| GlassError::Backend(format!("{attr_name} did not return a CFArray")))?;
    // SAFETY: `AXChildren`/`AXWindows` are documented by Apple to hold `AXUIElementRef`s;
    // this only attaches compile-time element-type information (no runtime effect) — the
    // same technique `axwindow::ax_windows` uses (`CFRetained::cast_unchecked`).
    let typed: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
    Ok(typed.iter().collect())
}

/// Copy `el`'s `attr_name` value, downcast-checked to a concrete `AXValue` (position/size
/// are always `AXValue`-wrapped `CGPoint`/`CGSize`, never a bare `CFType`).
fn axvalue(el: &AXUIElement, attr_name: &str) -> Result<CFRetained<AXValue>> {
    let value = copy_attribute(el, attr_name)?;
    value
        .downcast::<AXValue>()
        .map_err(|_| GlassError::Backend(format!("{attr_name} did not return an AXValue")))
}

/// Build a [`GlassError::Backend`] naming both the failing AX attribute and the raw
/// `AXError` code, so a diagnostic doesn't collapse every AX failure into one opaque
/// message (mirrors `axwindow::ax_backend_err`).
fn ax_err(context: &str, err: AXError) -> GlassError {
    GlassError::Backend(format!("{context}: AX call failed (AXError {})", err.0))
}
