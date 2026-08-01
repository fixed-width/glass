use std::time::{Duration, Instant};

use crate::error::Result;

/// Outcome of a [`poll_until`] loop.
#[derive(Debug)]
pub struct PollOutcome<T> {
    /// `Some` if the predicate was satisfied, `None` on timeout.
    pub value: Option<T>,
    /// Wall-clock milliseconds elapsed when the loop returned.
    pub elapsed_ms: u64,
}

/// Poll `tick` until it reports satisfied (`Ok(Some(_))`) or `timeout_ms`
/// elapses. The first tick runs before any sleep; the timeout is checked after
/// each unsatisfied tick, so a `timeout_ms` of 0 yields exactly one tick. A tick
/// `Err` aborts immediately (no silent swallowing).
pub fn poll_until<T>(
    interval_ms: u64,
    timeout_ms: u64,
    tick: impl FnMut() -> Result<Option<T>>,
) -> Result<PollOutcome<T>> {
    poll_until_with_pause(
        interval_ms,
        timeout_ms,
        |d| {
            std::thread::sleep(d);
            true
        },
        tick,
    )
}

/// [`poll_until`] with the wait between ticks supplied by the caller, and the power to skip a tick.
///
/// `pause` is handed the interval and answers whether the next tick is worth running: `false` from
/// a caller that can be *told* nothing changed skips work whose answer cannot have changed, and
/// `true` reproduces a plain sleep-and-retry.
///
/// The deadline is checked every iteration, tick or no tick, so a `pause` that always says "skip"
/// still ends the loop on time; and a `pause` that never blocks spins only until the deadline.
///
/// `pause` is called even when `interval_ms` is 0 — sleeping for zero is what the default does, and
/// a caller whose pause can *skip* must decide for itself whether a zero interval leaves it
/// anything to wait for.
pub fn poll_until_with_pause<T>(
    interval_ms: u64,
    timeout_ms: u64,
    mut pause: impl FnMut(Duration) -> bool,
    mut tick: impl FnMut() -> Result<Option<T>>,
) -> Result<PollOutcome<T>> {
    let start = Instant::now();
    let mut run_tick = true;
    loop {
        if run_tick && let Some(v) = tick()? {
            return Ok(PollOutcome {
                value: Some(v),
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            // Never answer "not found" on information older than the last pause: a pause told
            // "nothing changed" by a platform that declines to announce it would otherwise report
            // an element absent that is on screen. One tick at an already-spent deadline keeps a
            // skip a cost saving rather than a wrong answer.
            if !run_tick && let Some(v) = tick()? {
                return Ok(PollOutcome {
                    value: Some(v),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
            return Ok(PollOutcome {
                value: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
        run_tick = pause(Duration::from_millis(interval_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GlassError;

    #[test]
    fn the_pause_hook_replaces_the_sleep() {
        let paused = std::cell::Cell::new(0);
        let mut ticks = 0;
        poll_until_with_pause(
            5,
            5_000,
            |_| {
                paused.set(paused.get() + 1);
                true
            },
            || {
                ticks += 1;
                Ok(if ticks == 3 { Some(()) } else { None })
            },
        )
        .unwrap();
        // Two unsatisfied ticks, so two pauses — and no sleep of the loop's own, which is what
        // lets a caller wake on an event instead of waiting out an interval.
        assert_eq!(paused.get(), 2);
    }

    #[test]
    fn a_loop_that_skipped_its_ticks_looks_once_more_before_answering_no() {
        // A pause told "nothing changed" by a platform that declines to announce it must not
        // leave the loop answering from information it has not refreshed since.
        let mut ticks = 0;
        let out = poll_until_with_pause(
            1,
            30,
            |_| false, // "nothing changed" — every time, and wrongly
            || {
                ticks += 1;
                // Absent on the first look, present from then on: the change the pause hid.
                Ok(if ticks > 1 { Some(7) } else { None })
            },
        )
        .unwrap();
        assert_eq!(
            out.value,
            Some(7),
            "answered from information older than the last pause"
        );
    }

    #[test]
    fn the_deadline_look_is_skipped_when_this_iteration_already_ticked() {
        // The deadline read exists for information the loop has not refreshed. A loop that just
        // ticked has, so reading again would bill every ordinary polling caller an extra tick for
        // nothing.
        let mut ticks = 0;
        poll_until_with_pause(
            0,
            0, // one tick, then straight to the deadline
            |_| true,
            || {
                ticks += 1;
                Ok(None::<()>)
            },
        )
        .unwrap();
        assert_eq!(ticks, 1, "ticked again at a deadline it had just looked at");
    }

    #[test]
    fn a_pause_that_returns_early_still_honours_the_deadline() {
        // A signal that fires constantly must bound the loop, not spin it forever.
        let started = Instant::now();
        let out = poll_until_with_pause(10, 40, |_| true, || Ok(None::<()>)).unwrap();
        assert!(out.value.is_none());
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn returns_value_when_satisfied_immediately() {
        let out = poll_until(0, 1000, || Ok(Some(42))).unwrap();
        assert_eq!(out.value, Some(42));
    }

    #[test]
    fn polls_until_satisfied_then_stops() {
        let mut n = 0;
        let out = poll_until(0, 1000, || {
            n += 1;
            Ok(if n >= 3 { Some(n) } else { None })
        })
        .unwrap();
        assert_eq!(out.value, Some(3));
        assert_eq!(n, 3, "stops calling tick once satisfied");
    }

    #[test]
    fn times_out_with_none() {
        let out = poll_until(0, 0, || Ok::<Option<()>, GlassError>(None)).unwrap();
        assert!(out.value.is_none());
    }

    #[test]
    fn tick_error_propagates() {
        let err = poll_until(0, 1000, || -> Result<Option<()>> {
            Err(GlassError::Backend("boom".into()))
        })
        .unwrap_err();
        assert!(matches!(err, GlassError::Backend(_)));
    }
}
