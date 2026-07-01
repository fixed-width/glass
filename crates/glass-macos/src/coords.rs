//! Pure window-relative ↔ global coordinate math. Cross-platform → unit-tested on the
//! Linux dev box. The macOS backend (Plans 2–4) maps window-relative tool coordinates to
//! global screen coordinates here before posting CGEvents or capturing a sub-region.

use glass_core::frame::Region;
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

/// Convert a window-relative `region` from POINTS to backing PIXELS by scaling every field
/// by `scale` (macOS's `pointPixelScale`: `1.0` on a 1x display, `2.0` on 2x Retina).
///
/// The macOS capture backend needs this because its two coordinate sources disagree:
/// `capture::capture_window`'s `Frame` is sized in backing pixels
/// (`contentRect.size * pointPixelScale`), but the `region` passed to it is window-relative
/// points — the unit `WindowGeometry` (from `SCWindow.frame()`) and the session layer's
/// region validation both use. Cropping a points-sized region straight against a
/// pixels-sized `Frame` silently reads the wrong (quarter-sized, on 2x) sub-image. Pure and
/// cross-platform so the Retina (2x) math is unit-tested here even on a 1x dev box; the
/// wider points-vs-pixels reconciliation across geometry/frame/click (the other backends
/// use physical pixels throughout) is a later coordinate-design item, not solved by this
/// one conversion.
///
/// Each field rounds to the nearest pixel independently (`f64::round`, ties away from
/// zero) rather than truncating, so a fractional point value doesn't consistently lose a
/// pixel versus the frame it's cropped against.
pub fn scale_region(region: &Region, scale: f64) -> Region {
    let scaled = |v: u32| (v as f64 * scale).round() as u32;
    Region {
        x: scaled(region.x),
        y: scaled(region.y),
        width: scaled(region.width),
        height: scaled(region.height),
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
    fn scale_region_is_identity_at_1x() {
        let region = Region { x: 10, y: 20, width: 300, height: 200 };
        assert_eq!(scale_region(&region, 1.0), region);
    }

    #[test]
    fn scale_region_doubles_at_2x_retina() {
        let region = Region { x: 10, y: 20, width: 300, height: 200 };
        assert_eq!(scale_region(&region, 2.0), Region { x: 20, y: 40, width: 600, height: 400 });
    }

    #[test]
    fn scale_region_rounds_to_nearest_pixel() {
        // 1*1.5=1.5 -> 2, 3*1.5=4.5 -> 5 (round-half-away-from-zero, per f64::round):
        // fields round independently rather than truncating.
        let region = Region { x: 1, y: 1, width: 3, height: 3 };
        assert_eq!(scale_region(&region, 1.5), Region { x: 2, y: 2, width: 5, height: 5 });
    }
}
