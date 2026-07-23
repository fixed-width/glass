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
    /// `ignore` rects are window-relative animated regions to exclude from the
    /// comparison — pixels there never count as changed. When `region` is set,
    /// each rect is intersected with it and translated into region-local
    /// coordinates, so the caller always passes window-relative rects.
    pub fn diff_baseline(
        &mut self,
        name: &str,
        region: Option<&Region>,
        ignore: &[Region],
        tolerance: u8,
    ) -> Result<DiffResult> {
        self.diff_baseline_with_frame(name, region, ignore, tolerance)
            .map(|(r, _)| r)
    }

    /// Like [`diff_baseline`] but also returns the current frame that was compared.
    pub fn diff_baseline_with_frame(
        &mut self,
        name: &str,
        region: Option<&Region>,
        ignore: &[Region],
        tolerance: u8,
    ) -> Result<(DiffResult, Frame)> {
        let (baseline, current) = self.baseline_and_current(name, region)?;
        let mask = mask_for(ignore, region, baseline.width, baseline.height)?;
        let r = diff_with_mask(&baseline, &current, tolerance, &mask)?;
        Ok((r, current))
    }

    /// Perceptual diff (YIQ + anti-alias suppression) against a saved baseline —
    /// the default for regression, robust to anti-aliasing / sub-pixel / GPU-font
    /// rendering noise. `threshold` ∈ [0,1] (smaller = stricter). `region` scopes
    /// the comparison to a window-relative sub-rectangle. `ignore` behaves as in
    /// [`diff_baseline`].
    pub fn diff_baseline_perceptual(
        &mut self,
        name: &str,
        region: Option<&Region>,
        ignore: &[Region],
        threshold: f32,
    ) -> Result<DiffResult> {
        self.diff_baseline_perceptual_with_frame(name, region, ignore, threshold)
            .map(|(r, _)| r)
    }

    /// Like [`diff_baseline_perceptual`] but also returns the current frame compared.
    pub fn diff_baseline_perceptual_with_frame(
        &mut self,
        name: &str,
        region: Option<&Region>,
        ignore: &[Region],
        threshold: f32,
    ) -> Result<(DiffResult, Frame)> {
        let (baseline, current) = self.baseline_and_current(name, region)?;
        let mask = mask_for(ignore, region, baseline.width, baseline.height)?;
        let r = diff_perceptual_with_mask(&baseline, &current, threshold, &mask)?;
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
        let result = g.diff_baseline("main", None, &[], 0).unwrap();
        assert_eq!(result.changed_pixels, 1);
    }

    #[test]
    fn diff_missing_baseline_errors() {
        let platform =
            FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.diff_baseline("absent", None, &[], 0).unwrap_err(),
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
            g.diff_baseline("m", Some(&top_left), &[], 0)
                .unwrap()
                .changed_pixels,
            0
        );
        // Region includes the changed pixel -> sees exactly it.
        assert_eq!(
            g.diff_baseline("m", Some(&bottom_right), &[], 0)
                .unwrap()
                .changed_pixels,
            1
        );
        // Whole-frame diff still sees it.
        assert_eq!(
            g.diff_baseline("m", None, &[], 0).unwrap().changed_pixels,
            1
        );
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
                &[],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn diff_baseline_honours_an_ignore_rect() {
        let baseline_frame = Frame::solid(8, 8, [0, 0, 0, 255]);
        let mut changed = baseline_frame.clone();
        // Change one pixel inside the masked band and one outside it.
        changed.pixels[9 * 4] = 255; // (1,1), masked
        changed.pixels[45 * 4] = 255; // (5,5), visible
        let platform = FakePlatform::new(8, 8).with_frames(vec![baseline_frame, changed]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("b").unwrap();
        let ignore = [Region {
            x: 0,
            y: 0,
            width: 8,
            height: 3,
        }];
        let r = g.diff_baseline("b", None, &ignore, 0).unwrap();
        assert_eq!(r.changed_pixels, 1, "the masked change must not count");
        assert_eq!(r.ignored_pixels, 24);
    }

    #[test]
    fn baseline_ignore_is_window_relative_under_a_region() {
        let baseline_frame = Frame::solid(8, 8, [0, 0, 0, 255]);
        let mut changed = baseline_frame.clone();
        changed.pixels[45 * 4] = 255; // (5,5)
        let platform = FakePlatform::new(8, 8).with_frames(vec![baseline_frame, changed]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("b").unwrap();
        let region = Region {
            x: 4,
            y: 4,
            width: 4,
            height: 4,
        };
        // Window-relative mask over (5,5) -- must translate into region space.
        let ignore = [Region {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        }];
        let r = g.diff_baseline("b", Some(&region), &ignore, 0).unwrap();
        assert_eq!(r.changed_pixels, 0);
        assert_eq!(r.ignored_pixels, 1);
        assert_eq!(r.total_pixels, 16, "region is 4x4");
    }
}
