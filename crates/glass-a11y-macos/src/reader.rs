//! `MacosA11y`: the `AXUIElement` accessibility reader behind `glass-core`'s
//! [`Accessibility`] seam. Given the launched app's pid and the active window's pixel
//! geometry (from the display backend), it selects the matching `AXWindow`, recovers the
//! point→pixel scale, and walks the element subtree pre-order into a normalized [`AxTree`]
//! in window-relative pixels.
//!
//! **Runs inline on the caller's thread** — unlike the Linux (AT-SPI) and Windows (UIA)
//! readers, AX has no thread-affinity requirement that forces a worker thread, and the
//! on-box test binary already drives this from the process's true main thread. All `unsafe`
//! FFI lives in [`crate::ffi`]; this module is `unsafe`-free.
//!
//! **Fails closed, never stubs.** A missing Accessibility grant is a
//! [`GlassError::PermissionDenied`]; no matching window (including an empty pid set) is a
//! [`GlassError::WindowNotFound`]. It never returns an empty/placeholder tree.

use std::time::{Duration, Instant};

use glass_core::coords::pixel_geometry_from_content_rect;
use glass_core::platform::WindowGeometry;
use glass_core::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTarget, AxTree, GlassError, Result,
};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFRetained;

use crate::ffi;
use crate::mapping::{self, AxStateFacts};

/// Deepest subtree level walked. Bounds work on a pathological/cyclic tree together with
/// [`MAX_NODES`]; sized generously so it never truncates a real macOS UI.
const MAX_DEPTH: usize = 30;
/// Global cap on nodes *entered* — the hard ceiling on snapshot size.
const MAX_NODES: usize = 1500;
/// Per-level cap on siblings *examined* (on- or off-screen). [`MAX_NODES`] only counts
/// entered nodes and [`should_skip`] siblings are skipped without entering, so an
/// all-skipped level (a virtualized list of thousands) could iterate without ever tripping
/// `MAX_NODES`. Cap the per-level scan so the walk is genuinely bounded regardless of
/// breadth (mirrors the Windows reader).
const MAX_SIBLINGS: usize = 4096;

/// Per-axis pixel tolerance when matching an `AXWindow`'s origin against the backend's
/// reported window origin. Same basis as `axwindow.rs`'s geometry-match fallback.
const POSITION_TOLERANCE_PX: i32 = 8;
/// Slack (pixels) allowed between the backend's reported window height and the height the
/// width-derived `scale` predicts for the `AXWindow`. The scale is taken from *width*
/// because a title bar makes the AX frame height exceed the captured content height; this
/// slack absorbs that difference (generous enough to cover a title bar + toolbar even at 2x
/// Retina) while still rejecting a window whose height is wildly inconsistent with the
/// scale. Position + width already pin the single-window case; this is a secondary guard.
const HEIGHT_CONSISTENCY_SLACK_PX: f64 = 96.0;

/// Per-edge pixel tolerance for `set_value`'s bounds fingerprint (guards a stale id after
/// tree drift landing a same-role+name element elsewhere) — same basis as the Windows
/// reader's `SET_VALUE_BOUNDS_TOL`.
const SET_VALUE_BOUNDS_TOL: i64 = 12;
/// How long `set_value` polls the `AXValue` read-back for the write to take before declaring
/// it a no-op. Mirrors the Windows reader's `SET_VALUE_VERIFY_MS`.
const SET_VALUE_VERIFY_MS: u64 = 800;
/// Interval between read-back poll attempts.
const SET_VALUE_POLL_MS: u64 = 20;

/// Remedy text for a missing Accessibility grant. Kept in sync with `glass-macos`'s
/// `permissions.rs` wording (this crate can't depend on that private module).
const ACCESSIBILITY_REMEDY: &str =
    "enable glass in System Settings > Privacy & Security > Accessibility";

/// The macOS accessibility reader. Zero-sized; a fresh AX read is performed per `snapshot`.
#[derive(Debug, Default)]
pub struct MacosA11y;

impl MacosA11y {
    pub fn new() -> Self {
        Self
    }
}

impl Accessibility for MacosA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let (window_el, scale) = resolve_window(ctx)?;

        let mut count = 0usize;
        let root = walk(&window_el, &ctx.window, scale, 0, &mut count);
        // Ids/count are assigned by `glass-core` (`AxTree::assign_ids`) so numbering is
        // identical across OS backends.
        Ok(AxTree { root, count: 0 })
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let (window_el, scale) = resolve_window(ctx)?;

        // Start at 0 so `find_nth`'s pre-order numbering matches `snapshot`'s `walk` +
        // `AxTree::assign_ids` (root id = 0); the role+name+bounds fingerprint below
        // backstops any residual drift between the snapshot and this re-walk.
        let mut count = 0usize;
        let el = find_nth(window_el, 0, &mut count, target.id.0)
            .ok_or(GlassError::AxElementNotFound(target.id.0))?;

        // Verify role + name + bounds (guards a stale id / tree drift): if drift landed a
        // different same-role+name element on this pre-order id, its bounds sit elsewhere
        // and it is rejected here rather than silently overwritten.
        let ax_role = ffi::attribute_string(&el, "AXRole").unwrap_or_default();
        let role = mapping::map_role(&ax_role);
        let name =
            ffi::attribute_string(&el, "AXTitle").or_else(|| ffi::attribute_string(&el, "AXDescription"));
        let bounds = window_relative_rect(&el, scale, &ctx.window);
        if !target.matches(role, name.as_deref())
            || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
        {
            return Err(GlassError::AxElementChanged(target.id.0));
        }

        if !ffi::is_settable(&el, "AXValue") {
            return Err(GlassError::AxElementNotEditable(target.id.0));
        }

        let before = ffi::attribute_string(&el, "AXValue").unwrap_or_default();
        ffi::set_string_value(&el, text)?;

        // Read-back poll: some editables accept the AX write without an `AXError` but never
        // actually change `AXValue` (a misleading success) — require the read-back to show
        // the change before reporting success, never a silent false-success.
        let deadline = Instant::now() + Duration::from_millis(SET_VALUE_VERIFY_MS);
        loop {
            let after = ffi::attribute_string(&el, "AXValue").unwrap_or_default();
            if set_value_took(&before, &after, text) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(GlassError::AxValueNotApplied(target.id.0));
            }
            std::thread::sleep(Duration::from_millis(SET_VALUE_POLL_MS));
        }
    }
}

/// Resolve the `AXWindow` + point→pixel `scale` for `ctx`: the grant gate, pid, app element,
/// and window selection `snapshot` and `set_value` both need — shared so both address the
/// identical window.
fn resolve_window(ctx: &AxContext) -> Result<(CFRetained<AXUIElement>, f64)> {
    // Grant gate first — fail closed with an actionable error, never a stub tree.
    if !ffi::accessibility_is_trusted() {
        return Err(GlassError::PermissionDenied {
            which: "Accessibility".into(),
            remedy: ACCESSIBILITY_REMEDY.into(),
        });
    }

    let &pid = ctx.pids.first().ok_or(GlassError::WindowNotFound)?;
    let app = ffi::app_element(pid as i32);
    // A failed `AXWindows` read is "no windows" → fall through to `WindowNotFound`.
    let windows = ffi::app_windows(&app).unwrap_or_default();
    select_window(&windows, &ctx.window).ok_or(GlassError::WindowNotFound)
}

/// Select the `AXWindow` matching the backend's reported `win` and recover its point→pixel
/// `scale`. The scale comes from *width* (`win.width / ax_width_pts`); the window matches
/// when its `AXPosition` (scaled to pixels) lands within [`POSITION_TOLERANCE_PX`] of
/// `win`'s origin AND its height is consistent with that scale (within
/// [`HEIGHT_CONSISTENCY_SLACK_PX`]). Among candidates, the closest origin wins. `None` when
/// nothing matches (fail closed).
fn select_window(
    windows: &[CFRetained<AXUIElement>],
    win: &WindowGeometry,
) -> Option<(CFRetained<AXUIElement>, f64)> {
    let mut best: Option<(i32, CFRetained<AXUIElement>, f64)> = None;
    for w in windows {
        let Ok((ax_w, ax_h)) = ffi::ax_size(w) else { continue };
        if ax_w <= 0.0 || ax_h <= 0.0 {
            continue;
        }
        let scale = win.width as f64 / ax_w;
        if !scale.is_finite() || scale <= 0.0 {
            continue;
        }
        let Ok((ax_x, ax_y)) = ffi::ax_position(w) else { continue };
        let dx = ((ax_x * scale).round() as i32 - win.x).abs();
        let dy = ((ax_y * scale).round() as i32 - win.y).abs();
        if dx > POSITION_TOLERANCE_PX || dy > POSITION_TOLERANCE_PX {
            continue;
        }
        if (win.height as f64 - ax_h * scale).abs() > HEIGHT_CONSISTENCY_SLACK_PX {
            continue;
        }
        let dist = dx + dy;
        if best.as_ref().is_none_or(|(best_dist, _, _)| dist < *best_dist) {
            best = Some((dist, w.clone(), scale));
        }
    }
    best.map(|(_, w, scale)| (w, scale))
}

/// Pre-order walk: build this element's [`AxNode`], then recurse into its (non-skipped)
/// children in array order. `count` is the running node total, incremented on entry and
/// shared across the whole walk so [`MAX_NODES`] bounds the entire tree.
fn walk(
    el: &AXUIElement,
    win: &WindowGeometry,
    scale: f64,
    depth: usize,
    count: &mut usize,
) -> AxNode {
    *count += 1;

    let ax_role = ffi::attribute_string(el, "AXRole").unwrap_or_default();
    let role = mapping::map_role(&ax_role);
    // `AXRoleDescription` is the human-readable role ("button", "text field"); fall back to
    // the raw AX role string so `raw_role` is never empty.
    let raw_role = ffi::attribute_string(el, "AXRoleDescription").unwrap_or(ax_role);
    // Name = title, else description — both stable labels (e.g. `setAccessibilityLabel`
    // surfaces as `AXDescription`). Never fold in `AXValue`: it's volatile content, and a
    // node's name must stay stable for the `AxTarget` fingerprint `set_value` relies on.
    let name = ffi::attribute_string(el, "AXTitle").or_else(|| ffi::attribute_string(el, "AXDescription"));
    let value = ffi::attribute_string(el, "AXValue");
    let bounds = window_relative_rect(el, scale, win);
    let states = mapping::map_states(&gather_states(el));

    let mut children = Vec::new();
    if depth < MAX_DEPTH && *count < MAX_NODES {
        let mut siblings = 0usize;
        for child in ffi::children(el).unwrap_or_default() {
            siblings += 1;
            if siblings > MAX_SIBLINGS {
                break;
            }
            if !should_skip(&child) {
                children.push(walk(&child, win, scale, depth + 1, count));
            }
            if *count >= MAX_NODES {
                break;
            }
        }
    }

    AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role,
        raw_role,
        name,
        value,
        states,
        bounds,
        children,
    }
}

/// Whether to prune `el` from the walk: it has no positive-area geometry (zero-size /
/// collapsed / offscreen), so it is neither clickable nor useful in the outline. A named,
/// reusable predicate so [`find_nth`] prunes identically and its pre-order ids line up with
/// this walk's. A node whose size can't be read is *kept* (its `bounds` become `None`)
/// rather than pruned, so an unreadable-geometry container never silently drops its subtree.
pub(crate) fn should_skip(el: &AXUIElement) -> bool {
    matches!(ffi::ax_size(el), Ok((w, h)) if w <= 0.0 || h <= 0.0)
}

/// Pre-order walk mirroring [`walk`]'s traversal — same `should_skip` predicate, same
/// `AXChildren` order, same `MAX_DEPTH`/`MAX_NODES`/`MAX_SIBLINGS` bounds — to locate the
/// element at pre-order index `target`. That is the same numbering
/// `glass_core::AxTree::assign_ids` gives the tree `snapshot` returns (root = 0), so a
/// `target.id` captured from a snapshot lands on the same element here. `count` doubles as
/// the running id (a node's id is `count`'s value on arrival, before it increments) and the
/// `MAX_NODES` bound, identically to `walk`. Takes (and, on a mismatch, drops) ownership of
/// each candidate rather than borrowing, since a matched child must outlive the `Vec` of
/// siblings `ffi::children` returns.
fn find_nth(
    el: CFRetained<AXUIElement>,
    depth: usize,
    count: &mut usize,
    target: u32,
) -> Option<CFRetained<AXUIElement>> {
    let my_id = *count as u32;
    *count += 1;
    if my_id == target {
        return Some(el);
    }
    if depth < MAX_DEPTH && *count < MAX_NODES {
        let mut siblings = 0usize;
        for child in ffi::children(&el).unwrap_or_default() {
            siblings += 1;
            if siblings > MAX_SIBLINGS {
                break; // same per-level bound as walk(), so find_nth can't spin either
            }
            if !should_skip(&child) {
                if let Some(found) = find_nth(child, depth + 1, count, target) {
                    return Some(found);
                }
            }
            if *count >= MAX_NODES {
                break;
            }
        }
    }
    None
}

/// `el`'s window-relative bounds in pixels, or `None` when position/size can't be read or
/// the element has zero area. Shares `glass_core::coords`'s point→pixel conversion with the
/// capture/input path so a11y bounds and click geometry can't drift.
fn window_relative_rect(el: &AXUIElement, scale: f64, win: &WindowGeometry) -> Option<AxRect> {
    let (pos_x, pos_y) = ffi::ax_position(el).ok()?;
    let (size_w, size_h) = ffi::ax_size(el).ok()?;
    let g = pixel_geometry_from_content_rect(pos_x, pos_y, size_w, size_h, scale);
    if g.width == 0 || g.height == 0 {
        return None;
    }
    Some(AxRect { x: g.x - win.x, y: g.y - win.y, width: g.width, height: g.height })
}

/// Gather the plain state facts `mapping::map_states` normalizes: `AXEnabled`/`AXFocused`
/// (boolean attributes) and `editable`/`focusable` (whether `AXValue`/`AXFocused` are
/// writable). The remaining facts stay at their defaults — macOS doesn't expose them as
/// simple universal attributes, and the reader never over-claims a state it didn't read.
fn gather_states(el: &AXUIElement) -> AxStateFacts {
    AxStateFacts {
        enabled: ffi::attribute_bool(el, "AXEnabled").unwrap_or(false),
        focused: ffi::attribute_bool(el, "AXFocused").unwrap_or(false),
        focusable: ffi::is_settable(el, "AXFocused"),
        editable: ffi::is_settable(el, "AXValue"),
        ..Default::default()
    }
}

/// Whether a `set_value` write actually took, judged from the value read back. Some
/// read-only-in-practice editables accept the AX write without an `AXError` but never change
/// `AXValue` (a misleading success); a real set changes the value, possibly to a reformatted
/// string. So it took iff the read-back equals the request OR differs from the pre-set
/// value. Mirrors the Windows reader's `set_value_took`.
fn set_value_took(before: &str, after: &str, requested: &str) -> bool {
    after == requested || after != before
}

#[cfg(test)]
mod tests {
    use super::set_value_took;

    #[test]
    fn noop_is_not_taken() {
        // A read-only editable: value unchanged AND not the requested text.
        assert!(!set_value_took("hello", "hello", "world"));
    }

    #[test]
    fn exact_match_took() {
        assert!(set_value_took("hello", "world", "world"));
    }

    #[test]
    fn reformatted_change_took() {
        // The AX field may normalize the written text — changed from before, so it took.
        assert!(set_value_took("0", "0.0", "0"));
    }

    #[test]
    fn setting_current_value_is_taken() {
        // Edge case: requesting the value it already holds → equals request → taken.
        assert!(set_value_took("world", "world", "world"));
    }
}
