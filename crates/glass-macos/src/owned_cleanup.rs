//! Deadline policy for stopping recorded macOS process lifetimes, tested without native calls.

use glass_core::Deadline;
use std::io;
use std::time::{Duration, Instant};

const BATCH_BUDGET: Duration = Duration::from_secs(3);
const TERM_GRACE: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) fn bounded_deadline(deadline: Deadline, now: Instant) -> Deadline {
    // The broker's RPC wait is four seconds; leave time to return its receipt.
    Deadline::at(deadline.resolve(now + BATCH_BUDGET).0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopSignal {
    Term,
    Kill,
}

#[derive(Clone, Copy)]
enum StopState {
    Ready,
    TermSent(Instant),
    KillSent,
}

fn require_time(deadline: Deadline, now: Instant) -> io::Result<()> {
    if deadline
        .remaining_at(now)
        .is_some_and(|left| left.is_zero())
    {
        Err(io::Error::other("owned cleanup deadline elapsed"))
    } else {
        Ok(())
    }
}

pub(crate) fn stop_by<T: Copy>(
    processes: &[T],
    deadline: Deadline,
    mut now: impl FnMut() -> Instant,
    mut pause: impl FnMut(Duration),
    mut running: impl FnMut(T) -> io::Result<bool>,
    mut signal: impl FnMut(T, StopSignal) -> io::Result<()>,
) -> io::Result<()> {
    let mut states = vec![StopState::Ready; processes.len()];
    loop {
        require_time(deadline, now())?;
        let mut live = false;
        let mut sent = false;
        let mut next_escalation: Option<Instant> = None;
        for (process, state) in processes.iter().zip(&mut states) {
            require_time(deadline, now())?;
            let exists = running(*process)?;
            let observed_at = now();
            require_time(deadline, observed_at)?;
            if !exists {
                continue;
            }
            live = true;
            let (next_signal, next_state) = match *state {
                StopState::Ready => {
                    // Reserve at least half of the remaining time for KILL and exit observation.
                    let grace = TERM_GRACE
                        .min(deadline.remaining_at(observed_at).unwrap_or(TERM_GRACE * 2) / 2);
                    (StopSignal::Term, StopState::TermSent(observed_at + grace))
                }
                StopState::TermSent(at) if observed_at >= at => {
                    (StopSignal::Kill, StopState::KillSent)
                }
                StopState::TermSent(at) => {
                    next_escalation = Some(next_escalation.map_or(at, |next| next.min(at)));
                    continue;
                }
                StopState::KillSent => continue,
            };
            signal(*process, next_signal)?;
            *state = next_state;
            sent = true;
        }
        require_time(deadline, now())?;
        if !live {
            return Ok(());
        }
        // Observe signal results immediately, especially when a short caller budget is left.
        if sent {
            continue;
        }
        let before_pause = now();
        let mut wait = deadline.within(POLL_INTERVAL, before_pause);
        if let Some(at) = next_escalation {
            wait = wait.min(at.saturating_duration_since(before_pause));
        }
        pause(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn batch_budget_preserves_the_caller_and_leaves_rpc_reply_time() {
        let now = Instant::now();
        let caller = Deadline::at(now + Duration::from_millis(100));
        assert_eq!(bounded_deadline(caller, now), caller);
        assert_eq!(
            bounded_deadline(Deadline::UNBOUNDED, now).remaining_at(now),
            Some(Duration::from_secs(3))
        );
        let spent = Deadline::at(now - Duration::from_secs(1));
        assert_eq!(bounded_deadline(spent, now), spent);
    }

    #[test]
    fn an_expired_deadline_does_not_query_or_signal() {
        let now = Instant::now();
        let result = stop_by(
            &[1],
            Deadline::at(now),
            || now,
            |_| panic!("must not wait"),
            |_| panic!("must not query"),
            |_, _| panic!("must not signal"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_ownership_query_that_spends_the_deadline_cannot_signal_or_report_success() {
        for exists in [true, false] {
            let started = Instant::now();
            let ends = started + Duration::from_millis(100);
            let clock = Cell::new(started);
            let result = stop_by(
                &[1],
                Deadline::at(ends),
                || clock.get(),
                |_| panic!("must not wait"),
                |_| {
                    clock.set(ends);
                    Ok(exists)
                },
                |_, _| panic!("must not signal after the query exhausted the deadline"),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn a_short_budget_escalates_and_observes_exit_before_the_deadline() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let killed = Cell::new(false);
        let mut signals = Vec::new();
        stop_by(
            &[1],
            Deadline::at(started + Duration::from_millis(10)),
            || clock.get(),
            |wait| clock.set(clock.get() + wait),
            |_| {
                if killed.get() {
                    clock.set(clock.get() + Duration::from_millis(3));
                    Ok(false)
                } else {
                    Ok(true)
                }
            },
            |_, signal| {
                signals.push((signal, clock.get().duration_since(started)));
                killed.set(signal == StopSignal::Kill);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            signals,
            [
                (StopSignal::Term, Duration::ZERO),
                (StopSignal::Kill, Duration::from_millis(5))
            ]
        );
        assert_eq!(clock.get(), started + Duration::from_millis(8));
    }

    #[test]
    fn a_slow_initial_query_leaves_time_for_escalation_and_confirmation() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let first_query = Cell::new(true);
        let killed = Cell::new(false);
        let mut signals = Vec::new();
        stop_by(
            &[1],
            Deadline::at(started + Duration::from_millis(800)),
            || clock.get(),
            |wait| clock.set(clock.get() + wait),
            |_| {
                if first_query.replace(false) {
                    clock.set(started + Duration::from_millis(600));
                }
                if killed.get() {
                    clock.set(clock.get() + Duration::from_millis(80));
                    Ok(false)
                } else {
                    Ok(true)
                }
            },
            |_, signal| {
                signals.push((signal, clock.get().duration_since(started)));
                killed.set(signal == StopSignal::Kill);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            signals,
            [
                (StopSignal::Term, Duration::from_millis(600)),
                (StopSignal::Kill, Duration::from_millis(700))
            ]
        );
        assert_eq!(clock.get(), started + Duration::from_millis(780));
    }

    #[test]
    fn a_long_budget_preserves_the_full_term_grace() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let killed = Cell::new(false);
        let mut signals = Vec::new();
        stop_by(
            &[1],
            bounded_deadline(Deadline::UNBOUNDED, started),
            || clock.get(),
            |wait| clock.set(clock.get() + wait),
            |_| Ok(!killed.get()),
            |_, signal| {
                signals.push((signal, clock.get().duration_since(started)));
                killed.set(signal == StopSignal::Kill);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            signals,
            [
                (StopSignal::Term, Duration::ZERO),
                (StopSignal::Kill, Duration::from_millis(500))
            ]
        );
    }

    #[test]
    fn repeated_polls_and_multiple_processes_share_one_absolute_deadline() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let ends = started + Duration::from_millis(100);
        let mut signals = Vec::new();
        let result = stop_by(
            &[1, 2],
            Deadline::at(ends),
            || clock.get(),
            |wait| clock.set(clock.get() + wait),
            |_| Ok(true),
            |process, signal| {
                signals.push((process, signal));
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(clock.get(), ends);
        assert_eq!(
            signals,
            [
                (1, StopSignal::Term),
                (2, StopSignal::Term),
                (1, StopSignal::Kill),
                (2, StopSignal::Kill)
            ]
        );
    }

    #[test]
    fn an_exited_process_does_not_keep_its_escalation_timer_active() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let first_signalled = Cell::new(false);
        let result = stop_by(
            &[1, 2],
            Deadline::at(started + Duration::from_millis(100)),
            || clock.get(),
            |wait| {
                assert!(
                    !wait.is_zero(),
                    "an exited process must not cause busy polling"
                );
                clock.set(clock.get() + wait);
            },
            |process| Ok(process != 1 || !first_signalled.get()),
            |process, _| {
                if process == 1 {
                    first_signalled.set(true);
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(clock.get(), started + Duration::from_millis(100));
    }
}
