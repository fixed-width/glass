//! `Glass` baselines: save and diff against a stored frame.
use super::*;

impl Glass {
    pub fn save_baseline(&mut self, name: &str) -> Result<()> {
        let frame = {
            let s = self.active_mut()?;
            let frame = s.platform.capture_frame(None)?;
            s.pump();
            frame
        };
        self.baselines.save(name, &frame)
    }

    /// Load the named baseline and capture the current window frame, both scoped
    /// to `region` when set (the whole window otherwise). Baselines are stored
    /// whole and cropped here, so one saved baseline can be compared against any
    /// sub-region — and both operands are always cropped consistently, never
    /// silently mismatched.
    fn baseline_and_current(
        &mut self,
        name: &str,
        region: Option<&Region>,
    ) -> Result<(Frame, Frame)> {
        if let Some(r) = region {
            let geo = self.require_active()?.geometry.clone();
            r.check_fits(geo.width, geo.height)?;
        }
        let baseline = {
            let base = self.baselines.load(name)?;
            match region {
                Some(r) => base.crop(r)?,
                None => base,
            }
        };
        let current = {
            let s = self.active_mut()?;
            let frame = s.platform.capture_frame(region)?;
            s.pump();
            frame
        };
        Ok((baseline, current))
    }

    /// Exact per-channel diff of the current frame against a saved baseline.
    /// `region` scopes the comparison to a window-relative sub-rectangle.
    pub fn diff_baseline(
        &mut self,
        name: &str,
        region: Option<&Region>,
        tolerance: u8,
    ) -> Result<DiffResult> {
        self.diff_baseline_with_frame(name, region, tolerance)
            .map(|(r, _)| r)
    }

    /// Like [`diff_baseline`] but also returns the current frame that was compared.
    pub fn diff_baseline_with_frame(
        &mut self,
        name: &str,
        region: Option<&Region>,
        tolerance: u8,
    ) -> Result<(DiffResult, Frame)> {
        let (baseline, current) = self.baseline_and_current(name, region)?;
        let r = diff(&baseline, &current, tolerance)?;
        Ok((r, current))
    }

    /// Perceptual diff (YIQ + anti-alias suppression) against a saved baseline —
    /// the default for regression, robust to anti-aliasing / sub-pixel / GPU-font
    /// rendering noise. `threshold` ∈ [0,1] (smaller = stricter). `region` scopes
    /// the comparison to a window-relative sub-rectangle.
    pub fn diff_baseline_perceptual(
        &mut self,
        name: &str,
        region: Option<&Region>,
        threshold: f32,
    ) -> Result<DiffResult> {
        self.diff_baseline_perceptual_with_frame(name, region, threshold)
            .map(|(r, _)| r)
    }

    /// Like [`diff_baseline_perceptual`] but also returns the current frame compared.
    pub fn diff_baseline_perceptual_with_frame(
        &mut self,
        name: &str,
        region: Option<&Region>,
        threshold: f32,
    ) -> Result<(DiffResult, Frame)> {
        let (baseline, current) = self.baseline_and_current(name, region)?;
        let r = diff_perceptual(&baseline, &current, threshold)?;
        Ok((r, current))
    }
}

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;

    #[test]
    fn save_then_diff_baseline_reports_change() {
        let baseline_frame = Frame::solid(2, 2, [0, 0, 0, 255]);
        let mut changed = baseline_frame.clone();
        changed.pixels[0] = 255;
        // capture #1 -> save baseline; capture #2 -> diff against it.
        let platform = FakePlatform::new(2, 2).with_frames(vec![baseline_frame.clone(), changed]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("main").unwrap();
        let result = g.diff_baseline("main", None, 0).unwrap();
        assert_eq!(result.changed_pixels, 1);
    }

    #[test]
    fn diff_missing_baseline_errors() {
        let platform =
            FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.diff_baseline("absent", None, 0).unwrap_err(),
            GlassError::BaselineMissing(_)
        ));
    }

    #[test]
    fn diff_region_scopes_comparison_to_subrectangle() {
        // A single whole baseline is compared against several sub-regions: the
        // baseline is stored whole and cropped per-call, so both operands always
        // cover the same rectangle.
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[(3 * 4 + 3) * 4] = 255; // pixel (3,3)
        let platform = FakePlatform::new(4, 4).with_frames(vec![base, changed]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("m").unwrap();
        let top_left = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let bottom_right = Region {
            x: 2,
            y: 2,
            width: 2,
            height: 2,
        };
        // Region excludes the changed pixel -> no change.
        assert_eq!(
            g.diff_baseline("m", Some(&top_left), 0)
                .unwrap()
                .changed_pixels,
            0
        );
        // Region includes the changed pixel -> sees exactly it.
        assert_eq!(
            g.diff_baseline("m", Some(&bottom_right), 0)
                .unwrap()
                .changed_pixels,
            1
        );
        // Whole-frame diff still sees it.
        assert_eq!(g.diff_baseline("m", None, 0).unwrap().changed_pixels, 1);
    }

    #[test]
    fn diff_region_out_of_bounds_is_rejected() {
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![base.clone(), base]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("m").unwrap();
        let err = g
            .diff_baseline(
                "m",
                Some(&Region {
                    x: 0,
                    y: 0,
                    width: 9,
                    height: 1,
                }),
                0,
            )
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }
}
