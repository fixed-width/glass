//! When a value read repeatedly has stopped changing.
//!
//! macOS reports a window's geometry while the window is still opening, so the reading taken at
//! adoption is routinely a frame of the open animation rather than the window's real size (#263).
//! [`SettleTracker`] decides when a run of readings has stopped changing; [`settle_by_polling`]
//! owns the poll loop itself (sleeping between reads) so a caller only has to supply how to take
//! one reading. Kept generic and out of the `#[cfg(target_os = "macos")]` modules so the rule is
//! unit-tested on any host, and so `glass-core` could adopt it if the other backends turn out to
//! race the same way.
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

/// What one more reading tells the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum SettleStep<T> {
    /// This reading matched the one before it. Stop polling; this is the value.
    Settled(T),
    /// No agreement yet. Poll again, until the caller's own budget runs out.
    Continue,
}

/// Feeds readings in, one at a time, and reports the first time two consecutive readings agree.
///
/// Two samples rather than three or a fixed duration: a value that was already stable when first
/// read needs only one more reading to confirm it — the cheapest path through this loop, though
/// not the likely one. On the macOS backend that motivated this (#263), it was the minority
/// outcome: 1 of 12 measured cold launches was already stable at adoption; the other 11 needed
/// further polling before settling.
#[derive(Clone, Debug)]
pub(crate) struct SettleTracker<T> {
    previous: Option<T>,
}

// Hand-written rather than `#[derive(Default)]`: the derive adds a `T: Default` bound to the
// impl even though `Option<T>` needs none — `previous` defaults to `None` for any `T`.
impl<T> Default for SettleTracker<T> {
    fn default() -> Self {
        Self { previous: None }
    }
}

impl<T: Clone + PartialEq> SettleTracker<T> {
    pub(crate) fn new() -> Self {
        Self { previous: None }
    }

    /// Record `reading` and report whether it settles the sequence.
    pub(crate) fn observe(&mut self, reading: T) -> SettleStep<T> {
        // The clone only happens on the `Settled` path, where the caller needs `reading` back
        // by value *and* `self.previous` needs to keep its own copy; `Continue` carries no
        // payload, so that path moves `reading` straight into `self.previous` instead.
        if self.previous.as_ref() == Some(&reading) {
            let settled = reading.clone();
            self.previous = Some(reading);
            SettleStep::Settled(settled)
        } else {
            self.previous = Some(reading);
            SettleStep::Continue
        }
    }
}

/// How a [`settle_by_polling`] call ended.
#[derive(Debug, PartialEq)]
#[must_use]
pub enum SettleOutcome<E> {
    /// Two consecutive readings agreed before `budget` elapsed.
    Settled,
    /// `budget` elapsed with no two consecutive readings agreeing.
    BudgetExpired,
    /// `read` returned an error; polling stopped rather than retrying.
    ReadFailed(E),
}

/// Poll `read` every `interval`, seeded with `seed` as the first reading, until two consecutive
/// readings agree or `budget` elapses. Returns the freshest reading obtained — `seed` itself if
/// `read` never succeeded — paired with how the poll ended.
///
/// `interval` throttles `read`: a value that never settles is called roughly
/// `budget / (interval + read's own cost)` times — `interval` alone only bounds the count when
/// `read` is cheap relative to it, which is true of the tests below (an instant closure) but not
/// of a real `read` that does I/O. `Duration::ZERO` busy-spins between calls rather than pacing
/// them — fine for a test with a cheap, instant `read`, but not the shape a real caller wants.
/// The one production caller today (macOS's `settle_window`, #263) uses 25ms, well under its
/// own `read`'s cost (a live query), so pacing there comes mostly from the query itself.
///
/// `T`'s `PartialEq` decides settlement on the *whole* value — pick a `T` that carries only the
/// fields that should hold the loop open while they change. A `T` with an extra field that never
/// repeats (an unrounded coordinate a rounded one was derived from, say) never settles, even once
/// the fields that actually matter have stopped moving; see the caller-side regression this was
/// built to close, `settle_window` (`backend.rs`, #263), and this module's own
/// `a_wide_t_never_settles_while_any_of_its_fields_keeps_drifting` test.
pub fn settle_by_polling<T, E>(
    seed: T,
    budget: Duration,
    interval: Duration,
    mut read: impl FnMut() -> Result<T, E>,
) -> (T, SettleOutcome<E>)
where
    T: Clone + PartialEq,
{
    let mut tracker = SettleTracker::new();
    // The seed is the sequence's first element: it is what a value that was already settled has
    // to agree with for the loop to exit after a single poll.
    let _ = tracker.observe(seed.clone());
    let mut freshest = seed;
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        std::thread::sleep(interval);
        let next = match read() {
            Ok(v) => v,
            Err(e) => return (freshest, SettleOutcome::ReadFailed(e)),
        };
        freshest = next.clone();
        if let SettleStep::Settled(_) = tracker.observe(next) {
            return (freshest, SettleOutcome::Settled);
        }
    }
    (freshest, SettleOutcome::BudgetExpired)
}

#[cfg(test)]
mod tests {
    use super::{SettleOutcome, SettleStep, SettleTracker, settle_by_polling};
    use std::cell::Cell;
    use std::time::Duration;

    /// A single reading can never settle: "settled" means two readings agreed, and there is
    /// nothing yet to agree with.
    #[test]
    fn the_first_reading_never_settles() {
        let mut t = SettleTracker::new();
        assert_eq!(t.observe(10), SettleStep::Continue);
    }

    /// The cheap case (not the typical one — see [`SettleTracker`]'s doc): a window that was
    /// already stable settles on its second reading.
    #[test]
    fn two_equal_readings_settle() {
        let mut t = SettleTracker::new();
        let _ = t.observe(10);
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
        let _ = t.observe(10);
        let _ = t.observe(20);
        assert_eq!(t.observe(10), SettleStep::Continue);
    }

    /// Guards against the seed silently stopping being the sequence's first element: if it did,
    /// a reader that only ever confirms the seed would need a second call to settle, and every
    /// real caller would pay one extra round trip on every launch for nothing.
    #[test]
    fn a_reader_confirming_the_seed_settles_after_exactly_one_call() {
        let calls = Cell::new(0);
        let (value, outcome) =
            settle_by_polling(10, Duration::from_millis(50), Duration::ZERO, || {
                calls.set(calls.get() + 1);
                Ok::<i32, &str>(10)
            });
        assert_eq!(calls.get(), 1);
        assert_eq!(value, 10);
        assert_eq!(outcome, SettleOutcome::Settled);
    }

    /// A changing-then-stable reader must settle on the value the readings actually converged
    /// to, not the seed and not an intermediate value passed through on the way there.
    #[test]
    fn a_changing_then_stable_reader_settles_on_the_later_value() {
        let mut readings = [10, 20, 20].into_iter();
        let (value, outcome) =
            settle_by_polling(1, Duration::from_millis(50), Duration::ZERO, || {
                Ok::<i32, &str>(readings.next().expect("test provides exactly 3 readings"))
            });
        assert_eq!(value, 20);
        assert_eq!(outcome, SettleOutcome::Settled);
    }

    /// A reader that never repeats two consecutive values never settles: the budget must expire,
    /// and what comes back must be the last thing actually read, not the seed it started from —
    /// a caller with a real budget still wants the freshest information it managed to gather.
    #[test]
    fn a_never_repeating_reader_reports_budget_expiry_with_the_freshest_value() {
        let next_value = Cell::new(0);
        let (value, outcome) =
            settle_by_polling(-1, Duration::from_millis(50), Duration::ZERO, || {
                next_value.set(next_value.get() + 1);
                Ok::<i32, &str>(next_value.get())
            });
        assert_eq!(outcome, SettleOutcome::BudgetExpired);
        assert_eq!(
            value,
            next_value.get(),
            "must report the last value actually read"
        );
        assert_ne!(value, -1, "must not report the seed it started from");
    }

    /// A `read` failure must end the poll immediately (no retry) and hand back the last good
    /// reading plus the error — not the seed, and not a silently-swallowed failure that just
    /// tries again.
    #[test]
    fn a_read_failure_ends_the_poll_and_reports_the_last_good_value() {
        let calls = Cell::new(0);
        let (value, outcome) =
            settle_by_polling(0, Duration::from_millis(50), Duration::ZERO, || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Ok(5)
                } else {
                    Err("window vanished")
                }
            });
        assert_eq!(
            calls.get(),
            2,
            "must stop polling at the first failure, not retry"
        );
        assert_eq!(value, 5);
        assert_eq!(outcome, SettleOutcome::ReadFailed("window vanished"));
    }

    /// A `read` that fails on its very first call — before any reading past the seed ever
    /// succeeded — must report the seed itself, per `settle_by_polling`'s own doc ("seed itself
    /// if `read` never succeeded"). The previous failure test only proves a *later* failure keeps
    /// the last good reading; this covers the zero-successes case that doc separately promises.
    #[test]
    fn a_read_failure_on_the_first_poll_reports_the_seed() {
        let (value, outcome) =
            settle_by_polling(42, Duration::from_millis(50), Duration::ZERO, || {
                Err::<i32, _>("window vanished")
            });
        assert_eq!(
            value, 42,
            "must report the seed when no read ever succeeded"
        );
        assert_eq!(outcome, SettleOutcome::ReadFailed("window vanished"));
    }

    /// Pins #263's "the predicate silently widened" finding: `settle_by_polling` must compare
    /// the *whole* `T` via ordinary `PartialEq`, not just a chosen field. `Source` stands in for
    /// `WindowMatch`: `pixel` is the field that should decide settlement (like `geometry`), `raw`
    /// is a volatile field that never repeats (like the raw `origin_pt` a rounded `geometry` is
    /// derived from) — comparing only `pixel` would let this settle despite `raw` still drifting.
    #[test]
    fn a_wide_t_never_settles_while_any_of_its_fields_keeps_drifting() {
        #[derive(Clone, PartialEq)]
        struct Source {
            pixel: i32,
            raw: i64,
        }

        let mut raw = 0;
        let (_, outcome) = settle_by_polling(
            Source { pixel: 510, raw },
            Duration::from_millis(50),
            Duration::ZERO,
            || {
                raw += 1;
                Ok::<Source, &str>(Source { pixel: 510, raw })
            },
        );
        assert_eq!(
            outcome,
            SettleOutcome::BudgetExpired,
            "pixel never changed, but raw never repeated either — comparing the whole Source \
             must never settle"
        );
    }
}
