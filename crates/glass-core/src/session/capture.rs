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
