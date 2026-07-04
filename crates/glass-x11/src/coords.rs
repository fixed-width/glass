use glass_core::{GlassError, Region, Result, WindowGeometry};

/// Translate a window-relative point to absolute root coordinates given the
/// window's origin (top-left) in root coordinates. XTEST motion uses root
/// coordinates, so callers add the window origin (from `translate_coordinates`).
pub fn window_to_root(origin_x: i32, origin_y: i32, x: i32, y: i32) -> (i16, i16) {
    ((origin_x + x) as i16, (origin_y + y) as i16)
}

/// Verify the capture rectangle — the whole window, or `region` within it —
/// lies entirely within the X11 display (root window). glass runs the app on a
/// headless Xvfb of a fixed size; a window taller/wider than that screen (or
/// positioned so part of it is off-screen) makes `GetImage` cover non-viewable
/// area, and X returns a bare `BadMatch`. Detect that here and return an
/// actionable error naming both remedies, instead of surfacing the opaque
/// protocol error the driving agent can't act on.
///
/// `geo` is the window's root-relative origin + size; `display` is the root
/// window's pixel size.
pub(crate) fn check_capture_fits(
    geo: &WindowGeometry,
    region: Option<&Region>,
    display: (u32, u32),
) -> Result<()> {
    let (dw, dh) = (i64::from(display.0), i64::from(display.1));
    // The capture rectangle in window-local coords, then in root/display coords.
    let (cx, cy, w, h) = match region {
        Some(r) => (
            i64::from(r.x),
            i64::from(r.y),
            i64::from(r.width),
            i64::from(r.height),
        ),
        None => (0, 0, i64::from(geo.width), i64::from(geo.height)),
    };
    let (rx, ry) = (i64::from(geo.x) + cx, i64::from(geo.y) + cy);
    if rx >= 0 && ry >= 0 && rx + w <= dw && ry + h <= dh {
        return Ok(());
    }
    // A display that would contain this capture rectangle — a concrete value the
    // operator can drop straight into GLASS_XVFB_SCREEN.
    let need_w = (rx + w).max(dw);
    let need_h = (ry + h).max(dh);
    Err(GlassError::CaptureFailed(format!(
        "capture area {w}x{h} at ({rx},{ry}) extends beyond the {dw}x{dh} X11 display; \
         X11 cannot read the off-screen part (GetImage BadMatch). Resize the window to fit \
         within {dw}x{dh}, or restart glass with a larger display via \
         GLASS_XVFB_SCREEN={need_w}x{need_h}x24."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_origin() {
        assert_eq!(window_to_root(100, 50, 5, 7), (105, 57));
    }

    #[test]
    fn handles_zero_origin() {
        assert_eq!(window_to_root(0, 0, 12, 34), (12, 34));
    }

    #[test]
    fn capture_within_display_is_ok() {
        // An 800x600 window at (10,10) fits well inside a 1280x800 display.
        let geo = WindowGeometry {
            x: 10,
            y: 10,
            width: 800,
            height: 600,
        };
        assert!(check_capture_fits(&geo, None, (1280, 800)).is_ok());
    }

    #[test]
    fn window_exceeding_display_is_actionable_error() {
        // The reproduced case: a 1400x1000 window at (1,1) on a 1280x800 Xvfb.
        let geo = WindowGeometry {
            x: 1,
            y: 1,
            width: 1400,
            height: 1000,
        };
        let msg = check_capture_fits(&geo, None, (1280, 800))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("1400x1000"),
            "names the window/capture size: {msg}"
        );
        assert!(msg.contains("1280x800"), "names the display size: {msg}");
        assert!(
            msg.contains("GLASS_XVFB_SCREEN"),
            "names the larger-display remedy: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("resize"),
            "names the resize remedy: {msg}"
        );
    }

    #[test]
    fn onscreen_region_of_an_oversized_window_still_succeeds() {
        // The window is taller than the display (its lower 200px are off-screen),
        // but a region on its visible upper part maps fully on-screen — partial
        // capture of a too-big window must still work, not be rejected wholesale.
        let geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let region = Region {
            x: 0,
            y: 0,
            width: 500,
            height: 500,
        };
        assert!(check_capture_fits(&geo, Some(&region), (1280, 800)).is_ok());
    }

    #[test]
    fn region_reaching_the_offscreen_part_of_an_oversized_window_is_caught() {
        // Same oversized window; this region fits the window (y 600..900 <= 1000)
        // but reaches below the 800px display edge, so its lower rows are off-root.
        let geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let region = Region {
            x: 0,
            y: 600,
            width: 500,
            height: 300,
        };
        let msg = check_capture_fits(&geo, Some(&region), (1280, 800))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("1280x800"), "names the display size: {msg}");
    }

    #[test]
    fn window_flush_to_display_edges_is_ok() {
        // Exactly display-sized at the origin — the boundary case must pass.
        let geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        assert!(check_capture_fits(&geo, None, (1280, 800)).is_ok());
    }
}
