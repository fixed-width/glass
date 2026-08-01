#![forbid(unsafe_code)]
//! When a value read repeatedly has stopped changing.
//!
//! macOS reports a window's geometry while the window is still opening, so the reading taken at
//! adoption is routinely a frame of the open animation rather than the window's real size (#263).
//! [`SettleTracker`] decides when a run of readings has stopped changing; [`settle_by_polling`]
//! owns the poll loop itself (sleeping between reads) so a caller only has to supply how to take
//! one reading. Kept generic and out of the `#[cfg(target_os = "macos")]` modules so the rule is
//! unit-tested on any host, and so `glass-core` could adopt it if the other backends turn out to
//! race the same way.

use std::time::{Duration, Instant};

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
/// Two samples rather than three or a fixed duration: a value that was already stable when first
/// read needs only one more reading to confirm it — the cheapest path through this loop, though
/// not the likely one. On the macOS backend that motivated this (#263), it was the minority
/// outcome: 1 of 12 measured cold launches were already stable at adoption; the other 11 needed
/// further polling before settling.
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

/// How a [`settle_by_polling`] call ended.
#[derive(Debug, PartialEq)]
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
/// `interval` throttles `read`: a value that never settles is called roughly `budget / interval`
/// times, so `Duration::ZERO` (or another interval far below `read`'s own cost) busy-spins for
/// the whole budget rather than pacing the polls — fine for a test with a cheap, instant `read`,
/// but not the shape a real caller wants. The one production caller today (macOS's
/// `settle_window`, #263) uses 25ms.
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

    /// Pins #263's "the predicate silently widened" review finding, at the one seam that is
    /// actually testable from Linux: `settle_by_polling` compares the *whole* value `read`
    /// returns, via `T`'s ordinary `PartialEq` — nothing partial, nothing clever. This is exactly
    /// why `settle_window` (in `backend.rs`, cfg-gated to macOS and unreachable from here) passes
    /// `WindowGeometry` rather than the wider `WindowMatch`: `WindowMatch::origin_pt` is the raw
    /// unrounded point origin `geometry` is rounded from, so it can drift forever inside an
    /// already-settled pixel, and comparing the whole match — as an earlier revision of this
    /// branch did — would make that drift block settling forever even though the geometry that
    /// actually matters had already stopped changing. If `settle_by_polling`'s comparison were
    /// ever narrowed to ignore part of `T` (a plausible "optimization"), a wide `T` like this one
    /// would start settling despite an unmatched field still drifting, and this test would catch
    /// it.
    ///
    /// `Source` stands in for `WindowMatch`: `pixel` is the field that should decide settlement
    /// (like `geometry`), `raw` is a volatile field of the wider value that never repeats (like
    /// `origin_pt`).
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
