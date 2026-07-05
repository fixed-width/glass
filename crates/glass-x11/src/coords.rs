use glass_core::{GlassError, Region, Result, WindowGeometry};

/// Translate a window-relative point to absolute root coordinates given the
/// window's origin (top-left) in root coordinates. XTEST motion uses root
/// coordinates, so callers add the window origin (from `translate_coordinates`).
pub fn window_to_root(origin_x: i32, origin_y: i32, x: i32, y: i32) -> (i16, i16) {
    ((origin_x + x) as i16, (origin_y + y) as i16)
}

/// A capture rectangle fitted to the X11 display: the on-display root-coordinate
/// sub-rectangle actually read by `GetImage`, plus whether it was shrunk from
/// what the caller asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClippedRect {
    /// Root-coordinate top-left of the portion to capture.
    pub sx: i32,
    pub sy: i32,
    /// Size of the on-display portion (always > 0).
    pub w: u32,
    pub h: u32,
    /// True when the requested rectangle reached off the display and was shrunk
    /// to its on-display part (the returned frame is therefore a partial view).
    pub clipped: bool,
}

/// Fit the requested capture rectangle — the whole window, or `region` within
/// it — to the X11 display (root window), returning the on-display portion.
///
/// glass runs the app on a headless Xvfb of a fixed size; a window taller/wider
/// than that screen (or positioned so part of it is off-screen) makes `GetImage`
/// cover non-viewable area, and X rejects it with a bare `BadMatch`. Rather than
/// fail the whole capture, intersect the requested rectangle with the display
/// and read only the visible part — a partial capture is strictly more useful
/// than none, and is exactly the surface (an off-screen-anchored popover) whose
/// contents the caller most wants to see. A rectangle lying *entirely* off the
/// display has nothing to show and stays an actionable error.
///
/// `geo` is the window's root-relative origin + size; `display` is the root
/// window's pixel size.
pub(crate) fn clip_capture_to_display(
    geo: &WindowGeometry,
    region: Option<&Region>,
    display: (u32, u32),
) -> Result<ClippedRect> {
    let (dw, dh) = (i64::from(display.0), i64::from(display.1));
    // The requested rectangle in window-local coords, then in root/display coords.
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
    // Intersect the requested rectangle with the display [0,dw) x [0,dh).
    let ix = rx.max(0);
    let iy = ry.max(0);
    let iw = (rx + w).min(dw) - ix;
    let ih = (ry + h).min(dh) - iy;
    if iw <= 0 || ih <= 0 {
        // A display that would contain the requested rectangle — a concrete value
        // the operator can drop straight into GLASS_XVFB_SCREEN.
        let need_w = (rx + w).max(dw);
        let need_h = (ry + h).max(dh);
        return Err(GlassError::CaptureFailed(format!(
            "capture area {w}x{h} at ({rx},{ry}) lies entirely outside the {dw}x{dh} X11 \
             display; there is nothing on-screen to read (GetImage BadMatch). Reposition or \
             resize the window to overlap the display, or restart glass with a larger display \
             via GLASS_XVFB_SCREEN={need_w}x{need_h}x24."
        )));
    }
    let clipped = ix != rx || iy != ry || iw != w || ih != h;
    Ok(ClippedRect {
        sx: ix as i32,
        sy: iy as i32,
        w: iw as u32,
        h: ih as u32,
        clipped,
    })
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
    fn capture_within_display_is_unclipped() {
        // An 800x600 window at (10,10) fits well inside a 1280x800 display: the
        // whole window is read, nothing is clipped.
        let geo = WindowGeometry {
            x: 10,
            y: 10,
            width: 800,
            height: 600,
        };
        let r = clip_capture_to_display(&geo, None, (1280, 800)).unwrap();
        assert_eq!(
            r,
            ClippedRect {
                sx: 10,
                sy: 10,
                w: 800,
                h: 600,
                clipped: false
            }
        );
    }

    #[test]
    fn window_exceeding_display_is_clipped_to_visible() {
        // Formerly a hard error: a 1400x1000 window at (1,1) on a 1280x800 Xvfb.
        // Now the on-display 1279x799 portion is read and flagged clipped, so the
        // caller gets a partial frame instead of BadMatch.
        let geo = WindowGeometry {
            x: 1,
            y: 1,
            width: 1400,
            height: 1000,
        };
        let r = clip_capture_to_display(&geo, None, (1280, 800)).unwrap();
        assert_eq!(
            r,
            ClippedRect {
                sx: 1,
                sy: 1,
                w: 1279,
                h: 799,
                clipped: true
            }
        );
    }

    #[test]
    fn onscreen_region_of_an_oversized_window_is_unclipped() {
        // The window is taller than the display (its lower 200px are off-screen),
        // but a region on its visible upper part maps fully on-screen — a partial
        // capture of a too-big window reads exactly the region, unclipped.
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
        let r = clip_capture_to_display(&geo, Some(&region), (1280, 800)).unwrap();
        assert_eq!(
            r,
            ClippedRect {
                sx: 0,
                sy: 0,
                w: 500,
                h: 500,
                clipped: false
            }
        );
    }

    #[test]
    fn region_reaching_the_offscreen_part_of_an_oversized_window_is_clipped() {
        // Same oversized window; this region fits the window (y 600..900 <= 1000)
        // but reaches below the 800px display edge, so its lower 100 rows are
        // off-root. The visible 500x200 top of the region is read and clipped.
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
        let r = clip_capture_to_display(&geo, Some(&region), (1280, 800)).unwrap();
        assert_eq!(
            r,
            ClippedRect {
                sx: 0,
                sy: 600,
                w: 500,
                h: 200,
                clipped: true
            }
        );
    }

    #[test]
    fn negative_origin_popover_is_clipped_to_visible() {
        // The issue repro: an override-redirect popover anchored partly off the
        // left edge (x:-3). Its left 3 columns are off-root; the visible 323x135
        // is read (sx snapped to 0) and flagged clipped, not a BadMatch.
        let geo = WindowGeometry {
            x: -3,
            y: 25,
            width: 326,
            height: 135,
        };
        let r = clip_capture_to_display(&geo, None, (1280, 800)).unwrap();
        assert_eq!(
            r,
            ClippedRect {
                sx: 0,
                sy: 25,
                w: 323,
                h: 135,
                clipped: true
            }
        );
    }

    #[test]
    fn rectangle_entirely_off_display_is_actionable_error() {
        // A window pushed fully past the right edge has no on-screen pixels: a
        // partial capture is impossible, so this stays an actionable error.
        let geo = WindowGeometry {
            x: 2000,
            y: 0,
            width: 100,
            height: 100,
        };
        let msg = clip_capture_to_display(&geo, None, (1280, 800))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("1280x800"), "names the display size: {msg}");
        assert!(
            msg.contains("GLASS_XVFB_SCREEN"),
            "names the larger-display remedy: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("outside"),
            "explains nothing is on-screen: {msg}"
        );
    }

    #[test]
    fn window_flush_to_display_edges_is_unclipped() {
        // Exactly display-sized at the origin — the boundary case reads the whole
        // window with nothing clipped.
        let geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        let r = clip_capture_to_display(&geo, None, (1280, 800)).unwrap();
        assert_eq!(
            r,
            ClippedRect {
                sx: 0,
                sy: 0,
                w: 1280,
                h: 800,
                clipped: false
            }
        );
    }
}
