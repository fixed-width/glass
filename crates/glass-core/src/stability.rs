use crate::diff::{diff_with_mask, IgnoreMask};
use crate::error::Result;
use crate::frame::Frame;

/// Decides when a stream of captured frames has "settled": `settle_frames`
/// consecutive frames each unchanged (within `tolerance`) from the one before.
///
/// Settling means "the last N sampled frames were identical" — NOT "nothing will ever
/// change." A slow or sub-sampling-rate animation can hold one phase across the sampled
/// window and read as settled; [`saw_change`](Self::saw_change) records whether any change
/// was observed during this watch so callers can tell a quiet-the-whole-time settle from
/// one that simply caught a still moment.
pub struct StabilityTracker {
    settle_frames: u32,
    tolerance: u8,
    /// Pixels excluded from the settle comparison — a perpetually animating region
    /// (a blinking caret, a clock) can toggle here forever without ever preventing
    /// a settle.
    mask: IgnoreMask,
    last: Option<Frame>,
    stable_count: u32,
    saw_change: bool,
}

impl StabilityTracker {
    pub fn new(settle_frames: u32, tolerance: u8) -> Self {
        Self::with_mask(settle_frames, tolerance, IgnoreMask::empty())
    }

    /// Like [`new`](Self::new), but pixels covered by `mask` are excluded from the
    /// frame-to-frame comparison — so a perpetually animating region (a blinking
    /// caret, a clock) cannot prevent the stream from settling.
    pub fn with_mask(settle_frames: u32, tolerance: u8, mask: IgnoreMask) -> Self {
        Self {
            settle_frames: settle_frames.max(1),
            tolerance,
            mask,
            last: None,
            stable_count: 0,
            saw_change: false,
        }
    }

    /// Feed the next frame. Returns `true` once the frame stream has settled.
    /// Errors only if frame sizes change mid-stream.
    pub fn observe(&mut self, frame: Frame) -> Result<bool> {
        let had_prev = self.last.is_some();
        let unchanged = match &self.last {
            None => false,
            Some(prev) => {
                diff_with_mask(prev, &frame, self.tolerance, &self.mask)?.changed_pixels == 0
            }
        };
        if had_prev && !unchanged {
            self.saw_change = true;
        }
        self.stable_count = if unchanged { self.stable_count + 1 } else { 0 };
        self.last = Some(frame);
        Ok(self.stable_count >= self.settle_frames)
    }

    /// The most recently observed frame.
    pub fn last(&self) -> Option<&Frame> {
        self.last.as_ref()
    }

    /// Whether any frame-to-frame change was observed during this watch. `false` after a
    /// settle means the window was quiet throughout — but a watch shorter than an
    /// animation's period can still miss it, so this is a hint, not a guarantee of idleness.
    pub fn saw_change(&self) -> bool {
        self.saw_change
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Region;

    #[test]
    fn settles_after_consecutive_identical_frames() {
        let f = Frame::solid(2, 2, [9, 9, 9, 255]);
        let mut t = StabilityTracker::new(2, 0);
        assert!(!t.observe(f.clone()).unwrap()); // first frame, no prior
        assert!(!t.observe(f.clone()).unwrap()); // 1 stable comparison
        assert!(t.observe(f.clone()).unwrap()); // 2 stable comparisons
    }

    #[test]
    fn change_resets_the_counter() {
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        let mut t = StabilityTracker::new(2, 0);
        t.observe(a.clone()).unwrap();
        t.observe(a.clone()).unwrap(); // count = 1
        assert!(!t.observe(b.clone()).unwrap()); // changed -> reset to 0
        assert!(!t.observe(b.clone()).unwrap()); // count = 1
        assert!(t.observe(b.clone()).unwrap()); // count = 2
    }

    #[test]
    fn last_returns_latest_frame() {
        let a = Frame::solid(1, 1, [1, 1, 1, 255]);
        let b = Frame::solid(1, 1, [2, 2, 2, 255]);
        let mut t = StabilityTracker::new(1, 0);
        t.observe(a).unwrap();
        t.observe(b.clone()).unwrap();
        assert_eq!(t.last(), Some(&b));
    }

    #[test]
    fn a_masked_blinking_pixel_still_counts_as_settled() {
        let mask = IgnoreMask::new(
            &[Region {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
            4,
            4,
        )
        .unwrap();
        let mut t = StabilityTracker::with_mask(2, 0, mask);
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut blink = base.clone();
        blink.pixels[0] = 255; // only the masked pixel toggles

        assert!(!t.observe(base.clone()).unwrap());
        assert!(!t.observe(blink.clone()).unwrap());
        assert!(
            t.observe(base.clone()).unwrap(),
            "the toggling pixel is masked, so the stream is stable"
        );
    }

    #[test]
    fn an_unmasked_blinking_pixel_never_settles() {
        let mut t = StabilityTracker::new(2, 0);
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut blink = base.clone();
        blink.pixels[0] = 255;
        for _ in 0..6 {
            assert!(!t.observe(base.clone()).unwrap());
            assert!(!t.observe(blink.clone()).unwrap());
        }
    }

    #[test]
    fn saw_change_distinguishes_quiet_from_animated_settles() {
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        // Quiet throughout: settles with saw_change == false.
        let mut quiet = StabilityTracker::new(2, 0);
        quiet.observe(a.clone()).unwrap();
        quiet.observe(a.clone()).unwrap();
        assert!(quiet.observe(a.clone()).unwrap());
        assert!(!quiet.saw_change(), "no change seen -> saw_change is false");
        // Moved, then quieted: still settles, but saw_change == true.
        let mut moved = StabilityTracker::new(2, 0);
        moved.observe(a.clone()).unwrap();
        moved.observe(b.clone()).unwrap(); // a change
        moved.observe(b.clone()).unwrap();
        assert!(moved.observe(b.clone()).unwrap());
        assert!(
            moved.saw_change(),
            "a change occurred -> saw_change is true"
        );
    }
}
