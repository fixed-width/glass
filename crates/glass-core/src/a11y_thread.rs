//! Bounded calls for an accessibility backend whose platform API cannot run on the caller's
//! thread.
//!
//! AT-SPI is async and would `block_on` inside the caller's tokio runtime; UIA is COM and
//! thread-affine. Both readers therefore run every call on a fresh detached OS thread and wait on
//! a channel. Detached is the load-bearing word: a wait that ends early does not end the work, so
//! a caller told "no answer" has to learn *which* bound ended it — its own spent budget, which
//! [`crate::Glass::wait_for_element`] polls through, or a backend that stopped answering, which it
//! must not (glass#341).
//!
//! The macOS reader runs inline and does not use this: AX has no thread-affinity requirement.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::accessibility::AxDeadline;
use crate::{GlassError, Result};

/// One backend's detached-thread calls, each bounded by the caller's deadline or the reader's own
/// ceiling, whichever falls first.
pub struct ThreadBoundReader {
    subject: &'static str,
    ceiling: Duration,
}

impl ThreadBoundReader {
    /// `subject` names the platform in a timeout message — "(`{subject}` not responding)" — and
    /// `ceiling` is the reader's hard cap, so a backend that goes quiet cannot hang the calling
    /// tool for longer than that.
    pub const fn new(subject: &'static str, ceiling: Duration) -> ThreadBoundReader {
        ThreadBoundReader { subject, ceiling }
    }

    /// The platform name this reader puts in a timeout message.
    pub const fn subject(&self) -> &'static str {
        self.subject
    }

    /// How long a call may block, and whether the caller's deadline — not the ceiling — is what
    /// ends the wait.
    ///
    /// Both come from one comparison, decided before the wait: a call the *caller* cut short is a
    /// spent budget, one that used the whole ceiling is a backend that stopped answering, and
    /// inferring which afterwards is the mistake glass#341 recorded.
    fn bounded_wait(&self, deadline: AxDeadline) -> (Duration, bool) {
        let own = Instant::now() + self.ceiling;
        (
            deadline.cap(own).saturating_duration_since(Instant::now()),
            deadline.governs(own),
        )
    }

    /// Read the tree, bounded by whichever of the caller's deadline and the ceiling falls first.
    ///
    /// The deadline is checked before the spawn: a worker started for a caller that has already
    /// stopped waiting outlives the answer nobody reads.
    pub fn snapshot<T: Send + 'static>(
        &self,
        deadline: AxDeadline,
        job: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        if deadline.has_passed() {
            return Err(self.never_answered(true));
        }
        let (wait, by_caller) = self.bounded_wait(deadline);
        self.detached(wait, job, || self.never_answered(by_caller))
    }

    /// The verdict for a read that never answered. The caller's own deadline ending it is
    /// `AccessibilityNotReady`, which [`crate::Glass::wait_for_element`] polls through, where the
    /// backend going quiet for a whole ceiling is not.
    fn never_answered(&self, by_caller: bool) -> GlassError {
        if by_caller {
            return GlassError::AccessibilityNotReady(
                "no accessibility tree within the time this call allowed".into(),
            );
        }
        GlassError::AccessibilityUnavailable(format!(
            "accessibility snapshot timed out ({} not responding)",
            self.subject
        ))
    }

    /// Write a value, bounded by the ceiling alone.
    pub fn write(&self, job: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
        self.detached(self.ceiling, job, || {
            GlassError::AccessibilityUnavailable(format!(
                "accessibility set_value timed out ({} not responding)",
                self.subject
            ))
        })
    }

    /// Actuate the element, bounded by the ceiling alone.
    ///
    /// Its timeout says the action may still land, because the worker outlives the wait — and it
    /// is deliberately not fallback-eligible (see [`GlassError::invoke_fallback_eligible`]), so no
    /// pointer click is layered on top of an action that may be about to fire.
    pub fn action(&self, job: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
        self.detached(self.ceiling, job, || {
            GlassError::AccessibilityUnavailable(format!(
                "accessibility invoke timed out ({} not responding); the action may still land — \
                 re-snapshot before retrying",
                self.subject
            ))
        })
    }

    fn detached<T: Send + 'static>(
        &self,
        wait: Duration,
        job: impl FnOnce() -> Result<T> + Send + 'static,
        timed_out: impl FnOnce() -> GlassError,
    ) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
        match rx.recv_timeout(wait) {
            Ok(r) => r,
            Err(_) => Err(timed_out()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CEILING: Duration = Duration::from_secs(10);

    fn reader() -> ThreadBoundReader {
        ThreadBoundReader::new("a11y bus", CEILING)
    }

    /// glass#338: only the reader can hold a call inside the caller's timeout — the worker is
    /// detached, so nothing outside it can shorten one that has started.
    #[test]
    fn a_read_is_bounded_by_the_caller_when_that_falls_first() {
        let (wait, by_caller) = reader().bounded_wait(AxDeadline::from_millis(50));
        assert!(wait <= Duration::from_millis(50), "{wait:?}");
        assert!(by_caller);
    }

    /// The other direction: without it the test above passes on a reader that waits for nothing.
    #[test]
    fn a_caller_that_names_no_deadline_leaves_the_read_its_own_ceiling() {
        let (wait, by_caller) = reader().bounded_wait(AxDeadline::UNBOUNDED);
        assert!(wait > CEILING - Duration::from_secs(1), "{wait:?}");
        assert!(!by_caller);
    }

    /// The variant decides whether a wait polls on or fails, so the two causes must not collapse:
    /// a backend that went quiet for the whole ceiling is a real fault, and a caller that ran out
    /// of its own time is not.
    #[test]
    fn a_read_the_caller_cut_short_reads_as_not_ready_where_a_quiet_backend_does_not() {
        assert!(matches!(
            reader().never_answered(true),
            GlassError::AccessibilityNotReady(_)
        ));
        assert!(matches!(
            reader().never_answered(false),
            GlassError::AccessibilityUnavailable(_)
        ));
    }

    #[test]
    fn a_timeout_names_the_backend_that_stopped_answering() {
        let e = ThreadBoundReader::new("UIA", CEILING).never_answered(false);
        assert!(e.to_string().contains("UIA not responding"), "{e}");
    }

    /// How long to watch for a worker that should not exist. Paired with the control below, which
    /// fails if this window is ever too short for a spawned worker to report.
    const WORKER_WATCH: Duration = Duration::from_millis(200);

    #[test]
    fn a_spent_deadline_is_refused_without_starting_a_worker() {
        // Not just the verdict: with the pre-check gone the wait computes to zero and the caller
        // still gets `NotReady`, so only the worker's own silence distinguishes the two.
        let (started, ran) = mpsc::channel();
        let r: Result<()> = reader().snapshot(AxDeadline::from_millis(0), move || {
            let _ = started.send(());
            Ok(())
        });
        assert!(
            matches!(r, Err(GlassError::AccessibilityNotReady(_))),
            "{r:?}"
        );
        assert!(
            ran.recv_timeout(WORKER_WATCH).is_err(),
            "a worker must not be started for a caller that has stopped waiting: it is detached, \
             holds the backend's connection, and outlives the answer nobody reads"
        );
    }

    #[test]
    fn a_worker_that_is_started_reports_inside_that_same_window() {
        // The control: without it the test above would pass on a window too short to see any
        // worker at all.
        let (started, ran) = mpsc::channel();
        let _: Result<()> = reader().snapshot(AxDeadline::UNBOUNDED, move || {
            let _ = started.send(());
            Ok(())
        });
        assert!(ran.recv_timeout(WORKER_WATCH).is_ok());
    }

    #[test]
    fn an_answer_within_the_bound_is_returned() {
        assert_eq!(
            reader().snapshot(AxDeadline::UNBOUNDED, || Ok(7)).unwrap(),
            7
        );
    }

    #[test]
    fn the_jobs_own_error_is_returned_rather_than_a_timeout() {
        let r: Result<()> = reader().write(|| Err(GlassError::AxUnsupported));
        assert!(matches!(r, Err(GlassError::AxUnsupported)), "{r:?}");
    }

    #[test]
    fn a_job_that_outruns_the_ceiling_times_out_naming_the_operation() {
        let slow = ThreadBoundReader::new("a11y bus", Duration::from_millis(20));
        let e = slow
            .write(|| {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            })
            .unwrap_err();
        assert!(e.to_string().contains("set_value timed out"), "{e}");

        let e = slow
            .action(|| {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            })
            .unwrap_err();
        assert!(e.to_string().contains("invoke timed out"), "{e}");
        assert!(e.to_string().contains("may still land"), "{e}");
    }

    #[test]
    fn a_read_that_outruns_the_ceiling_blames_the_backend_not_the_caller() {
        let slow = ThreadBoundReader::new("a11y bus", Duration::from_millis(20));
        let e = slow
            .snapshot(AxDeadline::UNBOUNDED, || {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            })
            .unwrap_err();
        assert!(
            matches!(e, GlassError::AccessibilityUnavailable(_)),
            "a caller that named no deadline cannot be the one that ran out: {e}"
        );
    }
}
