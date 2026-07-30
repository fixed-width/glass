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
    poll_until_with_pause(interval_ms, timeout_ms, std::thread::sleep, tick)
}

/// [`poll_until`] with the wait between ticks supplied by the caller.
///
/// `pause` is handed the interval and may return early — a caller that can be *told* the state
/// changed (an accessibility event, say) uses it to wake immediately instead of sleeping out an
/// interval that no longer means anything. Returning early only skips the wait: the deadline still
/// governs, so a `pause` that never blocks turns the loop into a spin bounded by `timeout_ms`
/// rather than an unbounded one.
pub fn poll_until_with_pause<T>(
    interval_ms: u64,
    timeout_ms: u64,
    mut pause: impl FnMut(Duration),
    mut tick: impl FnMut() -> Result<Option<T>>,
) -> Result<PollOutcome<T>> {
    let start = Instant::now();
    loop {
        if let Some(v) = tick()? {
            return Ok(PollOutcome {
                value: Some(v),
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Ok(PollOutcome {
                value: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
        if interval_ms > 0 {
            pause(Duration::from_millis(interval_ms));
        }
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
            |_| paused.set(paused.get() + 1),
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
    fn a_pause_that_returns_early_still_honours_the_deadline() {
        // A signal that fires constantly must bound the loop, not spin it forever.
        let started = Instant::now();
        let out = poll_until_with_pause(10, 40, |_| {}, || Ok(None::<()>)).unwrap();
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
