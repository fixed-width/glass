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

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::deadline::{Deadline, Whose};
use crate::{GlassError, Result};

/// The bounded calls one accessibility backend makes on its detached worker thread.
///
/// Configuration only. Anything stateful added here — an in-flight guard, a straggler count, a
/// reused thread — must be shared across calls, so it belongs behind a lock rather than in a
/// plain field.
pub struct A11yThread {
    backend: &'static str,
    ceiling: Duration,
}

const SET_VALUE_PENDING: u8 = 0;
const SET_VALUE_DISPATCHED: u8 = 1;
const SET_VALUE_CANCELLED: u8 = 2;

/// The operation-specific dispatch gate for a detached native value mutation.
///
/// A backend performs all target resolution and guard reads before calling [`Self::dispatch`]. The
/// first call from any clone atomically claims permission to begin the native setter. Every later
/// call is refused, including after the enclosing operation returns. If the waiting caller has
/// already timed out, the setter is also refused, so a timeout reported as pre-write cannot later
/// mutate the value on the detached worker.
#[derive(Clone)]
pub struct SetValueDispatch {
    state: Arc<AtomicU8>,
}

impl SetValueDispatch {
    fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(SET_VALUE_PENDING)),
        }
    }

    fn begin(&self) -> Result<()> {
        match self.state.compare_exchange(
            SET_VALUE_PENDING,
            SET_VALUE_DISPATCHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(SET_VALUE_DISPATCHED) => Err(GlassError::Backend(
                "native accessibility set_value mutation was already dispatched".to_string(),
            )),
            Err(SET_VALUE_CANCELLED) => Err(GlassError::deadline_not_started(
                "native accessibility set_value mutation",
            )),
            Err(other) => unreachable!("unknown set_value dispatch state {other}"),
        }
    }

    /// Begin one native value mutation, or refuse it if the caller already stopped waiting.
    pub fn dispatch<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.begin()?;
        work()
    }

    /// Async form of [`Self::dispatch`] for native setters driven by an async accessibility API.
    pub async fn dispatch_async<T>(
        &self,
        work: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        self.begin()?;
        work.await
    }

    /// Atomically seal an unclaimed mutation capability. Returns whether dispatch already won.
    fn cancel_or_dispatched(&self) -> bool {
        match self.state.compare_exchange(
            SET_VALUE_PENDING,
            SET_VALUE_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(SET_VALUE_CANCELLED) => false,
            Err(SET_VALUE_DISPATCHED) => true,
            Err(other) => unreachable!("unknown set_value dispatch state {other}"),
        }
    }

    fn was_dispatched(&self) -> bool {
        self.state.load(Ordering::Acquire) == SET_VALUE_DISPATCHED
    }
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

    /// How long a call may block, and which bound ends it — one comparison, made before the wait
    /// (glass#341, glass#432).
    ///
    /// One clock read, so the ceiling branch is exactly `ceiling` rather than `ceiling` minus
    /// whatever fell between two reads. It trades the other way by the same nanoseconds on the
    /// caller branch, where the wait now expires a read after the deadline instead of before.
    fn bounded_wait(&self, deadline: Deadline) -> (Duration, Whose) {
        let now = Instant::now();
        let (ends, whose) = deadline.resolve(now + self.ceiling);
        (ends.saturating_duration_since(now), whose)
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
            return Err(GlassError::deadline_not_started(
                "native accessibility snapshot",
            ));
        }
        let (wait, ended_by) = self.bounded_wait(deadline);
        self.detached(
            Op::Snapshot,
            wait,
            job,
            || self.never_answered(ended_by),
            || self.worker_panicked(Op::Snapshot),
        )
    }

    /// Write under the nearer caller/ceiling deadline. The detached write may still land after the
    /// caller stops waiting, so a caller timeout remains post-dispatch and fallback-ineligible.
    pub fn set_value(
        &self,
        target: u32,
        deadline: Deadline,
        job: impl FnOnce(SetValueDispatch) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility set_value",
            ));
        }
        let (wait, ended_by) = self.bounded_wait(deadline);
        let dispatch = SetValueDispatch::new();
        let worker_dispatch = dispatch.clone();
        let timeout_dispatch = dispatch.clone();
        let panic_dispatch = dispatch.clone();
        let result = self.detached(
            Op::SetValue,
            wait,
            move || job(worker_dispatch),
            || self.set_value_no_answer(target, ended_by, &timeout_dispatch),
            || self.set_value_panicked(target, &panic_dispatch),
        );
        let was_dispatched = dispatch.cancel_or_dispatched();
        match result {
            Err(error) if was_dispatched && !error.set_value_failed_after_writing() => {
                Err(GlassError::AxWriteUnconfirmed(
                    target,
                    format!(
                        "the native value mutation was dispatched but failed before it could be confirmed ({error})"
                    ),
                ))
            }
            result => result,
        }
    }

    /// Actuate under the nearer caller/ceiling deadline, keeping timeouts fallback-ineligible
    /// because the detached action may still land.
    pub fn invoke(
        &self,
        deadline: Deadline,
        job: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility invoke",
            ));
        }
        let (wait, ended_by) = self.bounded_wait(deadline);
        self.detached(
            Op::Invoke,
            wait,
            job,
            || match ended_by {
                Whose::Caller => GlassError::caller_deadline_elapsed_with_guidance(
                    "native accessibility invoke",
                    "the action may still land; re-snapshot before retrying",
                ),
                Whose::Callee => self.timed_out(Op::Invoke),
            },
            || self.worker_panicked(Op::Invoke),
        )
    }

    fn set_value_no_answer(
        &self,
        target: u32,
        ended_by: Whose,
        dispatch: &SetValueDispatch,
    ) -> GlassError {
        if dispatch.cancel_or_dispatched() {
            let cause = match ended_by {
                Whose::Caller => "the caller deadline elapsed",
                Whose::Callee => "the accessibility backend timed out",
            };
            return GlassError::AxWriteUnconfirmed(
                target,
                format!(
                    "{cause} after the native value mutation was dispatched; it may still land"
                ),
            );
        }
        match ended_by {
            Whose::Caller => GlassError::caller_deadline_elapsed_with_guidance(
                "native accessibility set_value pre-write work",
                "the value mutation was not dispatched",
            ),
            Whose::Callee => GlassError::AccessibilityUnavailable(format!(
                "accessibility set_value timed out ({} not responding) before the value mutation was dispatched",
                self.backend
            )),
        }
    }

    fn set_value_panicked(&self, target: u32, dispatch: &SetValueDispatch) -> GlassError {
        if dispatch.was_dispatched() {
            GlassError::AxWriteUnconfirmed(
                target,
                format!(
                    "the {} accessibility worker panicked after the native value mutation was dispatched",
                    self.backend
                ),
            )
        } else {
            self.worker_panicked(Op::SetValue)
        }
    }

    /// The verdict for a read that never answered. A caller-owned bound remains structural so the
    /// caller can distinguish its spent sequence budget from a backend that went quiet.
    fn never_answered(&self, ended_by: Whose) -> GlassError {
        match ended_by {
            Whose::Caller => GlassError::caller_deadline_elapsed_with_guidance(
                "native accessibility snapshot",
                "no accessibility tree became available within the time this call allowed",
            ),
            Whose::Callee => self.timed_out(Op::Snapshot),
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

    /// `on_timeout` rather than a `Whose`: only a snapshot has two bounds to choose between, and
    /// handing the other two a verdict this never read let them pass a wrong one unnoticed.
    fn detached<T: Send + 'static>(
        &self,
        _op: Op,
        wait: Duration,
        job: impl FnOnce() -> Result<T> + Send + 'static,
        on_timeout: impl FnOnce() -> GlassError,
        on_disconnect: impl FnOnce() -> GlassError,
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
            Err(RecvTimeoutError::Timeout) => Err(on_timeout()),
            // The sender drops unsent only when the worker unwinds, so this is a panic, not a
            // slow answer — a timeout would claim the backend is alive and still working.
            Err(RecvTimeoutError::Disconnected) => Err(on_disconnect()),
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
        assert_eq!(ended_by, Whose::Caller);

        // And the whole bound, not zero: a Caller branch that waits for nothing spawns a worker,
        // then reports every read in the window as the caller running out of time.
        let (wait, _) = reader().bounded_wait(Deadline::from_millis(5_000));
        assert!(wait > Duration::from_secs(4), "{wait:?}");
    }

    /// The other direction: without it the test above passes on a reader that waits for nothing.
    #[test]
    fn a_caller_that_names_no_deadline_leaves_the_read_its_own_ceiling() {
        let (wait, ended_by) = reader().bounded_wait(Deadline::UNBOUNDED);
        // Exactly `CEILING`: the two-read shape this replaced came up short by whatever fell
        // between the reads.
        assert_eq!(wait, CEILING, "{wait:?}");
        assert_eq!(ended_by, Whose::Callee);
    }

    /// The structural owner decides whether a wait polls on or fails, so the two causes must not
    /// collapse: a backend that went quiet for the whole ceiling is a real fault, and a caller that
    /// ran out of its own time is not.
    #[test]
    fn a_read_the_caller_cut_short_keeps_the_caller_as_its_structural_owner() {
        let caller = reader().never_answered(Whose::Caller);
        assert_eq!(caller.bound(), Some(crate::BoundKind::TimedOut), "{caller}");
        assert_eq!(caller.bound_owner(), Some(Whose::Caller), "{caller}");
        assert_eq!(
            caller.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "{caller}"
        );

        assert!(matches!(
            reader().never_answered(Whose::Callee),
            GlassError::AccessibilityUnavailable(_)
        ));
    }

    #[test]
    fn a_timeout_names_the_backend_that_stopped_answering() {
        let e = A11yThread::new("UIA", CEILING).never_answered(Whose::Callee);
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
        let error = r.expect_err("a spent deadline must be refused");
        assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted), "{error}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched),
            "{error}"
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
        let r: Result<()> =
            reader().set_value(0, Deadline::UNBOUNDED, |_| Err(GlassError::AxUnsupported));
        assert!(matches!(r, Err(GlassError::AxUnsupported)), "{r:?}");
    }

    #[test]
    fn a_job_that_outruns_the_ceiling_names_the_operation_that_timed_out() {
        let e = impatient()
            .set_value(0, Deadline::UNBOUNDED, |_| hangs()())
            .unwrap_err();
        assert!(e.to_string().contains("set_value timed out"), "{e}");

        let e = impatient()
            .invoke(Deadline::UNBOUNDED, hangs())
            .unwrap_err();
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
            impatient()
                .set_value(0, Deadline::UNBOUNDED, |dispatch| {
                    dispatch.dispatch(hangs())
                })
                .unwrap_err(),
            impatient()
                .invoke(Deadline::UNBOUNDED, hangs())
                .unwrap_err(),
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
        let error = reader()
            .snapshot(Deadline::from_millis(30), hangs())
            .expect_err("the caller's deadline must end the read");
        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut), "{error}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "{error}"
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

        // The same verdict with a deadline present but further out — the shape of every read
        // `wait_for_element` makes, where blaming the caller would poll straight through a backend
        // that has stopped answering (glass#341).
        let e = impatient()
            .snapshot(Deadline::from_millis(60_000), hangs())
            .unwrap_err();
        assert!(
            matches!(e, GlassError::AccessibilityUnavailable(_)),
            "the ceiling fell first, so the caller still had time: {e}"
        );
    }

    /// A panicking worker drops its sender, which `recv_timeout` reports at once. Called a
    /// timeout, that claims the backend is alive and working; for a read it claims the caller ran
    /// out of time, which `wait_for_element` polls straight through.
    #[test]
    fn a_worker_that_panicked_is_not_reported_as_a_backend_that_went_quiet() {
        let e: GlassError = reader()
            .set_value(0, Deadline::UNBOUNDED, |_| {
                panic!("the backend crate unwound")
            })
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
            .invoke(Deadline::UNBOUNDED, || panic!("unwound after dispatch"))
            .unwrap_err();
        assert!(e.to_string().contains("may still land"), "{e}");
    }

    #[test]
    fn a_spent_invoke_deadline_starts_no_worker() {
        let (started, ran) = mpsc::channel();
        let r = reader().invoke(Deadline::from_millis(0), move || {
            let _ = started.send(());
            Ok(())
        });
        assert!(r.is_err(), "{r:?}");
        assert!(!r.unwrap_err().invoke_fallback_eligible());
        assert!(matches!(
            ran.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn a_hanging_set_value_returns_a_caller_owned_timeout() {
        let error = reader()
            .set_value(0, Deadline::from_millis(20), |_| hangs()())
            .unwrap_err();

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert!(
            error
                .to_string()
                .contains("value mutation was not dispatched"),
            "{error}"
        );
        assert!(!error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_timeout_during_pre_write_work_is_not_an_unconfirmed_value_write() {
        let error = reader()
            .set_value(7, Deadline::from_millis(20), |_| hangs()())
            .expect_err("pre-write resolution outlives the caller");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert!(!error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn sequential_dispatch_clones_execute_only_one_value_mutation() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_mutations = Arc::clone(&mutations);

        let result = reader().set_value(7, Deadline::UNBOUNDED, move |dispatch| {
            let second = dispatch.clone();
            dispatch.dispatch(|| {
                worker_mutations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })?;
            second.dispatch(|| {
                worker_mutations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 1, "{result:?}");
        let error = result.expect_err("the duplicate dispatch claim must be rejected");
        assert!(
            matches!(error, GlassError::AxWriteUnconfirmed(7, _)),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn racing_dispatch_clones_execute_only_one_value_mutation() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_mutations = Arc::clone(&mutations);

        let result = reader().set_value(7, Deadline::UNBOUNDED, move |dispatch| {
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let first_barrier = Arc::clone(&barrier);
            let second_barrier = Arc::clone(&barrier);
            let first_dispatch = dispatch.clone();
            let first_mutations = Arc::clone(&worker_mutations);
            let second_mutations = Arc::clone(&worker_mutations);
            let first = std::thread::spawn(move || {
                first_barrier.wait();
                first_dispatch.dispatch(|| {
                    first_mutations.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            });
            let second = std::thread::spawn(move || {
                second_barrier.wait();
                dispatch.dispatch(|| {
                    second_mutations.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            });
            barrier.wait();

            for result in [
                first.join().expect("the first claimant must not panic"),
                second.join().expect("the second claimant must not panic"),
            ] {
                result?;
            }
            Ok(())
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 1, "{result:?}");
        let error = result.expect_err("one racing dispatch claim must be rejected");
        assert!(
            matches!(error, GlassError::AxWriteUnconfirmed(7, _)),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_dispatch_clone_cannot_mutate_after_set_value_returns() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_mutations = Arc::clone(&mutations);
        let (late_tx, late_rx) = mpsc::channel();

        reader()
            .set_value(7, Deadline::UNBOUNDED, move |dispatch| {
                let late = dispatch.clone();
                dispatch.dispatch(|| {
                    worker_mutations.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })?;
                assert!(
                    late_tx.send(late).is_ok(),
                    "the test must retain the escaped dispatch clone"
                );
                Ok(())
            })
            .expect("the first value mutation must complete");

        let late = late_rx
            .recv()
            .expect("the worker must send its escaped clone");
        let late_result = late.dispatch(|| {
            mutations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 1, "{late_result:?}");
        assert!(late_result.is_err(), "the late claim must be rejected");
    }

    #[test]
    fn a_successful_noop_seals_an_escaped_dispatch_before_return() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (late_tx, late_rx) = mpsc::channel();

        reader()
            .set_value(7, Deadline::UNBOUNDED, move |dispatch| {
                assert!(
                    late_tx.send(dispatch.clone()).is_ok(),
                    "the test must retain the escaped dispatch clone"
                );
                Ok(())
            })
            .expect("a backend no-op remains a successful set_value");

        let late = late_rx
            .recv()
            .expect("the worker must send its escaped clone");
        let late_result = late.dispatch(|| {
            mutations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 0, "{late_result:?}");
        assert!(
            late_result.is_err(),
            "the late first claim must be rejected"
        );
    }

    #[test]
    fn a_pre_write_error_seals_an_escaped_dispatch_before_return() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (late_tx, late_rx) = mpsc::channel();

        let result = reader().set_value(7, Deadline::UNBOUNDED, move |dispatch| {
            assert!(
                late_tx.send(dispatch.clone()).is_ok(),
                "the test must retain the escaped dispatch clone"
            );
            Err(GlassError::AxUnsupported)
        });
        assert!(
            matches!(result, Err(GlassError::AxUnsupported)),
            "the pre-write error must remain unchanged: {result:?}"
        );

        let late = late_rx
            .recv()
            .expect("the worker must send its escaped clone");
        let late_result = late.dispatch(|| {
            mutations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 0, "{late_result:?}");
        assert!(
            late_result.is_err(),
            "the late first claim must be rejected"
        );
    }

    #[test]
    fn a_timeout_after_native_value_dispatch_is_an_unconfirmed_write() {
        let error = reader()
            .set_value(7, Deadline::from_millis(20), |dispatch| {
                dispatch.dispatch(hangs())
            })
            .expect_err("the native setter outlives the caller");

        assert!(
            matches!(error, GlassError::AxWriteUnconfirmed(7, _)),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_timed_out_pre_write_worker_cannot_dispatch_the_value_later() {
        let wrote = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_wrote = std::sync::Arc::clone(&wrote);

        let error = reader()
            .set_value(7, Deadline::from_millis(20), move |dispatch| {
                std::thread::sleep(Duration::from_millis(60));
                dispatch.dispatch(|| {
                    worker_wrote.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            })
            .expect_err("the caller stops during pre-write work");
        assert!(!error.set_value_failed_after_writing(), "{error}");

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !wrote.load(std::sync::atomic::Ordering::SeqCst),
            "the detached worker dispatched after the caller had retained its cached value"
        );
    }

    #[test]
    fn a_hanging_invoke_returns_a_caller_owned_timeout() {
        let started = Instant::now();
        let error = reader()
            .invoke(Deadline::from_millis(20), hangs())
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert!(!error.invoke_fallback_eligible(), "{error}");
        assert!(
            error.to_string().contains("action may still land"),
            "{error}"
        );
    }

    #[test]
    fn an_unbounded_set_value_retains_the_backend_ceiling() {
        let error = impatient()
            .set_value(0, Deadline::UNBOUNDED, |_| hangs()())
            .unwrap_err();

        assert!(matches!(error, GlassError::AccessibilityUnavailable(_)));
        assert_eq!(error.bound_owner(), None);
    }

    #[test]
    fn an_unbounded_invoke_retains_the_backend_ceiling() {
        let started = Instant::now();
        let e = impatient()
            .invoke(Deadline::UNBOUNDED, hangs())
            .unwrap_err();
        assert!(started.elapsed() >= IMPATIENT, "{:?}", started.elapsed());
        assert!(e.to_string().contains("invoke timed out"), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert!(e.to_string().contains("may still land"), "{e}");
    }
}
