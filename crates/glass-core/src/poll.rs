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
    mut tick: impl FnMut() -> Result<Option<T>>,
) -> Result<PollOutcome<T>> {
    let start = Instant::now();
    loop {
        if let Some(v) = tick()? {
            return Ok(PollOutcome { value: Some(v), elapsed_ms: start.elapsed().as_millis() as u64 });
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Ok(PollOutcome { value: None, elapsed_ms: start.elapsed().as_millis() as u64 });
        }
        if interval_ms > 0 {
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GlassError;

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
