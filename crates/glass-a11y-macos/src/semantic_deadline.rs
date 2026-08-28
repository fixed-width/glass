use std::time::{Duration, Instant};

use glass_core::{Deadline, GlassError, Result, Whose};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SemanticOperation {
    Snapshot,
    SetValue(u32),
    Invoke,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SemanticDeadline {
    deadline: Deadline,
    operation: SemanticOperation,
    dispatched: bool,
}

impl SemanticDeadline {
    pub(crate) fn snapshot(deadline: Deadline) -> Self {
        Self {
            deadline,
            operation: SemanticOperation::Snapshot,
            dispatched: false,
        }
    }

    pub(crate) fn set_value(deadline: Deadline, target: u32) -> Self {
        Self {
            deadline,
            operation: SemanticOperation::SetValue(target),
            dispatched: false,
        }
    }

    pub(crate) fn invoke(deadline: Deadline) -> Self {
        Self {
            deadline,
            operation: SemanticOperation::Invoke,
            dispatched: false,
        }
    }

    pub(crate) fn after_dispatch(self) -> Self {
        Self {
            dispatched: true,
            ..self
        }
    }

    pub(crate) fn expired(self) -> GlassError {
        match (self.operation, self.dispatched) {
            (SemanticOperation::Snapshot, false) => {
                GlassError::deadline_not_started("native accessibility snapshot")
            }
            (SemanticOperation::Snapshot, true) => {
                GlassError::caller_deadline_elapsed_with_guidance(
                    "native accessibility snapshot",
                    "no accessibility tree became available within the time this call allowed",
                )
            }
            (SemanticOperation::SetValue(_), false) => {
                GlassError::deadline_not_started("native accessibility set_value")
            }
            (SemanticOperation::SetValue(target), true) => GlassError::write_unconfirmed_because(
                target,
                "the caller deadline elapsed after the native value mutation was dispatched",
                GlassError::caller_deadline_elapsed("native accessibility set_value"),
            ),
            (SemanticOperation::Invoke, false) => {
                GlassError::deadline_not_started("native accessibility invoke")
            }
            (SemanticOperation::Invoke, true) => {
                GlassError::caller_deadline_elapsed("native accessibility invoke")
            }
        }
    }

    pub(crate) fn require(self) -> Result<()> {
        self.require_at(Instant::now())
    }

    pub(crate) fn require_at(self, now: Instant) -> Result<()> {
        if self
            .deadline
            .remaining_at(now)
            .is_some_and(|remaining| remaining.is_zero())
        {
            Err(self.expired())
        } else {
            Ok(())
        }
    }

    pub(crate) fn finish<T>(self, result: Result<T>) -> Result<T> {
        self.require()?;
        match result {
            Err(error) if error.bound_owner() == Some(Whose::Caller) => Err(self.expired()),
            Err(error)
                if matches!(self.operation, SemanticOperation::SetValue(_))
                    && self.dispatched
                    && !error.set_value_failed_after_writing() =>
            {
                let SemanticOperation::SetValue(target) = self.operation else {
                    unreachable!("guarded by the set_value operation match")
                };
                Err(GlassError::write_unconfirmed_because(
                    target,
                    "the native value mutation was dispatched but failed before it could be confirmed",
                    error,
                ))
            }
            result => result,
        }
    }

    pub(crate) fn run<T>(self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.require()?;
        self.finish(work())
    }

    pub(crate) fn observe<T>(self, work: impl FnOnce() -> T) -> Result<T> {
        self.require()?;
        let value = work();
        self.require()?;
        Ok(value)
    }

    pub(crate) fn dispatch<T>(self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.require()?;
        self.after_dispatch().finish(work())
    }

    pub(crate) fn phase(self, own: Instant) -> EffectiveDeadline {
        let (ends, owner) = self.deadline.resolve(own);
        EffectiveDeadline { ends, owner }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveDeadline {
    ends: Instant,
    owner: Whose,
}

impl EffectiveDeadline {
    pub(crate) fn expired_owner_at(self, now: Instant) -> Option<Whose> {
        (now >= self.ends).then_some(self.owner)
    }

    pub(crate) fn callee_expired_at(self, guard: SemanticDeadline, now: Instant) -> Result<bool> {
        match self.expired_owner_at(now) {
            Some(Whose::Caller) => Err(guard.expired()),
            Some(Whose::Callee) => Ok(true),
            None => Ok(false),
        }
    }

    pub(crate) fn callee_expired(self, guard: SemanticDeadline) -> Result<bool> {
        self.callee_expired_at(guard, Instant::now())
    }

    pub(crate) fn observe<T>(
        self,
        guard: SemanticDeadline,
        work: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        self.observe_with_clock(guard, Instant::now, work)
    }

    fn observe_with_clock<T>(
        self,
        guard: SemanticDeadline,
        mut now: impl FnMut() -> Instant,
        work: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        if self.callee_expired_at(guard, now())? {
            return Ok(None);
        }
        let value = work();
        if self.callee_expired_at(guard, now())? {
            return Ok(None);
        }
        Ok(Some(value))
    }

    pub(crate) fn sleep(self, guard: SemanticDeadline, requested: Duration) -> Result<bool> {
        if self.callee_expired(guard)? {
            return Ok(false);
        }
        std::thread::sleep(self.cap_at(requested, Instant::now()));
        Ok(!self.callee_expired(guard)?)
    }

    pub(crate) fn remaining_at(self, now: Instant) -> Duration {
        self.ends.saturating_duration_since(now)
    }

    pub(crate) fn cap_at(self, requested: Duration, now: Instant) -> Duration {
        requested.min(self.remaining_at(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{BoundDispatch, Whose};

    #[test]
    fn a_spent_snapshot_deadline_is_not_started() {
        let now = Instant::now();
        let error = SemanticDeadline::snapshot(Deadline::at(now))
            .require_at(now)
            .unwrap_err();

        assert_eq!(error.bound(), Some(glass_core::BoundKind::NotStarted));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn a_snapshot_deadline_expiring_after_dispatch_is_a_caller_timeout() {
        let now = Instant::now();
        let error = SemanticDeadline::snapshot(Deadline::at(now))
            .after_dispatch()
            .require_at(now)
            .unwrap_err();

        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn spent_mutations_have_not_dispatched() {
        let now = Instant::now();
        for guard in [
            SemanticDeadline::set_value(Deadline::at(now), 7),
            SemanticDeadline::invoke(Deadline::at(now)),
        ] {
            let error = guard.require_at(now).unwrap_err();
            assert_eq!(error.bound_owner(), Some(Whose::Caller));
            assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        }
    }

    #[test]
    fn a_deadline_expiring_after_dispatch_is_an_unconfirmed_value_write() {
        let now = Instant::now();
        let error = SemanticDeadline::set_value(Deadline::at(now), 7)
            .after_dispatch()
            .require_at(now)
            .unwrap_err();

        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
        assert_eq!(
            error.bound(),
            Some(glass_core::BoundKind::TimedOut),
            "{error}"
        );
        assert!(
            matches!(error.cause(), GlassError::Bounded { .. }),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_dispatched_set_value_failure_preserves_its_tool_source() {
        let error = SemanticDeadline::set_value(Deadline::UNBOUNDED, 7)
            .after_dispatch()
            .finish::<()>(Err(GlassError::ToolFailed {
                call: "AXUIElementSetAttributeValue".into(),
                said: " transport unavailable \n".into(),
            }))
            .expect_err("the native setter failed after dispatch");

        assert!(
            matches!(error.cause(), GlassError::ToolFailed { .. }),
            "{error}"
        );
        assert_eq!(error.tool_said(), Some("transport unavailable"), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched),
            "{error}"
        );
    }

    #[test]
    fn every_sleep_is_capped_by_the_absolute_caller_deadline() {
        let now = Instant::now();
        let duration = SemanticDeadline::snapshot(Deadline::at(now + Duration::from_millis(5)))
            .phase(now + Duration::from_secs(1))
            .cap_at(Duration::from_millis(40), now);

        assert_eq!(duration, Duration::from_millis(5));
    }

    #[test]
    fn confirmation_finishing_at_the_caller_deadline_cannot_return_true() {
        let now = Instant::now();
        let ends = now + Duration::from_millis(5);
        let guard = SemanticDeadline::set_value(Deadline::at(ends), 7).after_dispatch();
        let verification = guard.phase(now + Duration::from_secs(1));
        let mut observations = [now, ends].into_iter();

        let error = verification
            .observe_with_clock(
                guard,
                || observations.next().expect("two clock observations"),
                || true,
            )
            .expect_err("confirmation observed at expiry must not report success");

        assert!(error.set_value_failed_after_writing(), "{error}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
    }

    #[test]
    fn an_exact_caller_backend_tie_keeps_the_backend_ceiling_in_charge() {
        let now = Instant::now();
        let phase = SemanticDeadline::snapshot(Deadline::at(now)).phase(now);

        assert_eq!(phase.owner, Whose::Callee);
        assert_eq!(phase.expired_owner_at(now), Some(Whose::Callee));
    }
}
