//! One type for "when the caller stops waiting", shared by every bound in glass.
//!
//! It began as the accessibility readers' `AxDeadline`. Teardown then grew its own spellings — a
//! bare `Instant` in one place and an `Option<Instant>` in another, fifteen lines apart in `Adb`
//! — and `None` reads as "zero" as readily as "unbounded", which is the direction that fails
//! quietly: a call given zero is not run at all (glass#430).
//!
//! Where a bound is *required*, a plain `Instant` is still right and is left alone — `poll_until`,
//! the wayland clipboard transfer, `wait_for_agent_until`. This type is for the boundaries where a
//! caller may have no bound to give, so "none" has a name rather than being an empty `Option`.

/// When the caller stops waiting.
///
/// An accessibility reader cannot be interrupted mid-read — [`crate::Glass::wait_for_element`]
/// re-reads from a synchronous tick — so a 20s `uiautomator dump` answered a 10s wait until
/// readers took this (glass#338). The obligation is stated on
/// [`crate::Accessibility::snapshot`], where an implementer meets it; teardown's is on
/// [`crate::Platform::stop_app_by`].
///
/// [`Self::UNBOUNDED`] leaves the callee its own budget: it is the *widest* value, as `WalkLimits`'
/// unbounded is, and not a deadline of zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct Deadline(Option<std::time::Instant>);

impl Deadline {
    /// The caller named no instant to stop at.
    pub const UNBOUNDED: Self = Self(None);

    /// Stop `ms` milliseconds from now.
    pub fn from_millis(ms: u64) -> Self {
        Self::at(std::time::Instant::now() + std::time::Duration::from_millis(ms))
    }

    /// Stop at `instant` — for a caller that already holds the moment it stops waiting, and for a
    /// test that needs the same instant on both sides of a comparison.
    pub const fn at(instant: std::time::Instant) -> Self {
        Self(Some(instant))
    }

    /// `proposed`, or this deadline when it falls first — the bound one step of a call runs under.
    pub fn cap(self, proposed: std::time::Instant) -> std::time::Instant {
        match self.0 {
            Some(d) => proposed.min(d),
            None => proposed,
        }
    }

    /// Whether this deadline falls before `proposed`, so a step that runs out was cut off rather
    /// than unanswered.
    ///
    /// A tie is deliberately not this deadline: reporting a backend that hung for its whole
    /// ceiling as a spent caller budget hides it.
    pub fn governs(self, proposed: std::time::Instant) -> bool {
        self.0.is_some_and(|d| d < proposed)
    }

    /// How long is left, or `None` when the caller named no instant. Zero once it has passed.
    pub fn remaining(self) -> Option<std::time::Duration> {
        self.0
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
    }

    /// The budget a call actually gets: its own, or what is left of this deadline at `now`,
    /// whichever is nearer — zero once it has passed, since the caller is gone.
    ///
    /// `now` is a parameter rather than read here so an exact test can pin the rule.
    pub fn within(
        self,
        budget: std::time::Duration,
        now: std::time::Instant,
    ) -> std::time::Duration {
        match self.0 {
            Some(d) => budget.min(d.saturating_duration_since(now)),
            None => budget,
        }
    }

    /// This deadline brought `earlier` forward — for splitting one budget between steps that run
    /// in sequence. [`Self::UNBOUNDED`] stays unbounded: there is nothing to take a share of.
    pub fn less(self, earlier: std::time::Duration) -> Self {
        Self(self.0.map(|d| d - earlier))
    }

    /// Whether the caller named an instant that has passed — work started now cannot be wanted.
    ///
    /// `false` for [`Self::UNBOUNDED`] — this asks whether the caller has gone, not whether
    /// budget remains, and a reader reading it as the latter would retry an untimed call forever.
    pub fn has_passed(self) -> bool {
        self.remaining().is_some_and(|left| left.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The rule the deadline-taking runner exists for, pinned where no timing test can reach it.
    #[test]
    fn a_call_gets_the_nearer_of_its_own_budget_and_the_deadline_it_serves() {
        let now = Instant::now();
        assert_eq!(
            Deadline::at(now + Duration::from_secs(5)).within(Duration::from_secs(20), now),
            Duration::from_secs(5)
        );
        assert_eq!(
            Deadline::at(now + Duration::from_secs(5)).within(Duration::from_secs(3), now),
            Duration::from_secs(3)
        );
        assert_eq!(
            Deadline::at(now).within(Duration::from_secs(20), now),
            Duration::ZERO
        );
        let past = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        assert_eq!(
            Deadline::at(past).within(Duration::from_secs(20), now),
            Duration::ZERO
        );
        // No deadline is the widest value, not a budget of zero.
        assert_eq!(
            Deadline::UNBOUNDED.within(Duration::from_secs(20), now),
            Duration::from_secs(20)
        );
    }

    /// A share taken out of one budget, so two steps in sequence cannot each spend the whole.
    #[test]
    fn less_takes_a_share_and_leaves_unbounded_unbounded() {
        let now = Instant::now();
        let left = Deadline::at(now + Duration::from_secs(3))
            .less(Duration::from_secs(1))
            .remaining()
            .expect("still bounded");
        assert!(left <= Duration::from_secs(2) && left > Duration::from_millis(1900));
        assert_eq!(
            Deadline::UNBOUNDED.less(Duration::from_secs(1)),
            Deadline::UNBOUNDED
        );
    }
}
