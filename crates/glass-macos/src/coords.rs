//! Pure window-relative ↔ global coordinate math. Cross-platform → unit-tested on the
//! Linux dev box. The macOS backend (Plans 2–4) maps window-relative tool coordinates to
//! global screen coordinates here before posting CGEvents or capturing a sub-region.

use glass_core::GlassError;

/// Translate a window-relative point to a global screen point given the window origin.
pub fn to_global(origin: (i32, i32), rel: (i32, i32)) -> (i32, i32) {
    (origin.0 + rel.0, origin.1 + rel.1)
}

/// Reject a window-relative point outside the window (the no-out-of-bounds invariant).
pub fn check_in_bounds(x: i32, y: i32, width: u32, height: u32) -> Result<(), GlassError> {
    if x < 0 || y < 0 || x as i64 >= width as i64 || y as i64 >= height as i64 {
        return Err(GlassError::CoordOutOfBounds { x, y, width, height });
    }
    Ok(())
}

/// Clamp a window-relative region to the window rect, returning `(x, y, w, h)` = the
/// intersection of the region with the window. A region fully outside clamps to zero
/// size at the nearest edge. Computed in `i64` so no cast can wrap for any input.
pub fn clamp_region(rx: i32, ry: i32, rw: u32, rh: u32, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let (w_i, h_i) = (width as i64, height as i64);
    let left = (rx as i64).clamp(0, w_i);
    let top = (ry as i64).clamp(0, h_i);
    let right = (rx as i64 + rw as i64).clamp(0, w_i);
    let bottom = (ry as i64 + rh as i64).clamp(0, h_i);
    (left as u32, top as u32, (right - left) as u32, (bottom - top) as u32)
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
        assert_eq!(clamp_region(10, 10, 100, 100, 640, 480), (10, 10, 100, 100));
        assert_eq!(clamp_region(600, 0, 100, 50, 640, 480), (600, 0, 40, 50)); // width trimmed
        assert_eq!(clamp_region(700, 0, 100, 50, 640, 480), (640, 0, 0, 50)); // fully outside → 0 width
    }

    #[test]
    fn clamp_region_trims_left_top_overhang() {
        // fully outside on the left → zero width (mirror of the right-edge case)
        assert_eq!(clamp_region(-100, 0, 50, 50, 640, 480), (0, 0, 0, 50));
        // partial left overhang → only the in-window portion's width
        assert_eq!(clamp_region(-50, 0, 100, 50, 640, 480), (0, 0, 50, 50));
        // fully outside on the top → zero height
        assert_eq!(clamp_region(0, -100, 50, 50, 640, 480), (0, 0, 50, 0));
    }
}
