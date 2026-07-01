//! Pure window-relative ↔ global coordinate math. Cross-platform → unit-tested on the
//! Linux dev box.
//!
//! The whole glass tool boundary — `WindowGeometry`, the captured `Frame`, and
//! click/region coordinates — is backing PIXELS on macOS, matching `glass-x11` and
//! `glass-windows`. Quartz's `CGEvent` APIs, though, address the *global* screen in
//! POINTS, not pixels — so the macOS backend (Plan 3) uses [`pixel_to_global_point`] to
//! map a window-relative PIXEL coordinate to a global POINT before posting a CGEvent.
//! [`pixel_geometry_from_content_rect`] is the mirror-image conversion `scwindow.rs` uses
//! to turn `SCContentFilter`'s `contentRect` (POINTS) + `pointPixelScale` into the PIXEL
//! `WindowGeometry` the tool boundary reports.
//!
//! [`global_pixel_to_point`]/[`point_to_global_pixel`] are Plan 4's ABSOLUTE (no
//! window-origin offset) counterparts, for window `Move`/`Resize`/`Geometry` ops that
//! address the global screen directly via `AXUIElement` rather than a window-relative
//! click.

use glass_core::frame::Region;
use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result};

/// Translate a window-relative point to a global screen point given the window origin.
pub fn to_global(origin: (i32, i32), rel: (i32, i32)) -> (i32, i32) {
    (origin.0 + rel.0, origin.1 + rel.1)
}

/// Reject a window-relative point outside the window (the no-out-of-bounds invariant).
pub fn check_in_bounds(x: i32, y: i32, width: u32, height: u32) -> Result<()> {
    if x < 0 || y < 0 || x as i64 >= width as i64 || y as i64 >= height as i64 {
        return Err(GlassError::CoordOutOfBounds { x, y, width, height });
    }
    Ok(())
}

/// Clamp a window-relative region to the window rect, returning the intersection of the
/// region with the window as a [`Region`]. A region fully outside clamps to zero size at
/// the nearest edge. Computed in `i64` so no cast can wrap for any input.
pub fn clamp_region(rx: i32, ry: i32, rw: u32, rh: u32, width: u32, height: u32) -> Region {
    let (w_i, h_i) = (width as i64, height as i64);
    let left = (rx as i64).clamp(0, w_i);
    let top = (ry as i64).clamp(0, h_i);
    let right = (rx as i64 + rw as i64).clamp(0, w_i);
    let bottom = (ry as i64 + rh as i64).clamp(0, h_i);
    Region { x: left as u32, y: top as u32, width: (right - left) as u32, height: (bottom - top) as u32 }
}

/// Map a window-relative PIXEL coordinate to a global POINT in Quartz's screen space:
/// `origin_pt + rel_px / scale`. `scale` is the window's `pointPixelScale` (`1.0` on a 1x
/// display, `2.0` on 2x Retina) and `origin_pt` is the window's `contentRect.origin` in
/// POINTS (see [`pixel_geometry_from_content_rect`]'s doc for where both come from).
///
/// The macOS backend needs this because CGEvents post into a POINTS-addressed global
/// space, while every tool-boundary coordinate (click/region, `WindowGeometry`, the
/// captured `Frame`) is backing PIXELS — matching `glass-x11`/`glass-windows`. This is the
/// one place a pixel value crosses back into points, right before posting a CGEvent.
pub fn pixel_to_global_point(rel_px: (i32, i32), scale: f64, origin_pt: (f64, f64)) -> (f64, f64) {
    (origin_pt.0 + rel_px.0 as f64 / scale, origin_pt.1 + rel_px.1 as f64 / scale)
}

/// Map an ABSOLUTE global-screen PIXEL coordinate to Quartz's global POINT space:
/// `px / scale`. Unlike [`pixel_to_global_point`] (which is window-RELATIVE and adds a
/// window's `origin_pt`), this has no origin term — it's for Plan 4's window
/// Move/Resize/Geometry ops, which address the global screen directly (a `WindowOp::Move`
/// target, or an `AXUIElement` position/size already in global points), not a click
/// relative to a window's top-left.
pub fn global_pixel_to_point(px: (i32, i32), scale: f64) -> (f64, f64) {
    (px.0 as f64 / scale, px.1 as f64 / scale)
}

/// The inverse of [`global_pixel_to_point`]: map an ABSOLUTE global POINT (e.g. read back
/// from `AXUIElement`'s position/size) to a global PIXEL coordinate: `pt * scale`, rounded
/// to the nearest pixel (ties away from zero, matching
/// [`pixel_geometry_from_content_rect`]'s rounding).
pub fn point_to_global_pixel(pt: (f64, f64), scale: f64) -> (i32, i32) {
    ((pt.0 * scale).round() as i32, (pt.1 * scale).round() as i32)
}

/// Convert a window's `contentRect` (POINTS, from `SCContentFilter.contentRect()`) plus
/// its `pointPixelScale` into a `WindowGeometry` in backing PIXELS — the unit the whole
/// tool boundary uses. `x`/`y` is `contentRect.origin`, `width`/`height` is
/// `contentRect.size`; each field is independently scaled by `scale` and rounded to the
/// nearest pixel, matching how `capture::capture_window` sizes the captured `Frame`
/// (`contentRect.size * pointPixelScale`) — so `scwindow.rs`'s reported geometry and
/// `capture.rs`'s captured frame always agree on width/height in pixels.
///
/// Pure `f64` math (no `CGRect` dependency) so the Retina (2x) scaling is unit-tested here
/// even on a 1x dev box — `scwindow.rs` itself only compiles on macOS.
///
/// Each field rounds independently (`f64::round`, ties away from zero) rather than
/// truncating, so a fractional point value doesn't consistently lose a pixel. Width/height
/// clamp to `0` on a degenerate (negative) input rather than wrapping; `x`/`y` stay signed
/// (a window can sit left-of/above the primary display's origin in a multi-monitor
/// layout).
pub fn pixel_geometry_from_content_rect(x: f64, y: f64, width: f64, height: f64, scale: f64) -> WindowGeometry {
    let px = |v: f64| (v * scale).round();
    WindowGeometry {
        x: px(x) as i32,
        y: px(y) as i32,
        width: px(width).max(0.0) as u32,
        height: px(height).max(0.0) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_global_adds_origin() {
        assert_eq!(to_global((100, 200), (40, 80)), (140, 280));
        assert_eq!(to_global((-5, 0), (10, 10)), (5, 10));
    }

    #[test]
    fn in_bounds_accepts_inside_rejects_outside() {
        assert!(check_in_bounds(0, 0, 640, 480).is_ok());
        assert!(check_in_bounds(639, 479, 640, 480).is_ok());
        assert!(matches!(check_in_bounds(640, 0, 640, 480), Err(GlassError::CoordOutOfBounds { .. })));
        assert!(matches!(check_in_bounds(-1, 0, 640, 480), Err(GlassError::CoordOutOfBounds { .. })));
    }

    #[test]
    fn clamp_region_trims_to_window() {
        assert_eq!(clamp_region(10, 10, 100, 100, 640, 480), Region { x: 10, y: 10, width: 100, height: 100 });
        // width trimmed
        assert_eq!(clamp_region(600, 0, 100, 50, 640, 480), Region { x: 600, y: 0, width: 40, height: 50 });
        // fully outside → 0 width
        assert_eq!(clamp_region(700, 0, 100, 50, 640, 480), Region { x: 640, y: 0, width: 0, height: 50 });
    }

    #[test]
    fn clamp_region_trims_left_top_overhang() {
        // fully outside on the left → zero width (mirror of the right-edge case)
        assert_eq!(clamp_region(-100, 0, 50, 50, 640, 480), Region { x: 0, y: 0, width: 0, height: 50 });
        // partial left overhang → only the in-window portion's width
        assert_eq!(clamp_region(-50, 0, 100, 50, 640, 480), Region { x: 0, y: 0, width: 50, height: 50 });
        // fully outside on the top → zero height
        assert_eq!(clamp_region(0, -100, 50, 50, 640, 480), Region { x: 0, y: 0, width: 50, height: 0 });
    }

    #[test]
    fn pixel_to_global_point_at_1x() {
        assert_eq!(pixel_to_global_point((100, 200), 1.0, (10.0, 20.0)), (110.0, 220.0));
    }

    #[test]
    fn pixel_to_global_point_at_2x_retina() {
        // pixel 200 / 2 = 100pt + origin 10 = 110; pixel 400 / 2 = 200pt + origin 20 = 220.
        assert_eq!(pixel_to_global_point((200, 400), 2.0, (10.0, 20.0)), (110.0, 220.0));
    }

    #[test]
    fn pixel_to_global_point_handles_negative_rel_and_origin() {
        assert_eq!(pixel_to_global_point((-40, -10), 2.0, (-5.0, 0.0)), (-25.0, -5.0));
    }

    #[test]
    fn global_pixel_to_point_is_identity_at_1x() {
        assert_eq!(global_pixel_to_point((100, 200), 1.0), (100.0, 200.0));
    }

    #[test]
    fn global_pixel_to_point_halves_at_2x_retina() {
        assert_eq!(global_pixel_to_point((200, 400), 2.0), (100.0, 200.0));
    }

    #[test]
    fn global_pixel_to_point_handles_negative_pixels() {
        assert_eq!(global_pixel_to_point((-200, -3), 2.0), (-100.0, -1.5));
    }

    #[test]
    fn point_to_global_pixel_is_identity_at_1x() {
        assert_eq!(point_to_global_pixel((100.0, 200.0), 1.0), (100, 200));
    }

    #[test]
    fn point_to_global_pixel_doubles_at_2x_retina() {
        assert_eq!(point_to_global_pixel((100.0, 200.0), 2.0), (200, 400));
    }

    #[test]
    fn point_to_global_pixel_rounds_to_nearest_pixel() {
        // 1.25 * 2 = 2.5 -> 3 (round-half-away-from-zero, per f64::round).
        assert_eq!(point_to_global_pixel((1.25, 1.25), 2.0), (3, 3));
    }

    #[test]
    fn global_pixel_point_round_trip_at_1x_and_2x() {
        for scale in [1.0, 2.0] {
            let px = (137, -42);
            let pt = global_pixel_to_point(px, scale);
            assert_eq!(point_to_global_pixel(pt, scale), px);
        }
    }

    #[test]
    fn pixel_geometry_from_content_rect_is_identity_at_1x() {
        assert_eq!(
            pixel_geometry_from_content_rect(10.0, 20.0, 300.0, 200.0, 1.0),
            WindowGeometry { x: 10, y: 20, width: 300, height: 200 }
        );
    }

    #[test]
    fn pixel_geometry_from_content_rect_scales_at_2x_retina() {
        assert_eq!(
            pixel_geometry_from_content_rect(10.0, 20.0, 300.0, 200.0, 2.0),
            WindowGeometry { x: 20, y: 40, width: 600, height: 400 }
        );
    }

    #[test]
    fn pixel_geometry_from_content_rect_rounds_to_nearest_pixel() {
        // 1*1.5=1.5 -> 2, 3*1.5=4.5 -> 5 (round-half-away-from-zero, per f64::round):
        // fields round independently rather than truncating.
        assert_eq!(
            pixel_geometry_from_content_rect(1.0, 1.0, 3.0, 3.0, 1.5),
            WindowGeometry { x: 2, y: 2, width: 5, height: 5 }
        );
    }

    #[test]
    fn pixel_geometry_from_content_rect_clamps_negative_size_to_zero() {
        // A real contentRect from SCContentFilter never has a negative size, but the
        // conversion must not panic or wrap on malformed input.
        assert_eq!(
            pixel_geometry_from_content_rect(0.0, 0.0, -1.0, -1.0, 2.0),
            WindowGeometry { x: 0, y: 0, width: 0, height: 0 }
        );
    }

    #[test]
    fn pixel_geometry_from_content_rect_preserves_negative_origin() {
        assert_eq!(
            pixel_geometry_from_content_rect(-50.0, -10.0, 100.0, 80.0, 2.0),
            WindowGeometry { x: -100, y: -20, width: 200, height: 160 }
        );
    }
}
