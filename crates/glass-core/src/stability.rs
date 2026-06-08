use crate::diff::diff;
use crate::error::Result;
use crate::frame::Frame;

/// Decides when a stream of captured frames has "settled": `settle_frames`
/// consecutive frames each unchanged (within `tolerance`) from the one before.
pub struct StabilityTracker {
    settle_frames: u32,
    tolerance: u8,
    last: Option<Frame>,
    stable_count: u32,
}

impl StabilityTracker {
    pub fn new(settle_frames: u32, tolerance: u8) -> Self {
        Self { settle_frames: settle_frames.max(1), tolerance, last: None, stable_count: 0 }
    }

    /// Feed the next frame. Returns `true` once the frame stream has settled.
    /// Errors only if frame sizes change mid-stream.
    pub fn observe(&mut self, frame: Frame) -> Result<bool> {
        let unchanged = match &self.last {
            None => false,
            Some(prev) => diff(prev, &frame, self.tolerance)?.changed_pixels == 0,
        };
        self.stable_count = if unchanged { self.stable_count + 1 } else { 0 };
        self.last = Some(frame);
        Ok(self.stable_count >= self.settle_frames)
    }

    /// The most recently observed frame.
    pub fn last(&self) -> Option<&Frame> {
        self.last.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
