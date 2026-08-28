//! Pure active-window resolver deadline policy, shared by macOS and host tests.

use std::time::{Duration, Instant};

use glass_core::{Deadline, GlassError, Result, Whose};

pub(crate) fn resolve_by<T>(
    deadline: Deadline,
    own_timeout: Duration,
    poll_interval: Duration,
    mut now: impl FnMut() -> Instant,
    mut pause: impl FnMut(Duration),
    mut query: impl FnMut(Duration) -> Result<Option<T>>,
    missing: impl Fn() -> GlassError,
) -> Result<T> {
    let started = now();
    let (ends, owner) = deadline.resolve(started + own_timeout);
    let expired = || match owner {
        Whose::Caller => GlassError::deadline_not_started("macOS input window resolution"),
        Whose::Callee => missing(),
    };

    loop {
        let before_query = now();
        if before_query >= ends {
            return Err(expired());
        }
        let query_result = query(ends.saturating_duration_since(before_query));
        let observed_at = now();
        if observed_at >= ends {
            return Err(expired());
        }
        if let Some(value) = query_result? {
            return Ok(value);
        }
        pause(poll_interval.min(ends.saturating_duration_since(observed_at)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{BoundDispatch, BoundKind};
    use std::cell::Cell;

    #[test]
    fn caller_deadline_caps_the_resolver_and_rejects_a_late_match() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let observed_cap = Cell::new(Duration::ZERO);
        let caller_budget = Duration::from_millis(25);

        let error = resolve_by(
            Deadline::at(started + caller_budget),
            Duration::from_secs(2),
            Duration::from_millis(100),
            || clock.get(),
            |sleep| clock.set(clock.get() + sleep),
            |cap| {
                observed_cap.set(cap);
                clock.set(started + caller_budget);
                Ok(Some(()))
            },
            || GlassError::WindowNotFound,
        )
        .expect_err("a match observed at the caller deadline must not reach input dispatch");

        assert_eq!(observed_cap.get(), caller_budget);
        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn backend_ceiling_stays_backend_owned_when_it_falls_first() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let own_timeout = Duration::from_millis(25);

        let error = resolve_by(
            Deadline::at(started + Duration::from_secs(1)),
            own_timeout,
            Duration::from_millis(100),
            || clock.get(),
            |sleep| clock.set(clock.get() + sleep),
            |cap| {
                assert_eq!(cap, own_timeout);
                clock.set(started + own_timeout);
                Ok(Some(()))
            },
            || GlassError::WindowNotFound,
        )
        .expect_err("a match observed at the backend ceiling is not success");

        assert!(matches!(error, GlassError::WindowNotFound));
        assert_eq!(error.bound_owner(), None);
    }
}
