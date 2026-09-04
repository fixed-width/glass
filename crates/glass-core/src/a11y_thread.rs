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
use crate::{BoundDispatch, BoundKind, GlassError, Result};

/// The bounded calls one accessibility backend makes on its detached worker thread.
///
/// Native accessibility worker configuration; shared mutable state belongs behind a lock.
pub struct A11yThread {
    backend: &'static str,
    ceiling: Duration,
}

const MUTATION_PENDING: u8 = 0;
const MUTATION_DISPATCHED: u8 = 1;
const MUTATION_CANCELLED: u8 = 2;

/// A borrowed one-shot dispatch gate for detached native accessibility mutations.
///
/// [`Self::dispatch`] atomically claims the mutation after target resolution; cancellation wins if
/// the caller timed out. The borrowed capability cannot escape the worker job:
///
/// ```compile_fail
/// use glass_core::{A11yThread, Deadline, Result};
/// use std::time::Duration;
///
/// let _: Result<()> = A11yThread::new("example", Duration::from_secs(1)).invoke(
///     Deadline::UNBOUNDED,
///     |dispatch| {
///         let late = dispatch.clone();
///         std::thread::spawn(move || late.dispatch(|| Ok(())));
///         Ok(())
///     },
/// );
/// ```
pub struct A11yMutationDispatch {
    state: Arc<AtomicU8>,
    operation: &'static str,
    ends: Instant,
}

impl A11yMutationDispatch {
    fn new(operation: &'static str, ends: Instant) -> Self {
        Self {
            state: Arc::new(AtomicU8::new(MUTATION_PENDING)),
            operation,
            ends,
        }
    }

    fn duplicate(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            operation: self.operation,
            ends: self.ends,
        }
    }

    fn begin(&self) -> Result<()> {
        self.begin_at(Instant::now())
    }

    fn begin_at(&self, now: Instant) -> Result<()> {
        if now >= self.ends {
            return match self.state.compare_exchange(
                MUTATION_PENDING,
                MUTATION_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) | Err(MUTATION_CANCELLED) => Err(GlassError::deadline_not_started(&format!(
                    "native accessibility {} mutation",
                    self.operation
                ))),
                Err(MUTATION_DISPATCHED) => Err(GlassError::Backend(format!(
                    "native accessibility {} mutation was already dispatched",
                    self.operation
                ))),
                Err(other) => {
                    unreachable!("unknown accessibility mutation dispatch state {other}")
                }
            };
        }
        match self.state.compare_exchange(
            MUTATION_PENDING,
            MUTATION_DISPATCHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(MUTATION_DISPATCHED) => Err(GlassError::Backend(format!(
                "native accessibility {} mutation was already dispatched",
                self.operation
            ))),
            Err(MUTATION_CANCELLED) => Err(GlassError::deadline_not_started(&format!(
                "native accessibility {} mutation",
                self.operation
            ))),
            Err(other) => unreachable!("unknown accessibility mutation dispatch state {other}"),
        }
    }

    /// Begin one native mutation, or refuse it if the caller already stopped waiting.
    pub fn dispatch<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.begin()?;
        work()
    }

    /// Async form of [`Self::dispatch`] for mutations driven by an async accessibility API.
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
            MUTATION_PENDING,
            MUTATION_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(MUTATION_CANCELLED) => false,
            Err(MUTATION_DISPATCHED) => true,
            Err(other) => unreachable!("unknown accessibility mutation dispatch state {other}"),
        }
    }

    fn was_dispatched(&self) -> bool {
        self.state.load(Ordering::Acquire) == MUTATION_DISPATCHED
    }
}

/// Compatibility name for callers that only use the dispatch gate with `set_value`.
pub type SetValueDispatch = A11yMutationDispatch;

struct MutationCompletion(A11yMutationDispatch);

impl Drop for MutationCompletion {
    fn drop(&mut self) {
        self.0.cancel_or_dispatched();
    }
}

fn run_mutation_job(
    dispatch: A11yMutationDispatch,
    job: impl FnOnce(&A11yMutationDispatch) -> Result<()>,
) -> Result<()> {
    let _completion = MutationCompletion(dispatch.duplicate());
    let result = job(&dispatch);
    let was_dispatched = dispatch.cancel_or_dispatched();
    match result {
        Err(error) if !was_dispatched && !error.invoke_fallback_eligible() => {
            Err(error.before_dispatch())
        }
        result => result,
    }
}

/// A bounded call, for the message its failure carries.
#[derive(Clone, Copy)]
enum Op {
    Snapshot,
    SetValue,
    Invoke,
    Focus,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Snapshot => "snapshot",
            Op::SetValue => "set_value",
            Op::Invoke => "invoke",
            Op::Focus => "focus",
        }
    }

    /// What a caller must know when the wait ended but the worker did not: the write or the action
    /// may yet reach the element. A read has nothing to land, so it says nothing.
    fn may_still_land(self) -> &'static str {
        match self {
            Op::Snapshot => "",
            Op::SetValue => "; the write may still land — re-snapshot before retrying",
            Op::Invoke | Op::Focus => "; the action may still land — re-snapshot before retrying",
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

    /// Resolve the absolute end and owner from one clock read (glass#341, glass#432).
    fn bounded_wait(&self, deadline: Deadline) -> (Instant, Whose) {
        let now = Instant::now();
        deadline.resolve(now + self.ceiling)
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
        let (ends, ended_by) = self.bounded_wait(deadline);
        self.detached(
            Op::Snapshot,
            ends,
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
        job: impl FnOnce(&A11yMutationDispatch) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility set_value",
            ));
        }
        let (ends, ended_by) = self.bounded_wait(deadline);
        let dispatch = A11yMutationDispatch::new("set_value", ends);
        let worker_dispatch = dispatch.duplicate();
        let timeout_dispatch = dispatch.duplicate();
        let panic_dispatch = dispatch.duplicate();
        let result = self.detached(
            Op::SetValue,
            ends,
            move || run_mutation_job(worker_dispatch, job),
            || self.set_value_no_answer(target, ended_by, &timeout_dispatch),
            || self.set_value_panicked(target, &panic_dispatch),
        );
        let was_dispatched = dispatch.cancel_or_dispatched();
        match result {
            Err(error) if was_dispatched && !error.set_value_failed_after_writing() => {
                Err(GlassError::write_unconfirmed_because(
                    target,
                    "the native value mutation was dispatched but failed before it could be confirmed",
                    error,
                ))
            }
            result => result,
        }
    }

    /// Actuate under the nearer caller or ceiling deadline; a claimed mutation remains
    /// fallback-ineligible after caller timeout.
    pub fn invoke(
        &self,
        deadline: Deadline,
        job: impl FnOnce(&A11yMutationDispatch) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.mutation(Op::Invoke, "invoke", deadline, job)
    }

    pub fn focus(
        &self,
        deadline: Deadline,
        job: impl FnOnce(&A11yMutationDispatch) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.mutation(Op::Focus, "focus", deadline, job)
    }

    fn mutation(
        &self,
        op: Op,
        operation: &'static str,
        deadline: Deadline,
        job: impl FnOnce(&A11yMutationDispatch) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(&format!(
                "native accessibility {operation}"
            )));
        }
        let (ends, ended_by) = self.bounded_wait(deadline);
        let dispatch = A11yMutationDispatch::new(operation, ends);
        let worker_dispatch = dispatch.duplicate();
        let timeout_dispatch = dispatch.duplicate();
        let panic_dispatch = dispatch.duplicate();
        let result = self.detached(
            op,
            ends,
            move || run_mutation_job(worker_dispatch, job),
            || self.mutation_no_answer(op, operation, ended_by, &timeout_dispatch),
            || self.mutation_panicked(op, operation, &panic_dispatch),
        );
        dispatch.cancel_or_dispatched();
        result
    }

    fn mutation_no_answer(
        &self,
        op: Op,
        operation: &'static str,
        ended_by: Whose,
        dispatch: &A11yMutationDispatch,
    ) -> GlassError {
        match (ended_by, dispatch.cancel_or_dispatched()) {
            (Whose::Caller, true) => GlassError::caller_deadline_elapsed_with_guidance(
                &format!("native accessibility {operation}"),
                "the action may still land; re-snapshot before retrying",
            ),
            (Whose::Caller, false) => GlassError::Bounded {
                kind: BoundKind::TimedOut,
                whose: Whose::Caller,
                dispatch: BoundDispatch::NotDispatched,
                message: format!(
                    "native accessibility {operation}: the caller deadline elapsed during target resolution; the action was not dispatched"
                ),
            },
            (Whose::Callee, true) => self.timed_out(op).after_dispatch(),
            (Whose::Callee, false) => GlassError::AccessibilityUnavailable(format!(
                "accessibility {operation} timed out ({} not responding) before the native action was dispatched",
                self.backend
            ))
            .before_dispatch(),
        }
    }

    fn mutation_panicked(
        &self,
        op: Op,
        operation: &'static str,
        dispatch: &A11yMutationDispatch,
    ) -> GlassError {
        let error = self.worker_panicked(op);
        if dispatch.was_dispatched() {
            error.after_dispatch()
        } else {
            GlassError::AccessibilityUnavailable(format!(
                "the {} accessibility worker panicked during {operation} before the native action was dispatched — the panic is on glass's stderr",
                self.backend,
            ))
            .before_dispatch()
        }
    }

    fn set_value_no_answer(
        &self,
        target: u32,
        ended_by: Whose,
        dispatch: &A11yMutationDispatch,
    ) -> GlassError {
        if dispatch.cancel_or_dispatched() {
            let (detail, source) = match ended_by {
                Whose::Caller => (
                    "the caller deadline elapsed after the native value mutation was dispatched; it may still land",
                    GlassError::caller_deadline_elapsed_with_guidance(
                        "native accessibility set_value",
                        "the value mutation may still land",
                    ),
                ),
                Whose::Callee => (
                    "the accessibility backend timed out after the native value mutation was dispatched; it may still land",
                    GlassError::Bounded {
                        kind: BoundKind::TimedOut,
                        whose: Whose::Callee,
                        dispatch: BoundDispatch::MayHaveDispatched,
                        message: format!(
                            "accessibility set_value timed out ({} not responding); the action may still land",
                            self.backend
                        ),
                    },
                ),
            };
            return GlassError::write_unconfirmed_because(target, detail, source);
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

    fn set_value_panicked(&self, target: u32, dispatch: &A11yMutationDispatch) -> GlassError {
        if dispatch.was_dispatched() {
            GlassError::write_unconfirmed_because(
                target,
                format!(
                    "the {} accessibility worker panicked after the native value mutation was dispatched",
                    self.backend
                ),
                self.worker_panicked(Op::SetValue),
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

    fn worker_spawn_failed(&self, error: std::io::Error) -> GlassError {
        GlassError::AccessibilityUnavailable(format!(
            "could not start the {} accessibility worker: {error}",
            self.backend
        ))
        .before_dispatch()
    }

    /// `on_timeout` rather than a `Whose`: only a snapshot has two bounds to choose between, and
    /// handing the other two a verdict this never read let them pass a wrong one unnoticed.
    fn detached<T: Send + 'static>(
        &self,
        _op: Op,
        ends: Instant,
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
            .map_err(|error| self.worker_spawn_failed(error))?;
        receive_by(rx, ends, on_timeout, on_disconnect)
    }
}

fn receive_by<T>(
    rx: mpsc::Receiver<Result<T>>,
    ends: Instant,
    on_timeout: impl FnOnce() -> GlassError,
    on_disconnect: impl FnOnce() -> GlassError,
) -> Result<T> {
    receive_by_with_clock(rx, ends, Instant::now, on_timeout, on_disconnect)
}

fn receive_by_with_clock<T>(
    rx: mpsc::Receiver<Result<T>>,
    ends: Instant,
    mut now: impl FnMut() -> Instant,
    on_timeout: impl FnOnce() -> GlassError,
    on_disconnect: impl FnOnce() -> GlassError,
) -> Result<T> {
    let remaining = ends.saturating_duration_since(now());
    if remaining.is_zero() {
        return Err(on_timeout());
    }
    match rx.recv_timeout(remaining) {
        Ok(result) if now() < ends => result,
        Ok(_) | Err(RecvTimeoutError::Timeout) => Err(on_timeout()),
        // A disconnected sender means the worker unwound before sending, not that it ran slowly.
        Err(RecvTimeoutError::Disconnected) => Err(on_disconnect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};

    struct CountedReady {
        polls: Arc<AtomicUsize>,
    }

    impl Future for CountedReady {
        type Output = Result<()>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
    }

    fn poll_once<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
        let mut context = Context::from_waker(std::task::Waker::noop());
        future.poll(&mut context)
    }

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
        let (ends, ended_by) = reader().bounded_wait(Deadline::from_millis(50));
        let wait = ends.saturating_duration_since(Instant::now());
        assert!(wait <= Duration::from_millis(50), "{wait:?}");
        assert_eq!(ended_by, Whose::Caller);

        // And the whole bound, not zero: a Caller branch that waits for nothing spawns a worker,
        // then reports every read in the window as the caller running out of time.
        let (ends, _) = reader().bounded_wait(Deadline::from_millis(5_000));
        let wait = ends.saturating_duration_since(Instant::now());
        assert!(wait > Duration::from_secs(4), "{wait:?}");
    }

    /// The other direction: without it the test above passes on a reader that waits for nothing.
    #[test]
    fn a_caller_that_names_no_deadline_leaves_the_read_its_own_ceiling() {
        let before = Instant::now() + CEILING;
        let (ends, ended_by) = reader().bounded_wait(Deadline::UNBOUNDED);
        let after = Instant::now() + CEILING;
        assert!((before..=after).contains(&ends), "{ends:?}");
        assert_eq!(ended_by, Whose::Callee);
    }

    /// Preserve a caller-owned bound so waits can distinguish spent sequence time from a silent
    /// backend.
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
    fn an_answer_already_queued_after_the_absolute_bound_is_still_a_timeout() {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(7)).unwrap();
        let ended = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();

        let result = receive_by(
            rx,
            ended,
            || GlassError::Backend("absolute bound won".into()),
            || GlassError::Backend("worker disconnected".into()),
        );

        assert!(
            matches!(result, Err(GlassError::Backend(ref message)) if message == "absolute bound won"),
            "{result:?}"
        );
    }

    #[test]
    fn an_answer_dequeued_at_or_after_the_absolute_bound_is_still_a_timeout() {
        let ends = Instant::now() + Duration::from_secs(1);
        for observed in [ends, ends + Duration::from_millis(1)] {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(7)).unwrap();
            let mut readings = [ends - Duration::from_millis(1), observed].into_iter();

            let result = receive_by_with_clock(
                rx,
                ends,
                || readings.next().unwrap(),
                || GlassError::Backend("absolute bound won".into()),
                || GlassError::Backend("worker disconnected".into()),
            );

            assert!(
                matches!(result, Err(GlassError::Backend(ref message)) if message == "absolute bound won"),
                "{observed:?}: {result:?}"
            );
        }
    }

    #[test]
    fn the_jobs_own_error_is_returned_rather_than_a_timeout() {
        let r: Result<()> =
            reader().set_value(0, Deadline::UNBOUNDED, |_| Err(GlassError::AxUnsupported));
        assert!(matches!(r, Err(GlassError::AxUnsupported)), "{r:?}");
    }

    #[test]
    fn focus_property_error_before_the_dispatch_gate_is_not_dispatched() {
        let error = reader()
            .focus(Deadline::UNBOUNDED, |_| {
                Err(GlassError::AccessibilityUnavailable(
                    "scripted focus property HRESULT".into(),
                ))
            })
            .expect_err("the scripted focus property read fails");

        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(
            matches!(error.cause(), GlassError::AccessibilityUnavailable(message) if message == "scripted focus property HRESULT"),
            "{error:?}"
        );
    }

    #[test]
    fn invoke_pattern_error_before_the_dispatch_gate_is_not_dispatched() {
        let error = reader()
            .invoke(Deadline::UNBOUNDED, |_| {
                Err(GlassError::AccessibilityUnavailable(
                    "scripted invoke pattern HRESULT".into(),
                ))
            })
            .expect_err("the scripted invoke pattern lookup fails");

        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(
            matches!(error.cause(), GlassError::AccessibilityUnavailable(message) if message == "scripted invoke pattern HRESULT"),
            "{error:?}"
        );
    }

    #[test]
    fn set_value_pattern_error_before_the_dispatch_gate_is_not_dispatched() {
        let error = reader()
            .set_value(7, Deadline::UNBOUNDED, |_| {
                Err(GlassError::AccessibilityUnavailable(
                    "scripted set-value pattern HRESULT".into(),
                ))
            })
            .expect_err("the scripted set-value pattern lookup fails");

        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(!error.set_value_failed_after_writing(), "{error:?}");
        assert!(
            matches!(error.cause(), GlassError::AccessibilityUnavailable(message) if message == "scripted set-value pattern HRESULT"),
            "{error:?}"
        );
    }

    #[test]
    fn pre_dispatch_native_unavailable_keeps_its_typed_fallback_cause() {
        for error in [
            reader()
                .invoke(Deadline::UNBOUNDED, |_| Err(GlassError::AxUnsupported))
                .expect_err("unsupported invoke"),
            reader()
                .invoke(Deadline::UNBOUNDED, |_| {
                    Err(GlassError::AxActionUnavailable(9))
                })
                .expect_err("unavailable invoke"),
        ] {
            assert!(error.invoke_fallback_eligible(), "{error:?}");
        }
    }

    #[test]
    fn a_job_that_outruns_the_ceiling_names_the_operation_that_timed_out() {
        let e = impatient()
            .set_value(0, Deadline::UNBOUNDED, |_| hangs()())
            .unwrap_err();
        assert!(e.to_string().contains("set_value timed out"), "{e}");

        let e = impatient()
            .invoke(Deadline::UNBOUNDED, |dispatch| dispatch.dispatch(hangs()))
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
                .invoke(Deadline::UNBOUNDED, |dispatch| dispatch.dispatch(hangs()))
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
        assert!(!e.set_value_failed_after_writing(), "{e}");
        assert_eq!(e.bound_dispatch(), None, "{e}");
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
            .invoke(Deadline::UNBOUNDED, |dispatch| {
                dispatch.dispatch(|| panic!("unwound after dispatch"))
            })
            .unwrap_err();
        assert!(e.to_string().contains("may still land"), "{e}");
    }

    #[test]
    fn a_worker_spawn_failure_is_explicitly_not_dispatched() {
        let error = reader().worker_spawn_failed(std::io::Error::other("thread limit reached"));

        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched),
            "the worker and native value mutation never started: {error}"
        );
        assert!(!error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_spent_invoke_deadline_starts_no_worker() {
        let (started, ran) = mpsc::channel();
        let r = reader().invoke(Deadline::from_millis(0), move |_| {
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
    fn sequential_dispatch_attempts_execute_only_one_value_mutation() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_mutations = Arc::clone(&mutations);

        let result = reader().set_value(7, Deadline::UNBOUNDED, move |dispatch| {
            dispatch.dispatch(|| {
                worker_mutations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })?;
            dispatch.dispatch(|| {
                worker_mutations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 1, "{result:?}");
        let error = result.expect_err("the duplicate dispatch claim must be rejected");
        assert!(
            matches!(error, GlassError::AxWriteUnconfirmedCaused { id: 7, .. }),
            "{error}"
        );
        assert!(matches!(error.cause(), GlassError::Backend(_)), "{error}");
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn racing_async_dispatch_attempts_poll_only_the_winning_mutation_future() {
        let dispatch = A11yMutationDispatch::new("set_value", Instant::now() + CEILING);
        let first = dispatch.duplicate();
        let second = dispatch.duplicate();
        let winner_polls = Arc::new(AtomicUsize::new(0));
        let loser_polls = Arc::new(AtomicUsize::new(0));
        let mut winner = Box::pin(first.dispatch_async(CountedReady {
            polls: Arc::clone(&winner_polls),
        }));
        let mut loser = Box::pin(second.dispatch_async(CountedReady {
            polls: Arc::clone(&loser_polls),
        }));

        assert!(matches!(poll_once(winner.as_mut()), Poll::Ready(Ok(()))));
        assert!(matches!(poll_once(loser.as_mut()), Poll::Ready(Err(_))));
        assert_eq!(winner_polls.load(Ordering::SeqCst), 1);
        assert_eq!(loser_polls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancelled_async_dispatch_never_polls_the_mutation_future() {
        let dispatch = A11yMutationDispatch::new("set_value", Instant::now() + CEILING);
        assert!(!dispatch.cancel_or_dispatched());
        let polls = Arc::new(AtomicUsize::new(0));
        let mut cancelled = Box::pin(dispatch.dispatch_async(CountedReady {
            polls: Arc::clone(&polls),
        }));

        let Poll::Ready(Err(error)) = poll_once(cancelled.as_mut()) else {
            panic!("a cancelled dispatch must refuse before awaiting its mutation");
        };
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mutation_dispatch_at_or_after_its_absolute_bound_is_refused() {
        let ends = Instant::now() + Duration::from_secs(1);
        for observed in [ends, ends + Duration::from_millis(1)] {
            let dispatch = A11yMutationDispatch::new("invoke", ends);

            let error = dispatch.begin_at(observed).unwrap_err();

            assert_eq!(error.bound(), Some(BoundKind::NotStarted), "{observed:?}");
            assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
            assert!(!dispatch.was_dispatched());
        }
    }

    #[test]
    fn racing_scoped_dispatch_attempts_execute_only_one_value_mutation() {
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_mutations = Arc::clone(&mutations);

        let result = reader().set_value(7, Deadline::UNBOUNDED, move |dispatch| {
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let first_barrier = Arc::clone(&barrier);
            let second_barrier = Arc::clone(&barrier);
            let first_mutations = Arc::clone(&worker_mutations);
            let second_mutations = Arc::clone(&worker_mutations);
            std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    first_barrier.wait();
                    dispatch.dispatch(|| {
                        first_mutations.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                });
                let second = scope.spawn(|| {
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
            })
        });

        assert_eq!(mutations.load(Ordering::SeqCst), 1, "{result:?}");
        let error = result.expect_err("one racing dispatch claim must be rejected");
        assert!(
            matches!(error, GlassError::AxWriteUnconfirmedCaused { id: 7, .. }),
            "{error}"
        );
        assert!(matches!(error.cause(), GlassError::Backend(_)), "{error}");
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn worker_completion_seals_a_pending_noop_before_returning_its_result() {
        let dispatch = A11yMutationDispatch::new("set_value", Instant::now() + CEILING);
        let escaped = dispatch.duplicate();

        let result = run_mutation_job(dispatch, |_| Ok(()));

        assert!(result.is_ok(), "{result:?}");
        assert!(
            escaped.dispatch(|| Ok(())).is_err(),
            "completion must be sealed before the worker result becomes observable"
        );
    }

    #[test]
    fn worker_completion_seals_a_pending_pre_write_error_before_returning_it() {
        let dispatch = A11yMutationDispatch::new("set_value", Instant::now() + CEILING);
        let escaped = dispatch.duplicate();

        let result = run_mutation_job(dispatch, |_| Err(GlassError::AxUnsupported));

        assert!(
            matches!(result, Err(GlassError::AxUnsupported)),
            "{result:?}"
        );
        assert!(
            escaped.dispatch(|| Ok(())).is_err(),
            "an error result must not leave an escaped first claim alive"
        );
    }

    #[test]
    fn worker_completion_seals_a_pending_token_before_unwinding() {
        let dispatch = A11yMutationDispatch::new("set_value", Instant::now() + CEILING);
        let escaped = dispatch.duplicate();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = run_mutation_job(dispatch, |_| panic!("scripted worker panic"));
        }));

        assert!(result.is_err(), "the scripted worker must unwind");
        assert!(
            escaped.dispatch(|| Ok(())).is_err(),
            "unwinding must seal the mutation before sender disconnect is observable"
        );
    }

    #[test]
    fn a_timeout_after_native_value_dispatch_is_an_unconfirmed_write() {
        let error = reader()
            .set_value(7, Deadline::from_millis(20), |dispatch| {
                dispatch.dispatch(hangs())
            })
            .expect_err("the native setter outlives the caller");

        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut), "{error}");
        assert!(
            matches!(error.cause(), GlassError::Bounded { .. }),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_backend_ceiling_after_native_value_dispatch_preserves_its_bound() {
        let error = A11yThread::new("a11y bus", Duration::from_millis(20))
            .set_value(7, Deadline::UNBOUNDED, |dispatch| {
                dispatch.dispatch(|| {
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(())
                })
            })
            .expect_err("the native setter outlives the backend ceiling");

        assert_eq!(error.bound_owner(), Some(Whose::Callee), "{error}");
        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "{error}"
        );
        assert!(
            matches!(error.cause(), GlassError::Bounded { .. }),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_dispatched_value_failure_exposes_its_tool_source() {
        let error = reader()
            .set_value(7, Deadline::UNBOUNDED, |dispatch| {
                dispatch.dispatch(|| {
                    Err(GlassError::ToolFailed {
                        call: "native setter".into(),
                        said: " transport refused \n".into(),
                    })
                })
            })
            .expect_err("the dispatched native setter reports a transport failure");

        assert!(
            matches!(error.cause(), GlassError::ToolFailed { .. }),
            "{error}"
        );
        assert_eq!(error.tool_said(), Some("transport refused"), "{error}");
        let source = std::error::Error::source(&error)
            .expect("the post-write verdict must retain its structured source");
        assert!(
            matches!(
                source.downcast_ref::<Box<GlassError>>().map(Box::as_ref),
                Some(GlassError::ToolFailed { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn a_post_write_verdict_overrides_an_inner_not_dispatched_retry_cause() {
        let error = reader()
            .set_value(7, Deadline::UNBOUNDED, |dispatch| {
                dispatch.dispatch(|| Err(GlassError::deadline_not_started("native retry")))
            })
            .expect_err("the first value mutation dispatched before its retry was refused");

        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
        assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "the value mutation outranks its inner retry provenance: {error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn a_timed_out_pre_write_worker_cannot_dispatch_the_value_later() {
        let wrote = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_wrote = std::sync::Arc::clone(&wrote);

        let error = reader()
            .set_value(7, Deadline::from_millis(100), move |dispatch| {
                std::thread::sleep(Duration::from_millis(500));
                dispatch.dispatch(|| {
                    worker_wrote.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            })
            .expect_err("the caller stops during pre-write work");
        assert!(!error.set_value_failed_after_writing(), "{error}");

        std::thread::sleep(Duration::from_millis(600));
        assert!(
            !wrote.load(std::sync::atomic::Ordering::SeqCst),
            "the detached worker dispatched after the caller had retained its cached value"
        );
    }

    #[test]
    fn a_hanging_invoke_returns_a_caller_owned_timeout() {
        let started = Instant::now();
        let error = reader()
            .invoke(Deadline::from_millis(20), |dispatch| {
                dispatch.dispatch(hangs())
            })
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
    fn focus_timeout_before_dispatch_is_retry_safe() {
        let ran = Arc::new(AtomicUsize::new(0));
        let worker_ran = Arc::clone(&ran);
        let thread = A11yThread::new("focus test", Duration::from_millis(100));
        let error = thread
            .focus(Deadline::from_millis(5), move |dispatch| {
                std::thread::sleep(Duration::from_millis(20));
                dispatch.dispatch(|| {
                    worker_ran.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .unwrap_err();
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn focus_timeout_after_dispatch_is_may_have_dispatched() {
        let ran = Arc::new(AtomicUsize::new(0));
        let worker_ran = Arc::clone(&ran);
        let thread = A11yThread::new("focus test", Duration::from_millis(100));
        let error = thread
            .focus(Deadline::from_millis(5), move |dispatch| {
                dispatch.dispatch(|| {
                    worker_ran.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                })
            })
            .unwrap_err();
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sequential_focus_dispatch_attempts_execute_only_one_mutation() {
        let ran = Arc::new(AtomicUsize::new(0));
        let worker_ran = Arc::clone(&ran);
        let thread = A11yThread::new("focus test", Duration::from_secs(1));
        thread
            .focus(Deadline::UNBOUNDED, move |dispatch| {
                dispatch.dispatch(|| {
                    worker_ran.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })?;
                assert!(
                    dispatch
                        .dispatch(|| {
                            worker_ran.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .is_err()
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_timed_out_pre_invoke_worker_cannot_dispatch_the_action_later() {
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_invoked = std::sync::Arc::clone(&invoked);

        let error = reader()
            .invoke(Deadline::from_millis(20), move |dispatch| {
                std::thread::sleep(Duration::from_millis(60));
                dispatch.dispatch(|| {
                    worker_invoked.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            })
            .expect_err("the caller stops during pre-invoke target resolution");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched),
            "{error}"
        );

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "the detached worker dispatched after the caller timed out"
        );
    }

    #[test]
    fn a_backend_timeout_cancels_an_unclaimed_invoke_action() {
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_invoked = std::sync::Arc::clone(&invoked);

        let error = impatient()
            .invoke(Deadline::UNBOUNDED, move |dispatch| {
                std::thread::sleep(Duration::from_millis(60));
                dispatch.dispatch(|| {
                    worker_invoked.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            })
            .expect_err("the backend ceiling stops during pre-invoke target resolution");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched),
            "{error}"
        );
        assert!(
            matches!(error.cause(), GlassError::AccessibilityUnavailable(_)),
            "{error}"
        );

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "the detached worker dispatched after the backend ceiling timed out"
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
            .invoke(Deadline::UNBOUNDED, |dispatch| dispatch.dispatch(hangs()))
            .unwrap_err();
        assert!(started.elapsed() >= IMPATIENT, "{:?}", started.elapsed());
        assert!(e.to_string().contains("invoke timed out"), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert!(e.to_string().contains("may still land"), "{e}");
    }
}
