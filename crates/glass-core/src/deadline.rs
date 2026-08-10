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

    /// The instant a step is bounded by — the nearer of this deadline and `own`, the callee's own
    /// bound — and whose bound that is.
    ///
    /// One comparison answers both, so the instant and the blame cannot come from different
    /// proposals (glass#432), and the blame is settled before the step runs rather than inferred
    /// from the error it returns (glass#341).
    ///
    /// A tie is the callee's: one that used exactly its own bound must not be reported as a caller
    /// who ran out of time.
    ///
    /// The only instant-vs-instant comparison this type makes. [`Self::within`] is the same
    /// question in duration units; a third spelling of "whichever falls first" is a bug, not a
    /// convenience (glass#432).
    pub fn resolve(self, own: std::time::Instant) -> (std::time::Instant, Whose) {
        match self.0 {
            Some(d) if d < own => (d, Whose::Caller),
            _ => (own, Whose::Callee),
        }
    }

    /// How long is left, or `None` when the caller named no instant. Zero once it has passed.
    pub fn remaining(self) -> Option<std::time::Duration> {
        self.remaining_at(std::time::Instant::now())
    }

    /// [`Self::remaining`] measured from `now` — for a caller holding a fixed reference point,
    /// where reading the clock again would subtract whatever it has since spent.
    pub fn remaining_at(self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.0.map(|d| d.saturating_duration_since(now))
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
        // Not `cap(now + budget)`: that materialises an instant, and a budget near
        // `Duration::MAX` overflows the addition. Same comparison, in duration units.
        self.remaining_at(now)
            .map_or(budget, |left| budget.min(left))
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

/// Whose bound a step ran under: settled before it starts, never inferred from how it ended
/// (glass#341). [`Deadline::resolve`] answers it where both a caller's bound and the callee's own
/// exist; a step bounded by one side alone names that side directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Whose {
    /// The caller's deadline fell first: a spent budget, not a fault.
    Caller,
    /// The callee's own bound fell first, or level with the caller's — the caller still had time.
    Callee,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `UNBOUNDED` is not a deadline of zero, which would stop every call before it started.
    #[test]
    fn an_unbounded_deadline_is_never_spent() {
        assert_eq!(Deadline::UNBOUNDED.remaining(), None);
        assert!(!Deadline::UNBOUNDED.has_passed());
    }

    /// Both halves agree at every ordering, and the tie is load-bearing: a callee blames its own
    /// bound when both fall together, so a backend that hung for exactly its ceiling is never
    /// reported as a caller who ran out of time.
    #[test]
    fn resolve_answers_both_halves_from_one_comparison() {
        let now = std::time::Instant::now();
        let later = now + std::time::Duration::from_secs(60);
        let soon_at = now + std::time::Duration::from_secs(1);

        // The caller's bound falls first: it is both the instant and the blame.
        assert_eq!(
            Deadline::at(soon_at).resolve(later),
            (soon_at, Whose::Caller)
        );
        // The callee's own falls first, and keeps the blame.
        assert_eq!(Deadline::at(soon_at).resolve(now), (now, Whose::Callee));
        // A caller that named nothing leaves the callee its own, and the blame with it.
        assert_eq!(Deadline::UNBOUNDED.resolve(later), (later, Whose::Callee));
        // The tie, which only a shared instant can express: `<=` here would blame the caller for
        // a backend that hung for exactly its own ceiling.
        assert_eq!(Deadline::at(now).resolve(now), (now, Whose::Callee));
    }

    /// `within` is `resolve` in duration units, so a divergence between them is the split this
    /// type exists to prevent, coming back in a different shape (glass#432).
    #[test]
    fn every_unit_agrees_with_the_one_comparison() {
        let now = std::time::Instant::now();
        let second = std::time::Duration::from_secs(1);
        for deadline in [
            Deadline::at(now + second),
            Deadline::at(now),
            Deadline::at(now - second),
            // Past `proposed`, so the callee's own bound is the nearer one — without it every
            // bounded case here takes the caller arm.
            Deadline::at(now + std::time::Duration::from_secs(60)),
            Deadline::UNBOUNDED,
        ] {
            let proposed = now + std::time::Duration::from_secs(30);
            assert_eq!(
                deadline.within(std::time::Duration::from_secs(30), now),
                deadline.resolve(proposed).0.saturating_duration_since(now)
            );
            // A budget no `Instant` can hold: the deadline is the answer, and computing it must
            // not overflow on the way.
            assert_eq!(
                deadline.within(std::time::Duration::MAX, now),
                deadline
                    .remaining_at(now)
                    .unwrap_or(std::time::Duration::MAX)
            );
        }
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
