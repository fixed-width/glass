#![forbid(unsafe_code)]
//! When a value read repeatedly has stopped changing.
//!
//! macOS reports a window's geometry while the window is still opening, so the reading taken at
//! adoption is routinely a frame of the open animation rather than the window's real size (#263).
//! The caller polls; this decides when to stop. Kept generic and out of the `#[cfg(target_os =
//! "macos")]` modules so the rule is unit-tested on any host, and so `glass-core` could adopt it
//! if the other backends turn out to race the same way.

/// What one more reading tells the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettleStep<T> {
    /// This reading matched the one before it. Stop polling; this is the value.
    Settled(T),
    /// No agreement yet. Poll again, until the caller's own budget runs out.
    Continue,
}

/// Feeds readings in, one at a time, and reports the first time two consecutive readings agree.
///
/// Two samples rather than three or a fixed duration: a window that never animated agrees on its
/// second reading and costs one extra poll, which is the common case on every launch.
#[derive(Clone, Debug, Default)]
pub struct SettleTracker<T> {
    previous: Option<T>,
}

impl<T: Clone + PartialEq> SettleTracker<T> {
    pub fn new() -> Self {
        Self { previous: None }
    }

    /// Record `reading` and report whether it settles the sequence.
    pub fn observe(&mut self, reading: T) -> SettleStep<T> {
        let settled = self.previous.as_ref() == Some(&reading);
        self.previous = Some(reading.clone());
        if settled {
            SettleStep::Settled(reading)
        } else {
            SettleStep::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SettleStep, SettleTracker};

    /// A single reading can never settle: "settled" means two readings agreed, and there is
    /// nothing yet to agree with.
    #[test]
    fn the_first_reading_never_settles() {
        let mut t = SettleTracker::new();
        assert_eq!(t.observe(10), SettleStep::Continue);
    }

    /// The ordinary case, and the one that keeps the hot path cheap: a window that was already
    /// stable settles on its second reading.
    #[test]
    fn two_equal_readings_settle() {
        let mut t = SettleTracker::new();
        t.observe(10);
        assert_eq!(t.observe(10), SettleStep::Settled(10));
    }

    /// A window still animating keeps producing new values; only once it stops does a pair
    /// agree, and the value returned is the one that agreed — not the first reading.
    #[test]
    fn a_changing_reading_settles_on_the_later_value() {
        let mut t = SettleTracker::new();
        assert_eq!(t.observe(10), SettleStep::Continue);
        assert_eq!(t.observe(20), SettleStep::Continue);
        assert_eq!(t.observe(20), SettleStep::Settled(20));
    }

    /// A window whose geometry oscillates never settles, which is what makes the caller's
    /// budget load-bearing rather than decorative.
    #[test]
    fn alternating_readings_never_settle() {
        let mut t = SettleTracker::new();
        for (i, r) in [10, 20, 10, 20, 10].into_iter().enumerate() {
            assert_eq!(t.observe(r), SettleStep::Continue, "reading {i}");
        }
    }

    /// Equality is by value, not by position: a value that recurs after a change still has to
    /// agree with its immediate predecessor.
    #[test]
    fn agreement_is_with_the_immediate_predecessor_only() {
        let mut t = SettleTracker::new();
        t.observe(10);
        t.observe(20);
        assert_eq!(t.observe(10), SettleStep::Continue);
    }
}
