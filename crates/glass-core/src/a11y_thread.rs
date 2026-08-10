//! Bounded accessibility calls for a backend that cannot be driven from the caller's thread.
//!
//! UIA is COM and thread-affine; the AT-SPI reader drives an async API with `block_on`, which
//! panics inside the caller's tokio runtime. Both therefore run every bounded call — snapshot,
//! set_value, invoke — on a fresh detached OS thread and wait on a channel. (`subscribe_changes`
//! is not one of them: its own thread is the long-lived one.)
//!
//! Detached is load-bearing: a wait that ends early does not end the work, and the worker holds
//! the backend's own connection. So a caller told "no answer" has to learn *which* bound ended it —
//! its own spent budget, which [`crate::Glass::wait_for_element`] polls through, or a backend that
//! stopped answering, which it must not poll through (glass#341).
//!
//! The other readers do not need this: macOS AX runs inline, and the Android and iOS readers bound
//! their own subprocess calls.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::deadline::Deadline;
use crate::{GlassError, Result};

/// Which bound ended a wait. Decided before the wait, never inferred from its outcome — the
/// mistake glass#341 recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EndedBy {
    /// The caller's own deadline fell first: a spent budget, not a fault.
    Caller,
    /// The reader's ceiling ran out: the backend stopped answering.
    Ceiling,
}

/// The bounded calls one accessibility backend makes on its detached worker thread.
///
/// Configuration only. Anything stateful added here — an in-flight guard, a straggler count, a
/// reused thread — must be shared across calls, so it belongs behind a lock rather than in a
/// plain field.
pub struct A11yThread {
    backend: &'static str,
    ceiling: Duration,
}

/// A bounded call, for the message its failure carries.
#[derive(Clone, Copy)]
enum Op {
    Snapshot,
    SetValue,
    Invoke,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Snapshot => "snapshot",
            Op::SetValue => "set_value",
            Op::Invoke => "invoke",
        }
    }

    /// What a caller must know when the wait ended but the worker did not: the write or the action
    /// may yet reach the element. A read has nothing to land, so it says nothing.
    fn may_still_land(self) -> &'static str {
        match self {
            Op::Snapshot => "",
            Op::SetValue => "; the write may still land — re-snapshot before retrying",
            Op::Invoke => "; the action may still land — re-snapshot before retrying",
        }
    }
}

impl A11yThread {
    /// `backend` is what a failure message blames — "(`{backend}` not responding)" — and `ceiling`
    /// is the reader's hard cap, so a backend that goes quiet cannot hang the calling tool for
    /// longer than that.
    pub const fn new(backend: &'static str, ceiling: Duration) -> A11yThread {
        assert!(!backend.is_empty(), "a failure message must name a backend");
        assert!(
            !ceiling.is_zero(),
            "a zero ceiling fails every call while the work still runs"
        );
        A11yThread { backend, ceiling }
    }

    /// How long a call may block, and which bound ends it.
    ///
    /// Both come from one comparison, made before the wait: a call the *caller* cut short is a
    /// spent budget and one that used the whole ceiling is a backend that stopped answering, and
    /// inferring which afterwards is the mistake glass#341 recorded.
    fn bounded_wait(&self, deadline: Deadline) -> (Duration, EndedBy) {
        let own = Instant::now() + self.ceiling;
        let ended_by = if deadline.governs(own) {
            EndedBy::Caller
        } else {
            EndedBy::Ceiling
        };
        (
            deadline.cap(own).saturating_duration_since(Instant::now()),
            ended_by,
        )
    }

    /// Read the tree, bounded by whichever of the caller's deadline and the ceiling falls first.
    ///
    /// The deadline is checked before the spawn: a worker started for a caller that has stopped
    /// waiting holds the backend's connection while producing an answer nobody reads.
    pub fn snapshot<T: Send + 'static>(
        &self,
        deadline: Deadline,
        job: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        if deadline.has_passed() {
            return Err(self.never_answered(EndedBy::Caller));
        }
        let (wait, ended_by) = self.bounded_wait(deadline);
        self.detached(Op::Snapshot, wait, job, ended_by)
    }

    /// Write a value, bounded by the ceiling alone.
    ///
    /// No deadline: the seam places the capping obligation on `snapshot`, and the session builds
    /// both this context and `invoke`'s with [`Deadline::UNBOUNDED`], so there is none to honour.
    pub fn set_value(&self, job: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
        self.detached(Op::SetValue, self.ceiling, job, EndedBy::Ceiling)
    }

    /// Actuate the element, bounded by the ceiling alone.
    ///
    /// Its failures stay [`GlassError::AccessibilityUnavailable`], which
    /// [`GlassError::invoke_fallback_eligible`] excludes — so no pointer click is layered on top of
    /// an action that may be about to fire.
    pub fn invoke(&self, job: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
        self.detached(Op::Invoke, self.ceiling, job, EndedBy::Ceiling)
    }

    /// The verdict for a read that never answered. The caller's own deadline ending it is
    /// `AccessibilityNotReady`, which [`crate::Glass::wait_for_element`] polls through, where the
    /// backend going quiet for a whole ceiling is not.
    fn never_answered(&self, ended_by: EndedBy) -> GlassError {
        match ended_by {
            EndedBy::Caller => GlassError::AccessibilityNotReady(
                "no accessibility tree within the time this call allowed".into(),
            ),
            EndedBy::Ceiling => self.timed_out(Op::Snapshot),
        }
    }

    fn timed_out(&self, op: Op) -> GlassError {
        GlassError::AccessibilityUnavailable(format!(
            "accessibility {} timed out ({} not responding){}",
            op.name(),
            self.backend,
            op.may_still_land()
        ))
    }

    /// A worker that unwound. Never `AccessibilityNotReady`: a wait polls through that variant, so
    /// a reader panicking on every attempt would be reported as a caller running out of time, for
    /// the caller's whole window. The panic can land either side of the call reaching the element,
    /// so a write and an action keep their may-still-land caveat.
    fn worker_panicked(&self, op: Op) -> GlassError {
        GlassError::AccessibilityUnavailable(format!(
            "the {} accessibility worker panicked during {} — the panic is on glass's stderr{}",
            self.backend,
            op.name(),
            op.may_still_land()
        ))
    }

    fn detached<T: Send + 'static>(
        &self,
        op: Op,
        wait: Duration,
        job: impl FnOnce() -> Result<T> + Send + 'static,
        ended_by: EndedBy,
    ) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        // Named and fallible, unlike a bare `spawn`: a timed-out worker outlives its wait holding
        // the backend's connection, so several can be alive at once, and the OS refusing one is an
        // error this call can report rather than a panic in the caller.
        std::thread::Builder::new()
            .name(format!("glass-a11y-{}", self.backend))
            .spawn(move || {
                let _ = tx.send(job());
            })
            .map_err(|e| {
                GlassError::AccessibilityUnavailable(format!(
                    "could not start the {} accessibility worker: {e}",
                    self.backend
                ))
            })?;
        match rx.recv_timeout(wait) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => Err(match op {
                Op::Snapshot => self.never_answered(ended_by),
                _ => self.timed_out(op),
            }),
            // The sender drops unsent only when the worker unwinds, so this is a panic, not a
            // slow answer — a timeout would claim the backend is alive and still working.
            Err(RecvTimeoutError::Disconnected) => Err(self.worker_panicked(op)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CEILING: Duration = Duration::from_secs(10);
    /// Short enough that a test can wait out the whole thing.
    const IMPATIENT: Duration = Duration::from_millis(20);

    fn reader() -> A11yThread {
        A11yThread::new("a11y bus", CEILING)
    }

    fn impatient() -> A11yThread {
        A11yThread::new("a11y bus", IMPATIENT)
    }

    /// A job that never answers inside an [`IMPATIENT`] ceiling. Half a second, not thirty: each
    /// call leaves this thread sleeping after the test has moved on.
    fn hangs() -> impl FnOnce() -> Result<()> + Send + 'static {
        || {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        }
    }

    /// glass#338: only the reader can hold a call inside the caller's timeout — the worker is
    /// detached, so nothing outside it can shorten one that has started.
    #[test]
    fn a_read_is_bounded_by_the_caller_when_that_falls_first() {
        let (wait, ended_by) = reader().bounded_wait(Deadline::from_millis(50));
        assert!(wait <= Duration::from_millis(50), "{wait:?}");
        assert_eq!(ended_by, EndedBy::Caller);
    }

    /// The other direction: without it the test above passes on a reader that waits for nothing.
    #[test]
    fn a_caller_that_names_no_deadline_leaves_the_read_its_own_ceiling() {
        let (wait, ended_by) = reader().bounded_wait(Deadline::UNBOUNDED);
        assert!(wait > CEILING - Duration::from_secs(1), "{wait:?}");
        assert_eq!(ended_by, EndedBy::Ceiling);
    }

    /// The variant decides whether a wait polls on or fails, so the two causes must not collapse:
    /// a backend that went quiet for the whole ceiling is a real fault, and a caller that ran out
    /// of its own time is not.
    #[test]
    fn a_read_the_caller_cut_short_reads_as_not_ready_where_a_quiet_backend_does_not() {
        assert!(matches!(
            reader().never_answered(EndedBy::Caller),
            GlassError::AccessibilityNotReady(_)
        ));
        assert!(matches!(
            reader().never_answered(EndedBy::Ceiling),
            GlassError::AccessibilityUnavailable(_)
        ));
    }

    #[test]
    fn a_timeout_names_the_backend_that_stopped_answering() {
        let e = A11yThread::new("UIA", CEILING).never_answered(EndedBy::Ceiling);
        assert!(e.to_string().contains("UIA not responding"), "{e}");
    }

    #[test]
    fn a_spent_deadline_is_refused_without_starting_a_worker() {
        // With the pre-check gone the wait computes to zero and the caller still gets `NotReady`,
        // so only the worker's absence distinguishes the two. Asserted as a disconnect rather than
        // a quiet window, which would be a race: refusing drops `job` unrun and its `Sender` with
        // it, where a worker that ran would have sent. No load can turn one into the other.
        let (started, ran) = mpsc::channel();
        let r: Result<()> = reader().snapshot(Deadline::from_millis(0), move || {
            let _ = started.send(());
            Ok(())
        });
        assert!(
            matches!(r, Err(GlassError::AccessibilityNotReady(_))),
            "{r:?}"
        );
        assert!(
            matches!(ran.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
            "a worker must not be started for a caller that has stopped waiting: it is detached \
             and holds the backend's connection while producing an answer nobody reads"
        );
    }

    #[test]
    fn an_answer_within_the_bound_is_returned() {
        assert_eq!(reader().snapshot(Deadline::UNBOUNDED, || Ok(7)).unwrap(), 7);
    }

    #[test]
    fn the_jobs_own_error_is_returned_rather_than_a_timeout() {
        let r: Result<()> = reader().set_value(|| Err(GlassError::AxUnsupported));
        assert!(matches!(r, Err(GlassError::AxUnsupported)), "{r:?}");
    }

    #[test]
    fn a_job_that_outruns_the_ceiling_names_the_operation_that_timed_out() {
        let e = impatient().set_value(hangs()).unwrap_err();
        assert!(e.to_string().contains("set_value timed out"), "{e}");

        let e = impatient().invoke(hangs()).unwrap_err();
        assert!(e.to_string().contains("invoke timed out"), "{e}");
        // The variant, not the prose, is what withholds the pointer-click fallback from an action
        // that may be about to fire.
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    /// The worker outlives the wait for a write as it does for an action — an agent told only
    /// "timed out" retypes the text, and the abandoned worker writes it again.
    #[test]
    fn a_write_or_an_action_that_timed_out_says_it_may_still_land() {
        for e in [
            impatient().set_value(hangs()).unwrap_err(),
            impatient().invoke(hangs()).unwrap_err(),
        ] {
            assert!(e.to_string().contains("may still land"), "{e}");
        }
    }

    /// A read has nothing in flight to land, so the caveat would be noise.
    #[test]
    fn a_read_that_timed_out_claims_nothing_about_landing() {
        let e = impatient()
            .snapshot(Deadline::UNBOUNDED, hangs())
            .unwrap_err();
        assert!(e.to_string().contains("snapshot timed out"), "{e}");
        assert!(!e.to_string().contains("may still land"), "{e}");
    }

    /// The direction that costs more: blamed on the backend, a slow app during a
    /// `wait_for_element` aborts the whole wait instead of polling on.
    #[test]
    fn a_read_the_caller_ran_out_of_time_for_blames_the_caller_not_the_backend() {
        // A live deadline well inside the ceiling, against a job that will not answer within it.
        let r: Result<()> = reader().snapshot(Deadline::from_millis(30), hangs());
        assert!(
            matches!(r, Err(GlassError::AccessibilityNotReady(_))),
            "{r:?}"
        );
    }

    #[test]
    fn a_read_that_outruns_the_ceiling_blames_the_backend_not_the_caller() {
        let e = impatient()
            .snapshot(Deadline::UNBOUNDED, hangs())
            .unwrap_err();
        assert!(
            matches!(e, GlassError::AccessibilityUnavailable(_)),
            "a caller that named no deadline cannot be the one that ran out: {e}"
        );
    }

    /// A panicking worker drops its sender, which `recv_timeout` reports at once. Called a
    /// timeout, that claims the backend is alive and working; for a read it claims the caller ran
    /// out of time, which `wait_for_element` polls straight through.
    #[test]
    fn a_worker_that_panicked_is_not_reported_as_a_backend_that_went_quiet() {
        let e: GlassError = reader()
            .set_value(|| panic!("the backend crate unwound"))
            .unwrap_err();
        assert!(e.to_string().contains("panicked"), "{e}");
        assert!(!e.to_string().contains("timed out"), "{e}");
    }

    #[test]
    fn a_panicking_read_is_never_the_variant_a_wait_polls_through() {
        let e = reader()
            .snapshot(Deadline::from_millis(60_000), || -> Result<()> {
                panic!("the backend crate unwound")
            })
            .unwrap_err();
        assert!(
            matches!(e, GlassError::AccessibilityUnavailable(_)),
            "a panic reported as NotReady is re-attempted for the caller's whole window: {e}"
        );
    }

    /// A panic can land either side of the call reaching the element, so the caveat a read does
    /// not need still applies here.
    #[test]
    fn a_panicking_action_still_says_it_may_have_landed() {
        let e = reader()
            .invoke(|| panic!("unwound after dispatch"))
            .unwrap_err();
        assert!(e.to_string().contains("may still land"), "{e}");
    }
}
