//! `Glass` frame capture: screenshot and region capture.
use super::*;

impl Glass {
    /// Capture the active window, or — when `window` is set — a different
    /// window's region WITHOUT changing which window is active (unlike
    /// `select_window`). `region` is relative to whichever window is captured.
    pub fn screenshot(
        &mut self,
        region: Option<Region>,
        window: Option<WindowId>,
    ) -> Result<Frame> {
        self.capture(window, region.as_ref())
    }

    /// Capture `window`'s region (or, when `None`, the active window's), pumping
    /// logs afterward either way. A specific window's own geometry governs its
    /// capture — the backend validates `id` and any region against it — so the
    /// active window's cached `s.geometry` is only consulted for the `None` case.
    // pub(super): also used by the `wait` and `a11y` submodules to grab frames.
    pub(super) fn capture(
        &mut self,
        window: Option<WindowId>,
        region: Option<&Region>,
    ) -> Result<Frame> {
        let s = self.active_mut()?;
        let frame = match window {
            Some(id) => s.platform.capture_window(id, region)?,
            None => {
                if let Some(r) = region {
                    r.check_fits(s.geometry.width, s.geometry.height)?;
                }
                s.platform.capture_frame(region)?
            }
        };
        s.pump();
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;

    #[test]
    fn screenshot_returns_backend_frame() {
        let frame = Frame::solid(4, 4, [7, 7, 7, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![frame.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert_eq!(g.screenshot(None, None).unwrap(), frame);
    }

    #[test]
    fn screenshot_with_region_returns_subrectangle() {
        let frame = Frame::solid(4, 4, [7, 7, 7, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![frame]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let out = g
            .screenshot(
                Some(Region {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                }),
                None,
            )
            .unwrap();
        assert_eq!((out.width, out.height), (2, 2));
    }

    #[test]
    fn screenshot_region_out_of_bounds_is_rejected() {
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let err = g
            .screenshot(
                Some(Region {
                    x: 0,
                    y: 0,
                    width: 9,
                    height: 1,
                }),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn screenshot_with_window_id_captures_that_window_without_changing_active() {
        // Two windows: A (active) and B. screenshot(None, Some(B.id)) must return
        // B's frame — via capture_window, NOT capture_frame — while the session's
        // active window (still A) is left untouched.
        let frame_b = Frame::solid(8, 8, [9, 9, 9, 255]);
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 100,
                y: 0,
                width: 8,
                height: 8,
            },
            active: false,
        };
        let capture_log = Arc::new(Mutex::new(Vec::new()));
        let capture_window_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_windows(vec![a.clone(), b.clone()])
            .with_capture_log(capture_log.clone())
            .with_capture_window_log(capture_window_log.clone())
            .with_window_frame(WindowId(2), frame_b.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.select_window(WindowId(1)).unwrap(); // A is active

        let out = g.screenshot(None, Some(WindowId(2))).unwrap();
        assert_eq!(out, frame_b, "screenshot(window: B) returns B's frame");
        assert_eq!(
            g.geometry().unwrap(),
            a.geometry,
            "active window is still A after capturing B"
        );
        assert!(
            capture_log.lock().unwrap().is_empty(),
            "capturing a specific window must not go through capture_frame"
        );
        assert_eq!(
            *capture_window_log.lock().unwrap(),
            vec![(WindowId(2), None)]
        );
    }

    #[test]
    fn screenshot_with_unknown_window_id_errors() {
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.screenshot(None, Some(WindowId(999))).unwrap_err(),
            GlassError::WindowNotFound
        ));
    }
}
