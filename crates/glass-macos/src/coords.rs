//! Pure window-relative ↔ global coordinate math. Cross-platform → unit-tested on the
//! Linux dev box.
//!
//! The point↔pixel scale kernel itself lives in [`glass_core::coords`] so glass-macos
//! (capture/window ops) and glass-a11y-macos (the a11y reader) share ONE implementation —
//! a11y bounds and capture/input geometry can never drift apart. This module keeps the
//! window-relative helpers (`to_global`, `check_in_bounds`, `clamp_region`) that only
//! glass-macos needs.

use glass_core::frame::Region;
use glass_core::{GlassError, Result};

// The scale kernel lives in glass-core so glass-macos (capture/window ops) and
// glass-a11y-macos (the a11y reader) share ONE implementation — a11y bounds and
// capture/input geometry can never drift apart. See the a11y-reader design's
// "anti-drift guard" section.
pub use glass_core::coords::{
    global_pixel_to_point, pixel_geometry_from_content_rect, pixel_to_global_point,
    point_to_global_pixel,
};

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
}
