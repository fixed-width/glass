use super::*;

/// Reader headroom reserved before a quiet wait's deadline, capped at one quarter of the
/// remaining budget.
const FINAL_READ_HEADROOM: std::time::Duration = std::time::Duration::from_millis(20);

pub(super) struct A11yPollOutcome<O> {
    pub observation: O,
    pub satisfied: bool,
    pub elapsed_ms: u64,
    pub timed_out_by: Option<crate::Whose>,
}

fn final_read_pause(left: std::time::Duration) -> std::time::Duration {
    left.saturating_sub(FINAL_READ_HEADROOM.min(left / 4))
}

fn quiet_wait_needs_read(
    final_read: bool,
    since_last: std::time::Duration,
    reread_after: std::time::Duration,
) -> bool {
    final_read || since_last >= reread_after
}

fn should_schedule_final_read(
    already_scheduled: bool,
    left: Option<std::time::Duration>,
    interval: std::time::Duration,
) -> bool {
    !already_scheduled && left.is_some_and(|left| left <= interval)
}

fn reader_relative_caller_bound(error: &GlassError) -> bool {
    error.bound_owner() == Some(crate::Whose::Caller)
}

fn outer_sequence_expired(owner: crate::Whose, expired: bool) -> bool {
    owner == crate::Whose::Caller && expired
}

fn should_reclassify_nested_bound(effective_owner: crate::Whose, sequence_expired: bool) -> bool {
    effective_owner == crate::Whose::Callee && !sequence_expired
}

/// Reclassify a reader-relative caller bound without overriding an expired outer sequence.
fn resolve_nested_accessibility_bound(
    error: GlassError,
    effective_owner: crate::Whose,
    sequence_deadline: Deadline,
) -> GlassError {
    if should_reclassify_nested_bound(effective_owner, sequence_deadline.has_passed()) {
        match error {
            GlassError::Bounded {
                kind,
                whose: crate::Whose::Caller,
                dispatch,
                message,
            } => GlassError::Bounded {
                kind,
                whose: crate::Whose::Callee,
                dispatch,
                message,
            },
            error => error,
        }
    } else {
        error
    }
}

impl Glass {
    pub(super) fn poll_accessibility_until<O>(
        &mut self,
        interval_ms: u64,
        timeout_ms: u64,
        sequence_deadline: Deadline,
        operation: &'static str,
        observe: impl FnMut(&AxTree) -> O,
        satisfied: impl FnMut(&O) -> bool,
    ) -> Result<A11yPollOutcome<O>> {
        self.poll_accessibility_until_with_reread(
            interval_ms,
            std::time::Duration::from_secs(1),
            timeout_ms,
            sequence_deadline,
            operation,
            observe,
            satisfied,
        )
    }

    pub(super) fn poll_accessibility_until_with_reread<O>(
        &mut self,
        interval_ms: u64,
        reread_after: std::time::Duration,
        timeout_ms: u64,
        sequence_deadline: Deadline,
        operation: &'static str,
        observe: impl FnMut(&AxTree) -> O,
        satisfied: impl FnMut(&O) -> bool,
    ) -> Result<A11yPollOutcome<O>> {
        if sequence_deadline.has_passed() {
            return Err(GlassError::deadline_not_started(operation));
        }
        self.require_active()?;
        let started = std::time::Instant::now();
        let (effective_duration, whose) =
            sequence_deadline.budget(std::time::Duration::from_millis(timeout_ms), started);
        let action_deadline = Deadline::at(started + effective_duration);
        self.poll_accessibility_until_with_deadline(
            interval_ms,
            reread_after,
            action_deadline,
            whose,
            timeout_ms > 0,
            sequence_deadline,
            operation,
            started,
            observe,
            satisfied,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn poll_accessibility_until_by_deadline<O>(
        &mut self,
        interval_ms: u64,
        reread_after: std::time::Duration,
        action_deadline: Deadline,
        whose: crate::Whose,
        allow_wait: bool,
        sequence_deadline: Deadline,
        operation: &'static str,
        observe: impl FnMut(&AxTree) -> O,
        satisfied: impl FnMut(&O) -> bool,
    ) -> Result<A11yPollOutcome<O>> {
        if sequence_deadline.has_passed() {
            return Err(GlassError::deadline_not_started(operation));
        }
        self.require_active()?;
        if allow_wait && action_deadline.has_passed() {
            return Err(GlassError::Bounded {
                kind: crate::BoundKind::NotStarted,
                whose,
                dispatch: crate::BoundDispatch::NotDispatched,
                message: format!(
                    "{operation}: its effective deadline was already spent, so it was not started"
                ),
            });
        }
        self.poll_accessibility_until_with_deadline(
            interval_ms,
            reread_after,
            action_deadline,
            whose,
            allow_wait,
            sequence_deadline,
            operation,
            std::time::Instant::now(),
            observe,
            satisfied,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn poll_accessibility_until_with_deadline<O>(
        &mut self,
        interval_ms: u64,
        reread_after: std::time::Duration,
        action_deadline: Deadline,
        whose: crate::Whose,
        allow_wait: bool,
        sequence_deadline: Deadline,
        operation: &'static str,
        started: std::time::Instant,
        mut observe: impl FnMut(&AxTree) -> O,
        mut satisfied: impl FnMut(&O) -> bool,
    ) -> Result<A11yPollOutcome<O>> {
        let mut signal = (interval_ms > 0)
            .then(|| self.subscribe_a11y_changes(action_deadline))
            .flatten();
        let remaining = if allow_wait {
            let deadline = action_deadline
                .instant()
                .expect("a waiting accessibility poll has a finite deadline");
            let effective_duration = deadline.saturating_duration_since(started);
            (effective_duration.as_millis() as u64)
                .saturating_sub(started.elapsed().as_millis() as u64)
        } else {
            0
        };
        let mut last_read = std::time::Instant::now();
        let mut unread: Option<GlassError> = None;
        let mut unread_owner = whose;
        let mut saw_a_tree = false;
        let first_read_deadline = if !allow_wait {
            sequence_deadline
        } else {
            action_deadline
        };
        let mut looked = false;
        let mut last_observation = None;
        let final_read_scheduled = std::cell::Cell::new(false);
        let outcome = crate::poll::poll_until_with_pause(
            interval_ms,
            remaining,
            |d| {
                let left = action_deadline.remaining();
                let final_read = should_schedule_final_read(final_read_scheduled.get(), left, d);
                if final_read {
                    final_read_scheduled.set(true);
                }
                let paused_at = std::time::Instant::now();
                let pause_budget = if final_read {
                    let left = left.expect("a final read is scheduled only for a bounded wait");
                    final_read_pause(left)
                } else {
                    left.unwrap_or(d).min(d)
                };
                let read_now = match signal.as_mut() {
                    Some(s) => match s.wait(pause_budget) {
                        ChangeWait::Changed => true,
                        ChangeWait::Quiet => {
                            quiet_wait_needs_read(final_read, last_read.elapsed(), reread_after)
                        }
                        ChangeWait::Unusable => {
                            signal = None;
                            true
                        }
                    },
                    None => true,
                };
                if read_now {
                    last_read = std::time::Instant::now();
                }
                std::thread::sleep(pause_budget.saturating_sub(paused_at.elapsed()));
                read_now
            },
            || {
                let first_read = !looked;
                let bound = if first_read {
                    first_read_deadline
                } else {
                    action_deadline
                };
                let read_owner =
                    if first_read && !allow_wait && sequence_deadline.instant().is_some() {
                        crate::Whose::Caller
                    } else {
                        whose
                    };
                if !first_read && bound.has_passed() {
                    return Ok(None);
                }
                looked = true;
                let tree = match self.a11y_resnapshot_for_wait(bound) {
                    Ok(t) => {
                        saw_a_tree = true;
                        t
                    }
                    Err(e @ GlassError::AccessibilityNotReady(_)) => {
                        unread_owner = read_owner;
                        unread = Some(e);
                        return Ok(None);
                    }
                    Err(e) if reader_relative_caller_bound(&e) => {
                        unread_owner = read_owner;
                        unread = Some(e);
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                };
                let observation = observe(&tree);
                let is_satisfied = satisfied(&observation);
                last_observation = Some(observation);
                Ok(is_satisfied.then_some(()))
            },
        )?;
        if outcome.value.is_none()
            && !saw_a_tree
            && let Some(e) = unread
        {
            if e.bound_owner() == Some(crate::Whose::Caller) {
                return Err(resolve_nested_accessibility_bound(
                    e,
                    unread_owner,
                    sequence_deadline,
                ));
            }
            if outer_sequence_expired(unread_owner, sequence_deadline.has_passed()) {
                return Err(GlassError::caller_deadline_elapsed_with_guidance(
                    operation,
                    &e.to_string(),
                ));
            }
            return Err(e);
        }
        Ok(A11yPollOutcome {
            observation: last_observation
                .expect("a successful or soft-timed-out poll observed a tree"),
            satisfied: outcome.value.is_some(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            timed_out_by: outcome.value.is_none().then_some(whose),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::*;

    #[test]
    fn accessibility_deadline_helpers_require_every_provenance_clause() {
        assert!(should_reclassify_nested_bound(crate::Whose::Callee, false));
        assert!(!should_reclassify_nested_bound(crate::Whose::Caller, false));
        assert!(!should_reclassify_nested_bound(crate::Whose::Callee, true));

        assert!(reader_relative_caller_bound(
            &GlassError::caller_deadline_elapsed("reader")
        ));
        assert!(!reader_relative_caller_bound(&GlassError::Bounded {
            kind: crate::BoundKind::TimedOut,
            whose: crate::Whose::Callee,
            dispatch: crate::BoundDispatch::MayHaveDispatched,
            message: "reader ceiling".into(),
        }));
        assert!(!reader_relative_caller_bound(&GlassError::Backend(
            "reader failed".into()
        )));

        assert!(outer_sequence_expired(crate::Whose::Caller, true));
        assert!(!outer_sequence_expired(crate::Whose::Callee, true));
        assert!(!outer_sequence_expired(crate::Whose::Caller, false));
    }

    #[test]
    fn final_read_pause_reserves_a_quarter_up_to_the_headroom_cap() {
        assert_eq!(
            final_read_pause(Duration::from_millis(80)),
            Duration::from_millis(60)
        );
        assert_eq!(
            final_read_pause(Duration::from_millis(40)),
            Duration::from_millis(30)
        );
        assert_eq!(
            final_read_pause(Duration::from_millis(4)),
            Duration::from_millis(3)
        );
    }

    #[test]
    fn a_quiet_signal_reads_only_for_a_safety_or_periodic_refresh() {
        let reread_after = Duration::from_secs(1);
        assert!(!quiet_wait_needs_read(
            false,
            reread_after - Duration::from_millis(1),
            reread_after,
        ));
        assert!(quiet_wait_needs_read(false, reread_after, reread_after));
        assert!(quiet_wait_needs_read(true, Duration::ZERO, reread_after));
    }

    #[test]
    fn a_final_safety_read_is_scheduled_once_when_the_interval_reaches_the_deadline() {
        let interval = Duration::from_millis(100);
        assert!(should_schedule_final_read(false, Some(interval), interval));
        assert!(!should_schedule_final_read(true, Some(interval), interval));
        assert!(!should_schedule_final_read(false, None, interval));
        assert!(!should_schedule_final_read(
            false,
            Some(interval + Duration::from_millis(1)),
            interval
        ));
    }

    #[test]
    fn shared_poll_returns_the_last_fresh_observation_on_soft_timeout() {
        let mut first = fake_tree();
        first.root.name = Some("first".into());
        let mut second = fake_tree();
        second.root.name = Some("latest".into());
        let mut glass = glass_with_a11y_seq(FakePlatform::new(100, 100), vec![first, second]);
        glass.start(&spec()).unwrap();
        let out = glass
            .poll_accessibility_until(
                0,
                50,
                Deadline::UNBOUNDED,
                "test accessibility poll",
                |tree| tree.root.name.clone().unwrap_or_default(),
                |_| false,
            )
            .unwrap();
        assert!(!out.satisfied);
        assert_eq!(out.observation, "latest");
    }
}
