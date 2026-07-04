//! Window-relative <-> physical screen, and physical -> normalized (0..65535) for SendInput.

/// Map a window-relative point to absolute screen pixels given the window's DWM frame origin.
pub fn window_to_screen(win_origin: (i32, i32), rel: (i32, i32)) -> (i32, i32) {
    (win_origin.0 + rel.0, win_origin.1 + rel.1)
}

/// Map a screen pixel to the 0..65535 normalized virtual-desktop space SendInput expects.
/// `vorigin`/`vsize` are GetSystemMetrics(SM_X/Y/CX/CYVIRTUALSCREEN). The caller guarantees
/// `p` lies within `[vorigin, vorigin + vsize)` (coords are validated against window bounds
/// before reaching the backend); out-of-range input is not clamped.
pub fn screen_to_normalized(vorigin: (i32, i32), vsize: (i32, i32), p: (i32, i32)) -> (i32, i32) {
    let nx = ((p.0 - vorigin.0) as i64 * 65535 / (vsize.0.max(2) as i64 - 1)) as i32;
    let ny = ((p.1 - vorigin.1) as i64 * 65535 / (vsize.1.max(2) as i64 - 1)) as i32;
    (nx, ny)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_relative_origin_maps_to_window_corner() {
        assert_eq!(window_to_screen((100, 50), (0, 0)), (100, 50));
        assert_eq!(window_to_screen((100, 50), (10, 20)), (110, 70));
    }
    #[test]
    fn normalized_maps_corners_to_full_range() {
        // single 1920x1080 monitor at origin
        assert_eq!(screen_to_normalized((0, 0), (1920, 1080), (0, 0)), (0, 0));
        let (nx, ny) = screen_to_normalized((0, 0), (1920, 1080), (1919, 1079));
        assert_eq!((nx, ny), (65535, 65535));
    }
    #[test]
    fn normalized_handles_virtual_desktop_offset() {
        // a second monitor to the right: virtual origin stays (0,0), width spans both
        let (nx, _) = screen_to_normalized((0, 0), (3840, 1080), (3839, 0));
        assert_eq!(nx, 65535);
    }
    #[test]
    fn normalized_subtracts_virtual_origin() {
        // a monitor to the LEFT of primary: the virtual origin is negative, so the
        // subtraction (not a no-op here) must place its left edge at 0 and right at 65535.
        assert_eq!(
            screen_to_normalized((-1920, 0), (3840, 1080), (-1920, 0)),
            (0, 0)
        );
        let (nx, _) = screen_to_normalized((-1920, 0), (3840, 1080), (1919, 0));
        assert_eq!(nx, 65535);
    }
    #[test]
    fn normalized_degenerate_size_does_not_panic() {
        // vsize of 1 (or 0) must not divide by zero; the .max(2) - 1 guard makes the divisor 1.
        assert_eq!(screen_to_normalized((0, 0), (1, 1), (0, 0)), (0, 0));
    }
}
