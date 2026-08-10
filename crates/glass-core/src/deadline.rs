//! One type for "when the caller stops waiting", shared by every bound in glass.
//!
//! It began as the accessibility readers' `AxDeadline`. `Adb` then grew two more spellings fifteen
//! lines apart — a bare `Instant` for the dump sequence (glass#312) and an `Option<Instant>` for
//! teardown (glass#431) — and `None` reads as "zero" as readily as "unbounded", which is the
//! direction that fails quietly: a call given zero is not run at all (glass#430).
//!
//! Where a bound is *required*, a plain `Instant` is still right and is left alone — the wayland
//! clipboard transfer, `wait_for_agent_until`, `bounded`'s own `poll_until`. This type is for the
//! boundaries where a caller may have no bound to give, so "none" has a name rather than being an
//! empty `Option`.

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

    /// This deadline pulled in by `reserve`, but never by more than half of what is left — for
    /// splitting one budget between steps that run in sequence, where a reserve larger than the
    /// budget would otherwise starve the first step entirely.
    ///
    /// [`Self::UNBOUNDED`] stays unbounded: there is nothing to take a share of. Never lands
    /// before now, so it cannot underflow the monotonic clock the way a bare subtraction can.
    pub fn reserving(self, reserve: std::time::Duration) -> Self {
        Self(self.0.map(|d| {
            let left = d.saturating_duration_since(std::time::Instant::now());
            d - reserve.min(left / 2)
        }))
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

    /// The two bounds a callee holds — its own and its caller's — resolve to whichever falls
    /// first, in both directions.
    #[test]
    fn a_deadline_caps_a_proposed_instant_only_when_it_falls_first() {
        let soon = Deadline::from_millis(1_000);
        let later = std::time::Instant::now() + std::time::Duration::from_secs(60);
        assert!(soon.cap(later) < later, "the nearer bound did not govern");

        let now = std::time::Instant::now();
        assert_eq!(
            soon.cap(now),
            now,
            "a reader's own nearer bound was widened"
        );
    }

    /// A caller that named no instant leaves the callee whatever it proposed — `UNBOUNDED` is not a
    /// deadline of zero, which would stop every call before it started.
    #[test]
    fn no_deadline_caps_nothing_and_is_never_spent() {
        let proposed = std::time::Instant::now() + std::time::Duration::from_secs(60);
        assert_eq!(Deadline::UNBOUNDED.cap(proposed), proposed);
        assert_eq!(Deadline::UNBOUNDED.remaining(), None);
        assert!(!Deadline::UNBOUNDED.has_passed());
    }

    /// The tie-break is load-bearing: a callee blames its own bound when both fall together, so a
    /// backend that hung for exactly its ceiling is never reported as a caller who ran out of time.
    #[test]
    fn a_deadline_governs_a_step_only_when_it_falls_strictly_first() {
        let now = std::time::Instant::now();
        let soon = Deadline::from_millis(1_000);
        assert!(soon.governs(now + std::time::Duration::from_secs(60)));
        assert!(!soon.governs(now));
        assert!(!Deadline::UNBOUNDED.governs(now + std::time::Duration::from_secs(60)));
        // The tie itself, which only a shared instant can express: `<=` here would blame the
        // caller for a backend that hung for exactly its own ceiling.
        assert!(!Deadline::at(now).governs(now));
    }

    #[test]
    fn a_deadline_has_passed_once_its_instant_has_passed() {
        assert!(Deadline::from_millis(0).has_passed());
        assert!(!Deadline::from_millis(60_000).has_passed());
    }

    /// The rule [`crate::run_bounded_until`] exists for, pinned where no timing test can reach it.
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
    fn reserving_takes_a_share_and_leaves_unbounded_unbounded() {
        let left = Deadline::at(Instant::now() + Duration::from_secs(3))
            .reserving(Duration::from_secs(1))
            .remaining()
            .expect("still bounded");
        assert!(
            left <= Duration::from_secs(2) && left > Duration::from_millis(1900),
            "{left:?}"
        );
        assert_eq!(
            Deadline::UNBOUNDED.reserving(Duration::from_secs(1)),
            Deadline::UNBOUNDED
        );
    }

    /// The clamp: a reserve bigger than what is left must not take everything, or the step it was
    /// held back from gets a deadline already in the past and is skipped rather than run.
    #[test]
    fn a_reserve_larger_than_the_budget_takes_only_half() {
        let left = Deadline::at(Instant::now() + Duration::from_millis(100))
            .reserving(Duration::from_secs(30))
            .remaining()
            .expect("still bounded");
        assert!(
            left > Duration::from_millis(30) && left <= Duration::from_millis(50),
            "a 30s reserve out of 100ms should leave about half, left {left:?}"
        );
    }
}
