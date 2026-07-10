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
