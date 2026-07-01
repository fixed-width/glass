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

use glass_core::coords::pixel_geometry_from_content_rect;
use glass_core::platform::WindowGeometry;
use glass_core::{Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTree, GlassError, Result};
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
        let (window_el, scale) =
            select_window(&windows, &ctx.window).ok_or(GlassError::WindowNotFound)?;

        let mut count = 0usize;
        let root = walk(&window_el, &ctx.window, scale, 0, &mut count);
        // Ids/count are assigned by `glass-core` (`AxTree::assign_ids`) so numbering is
        // identical across OS backends.
        Ok(AxTree { root, count: 0 })
    }
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
    let value = ffi::attribute_string(el, "AXValue");
    // Name = title, else the current value (a titleless text field surfaces its content as
    // its name — the only string `AxTree::to_outline` renders), else the description.
    let name = ffi::attribute_string(el, "AXTitle")
        .or_else(|| value.clone())
        .or_else(|| ffi::attribute_string(el, "AXDescription"));
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
/// reusable predicate so Task 5's `find_nth` prunes identically and its pre-order ids line
/// up with this walk's. A node whose size can't be read is *kept* (its `bounds` become
/// `None`) rather than pruned, so an unreadable-geometry container never silently drops its
/// subtree.
pub(crate) fn should_skip(el: &AXUIElement) -> bool {
    matches!(ffi::ax_size(el), Ok((w, h)) if w <= 0.0 || h <= 0.0)
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
