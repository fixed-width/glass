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

/// Clamp a window-relative region to the window rect, returning `(x, y, w, h)` with the
/// width/height trimmed so the region never exceeds the window. A region fully outside
/// clamps to zero size at the nearest edge.
pub fn clamp_region(rx: i32, ry: i32, rw: u32, rh: u32, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let x = rx.clamp(0, width as i32) as u32;
    let y = ry.clamp(0, height as i32) as u32;
    let w = rw.min(width.saturating_sub(x));
    let h = rh.min(height.saturating_sub(y));
    (x, y, w, h)
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
}
