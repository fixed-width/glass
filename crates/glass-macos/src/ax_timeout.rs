//! Pure AX messaging-timeout orchestration shared by the macOS backend and host-runnable tests.

use glass_core::{Deadline, GlassError, Result};
use std::sync::{Condvar, Mutex};

static AX_MESSAGING_TIMEOUT_SCOPE: TimeoutScopeGate = TimeoutScopeGate::new();

struct TimeoutScopeGate {
    active: Mutex<bool>,
    available: Condvar,
}

impl TimeoutScopeGate {
    const fn new() -> Self {
        Self {
            active: Mutex::new(false),
            available: Condvar::new(),
        }
    }

    fn acquire(&'static self, deadline: Deadline) -> Result<TimeoutScopePermit> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started(
                    "macOS AX window operation",
                ));
            }
            if !*active {
                *active = true;
                return Ok(TimeoutScopePermit { gate: self });
            }

            active = match deadline.remaining() {
                None => self
                    .available
                    .wait(active)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                Some(remaining) => {
                    let (next, _) = self
                        .available
                        .wait_timeout(active, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    next
                }
            };
        }
    }
}

struct TimeoutScopePermit {
    gate: &'static TimeoutScopeGate,
}

impl Drop for TimeoutScopePermit {
    fn drop(&mut self) {
        let mut active = self
            .gate
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = false;
        self.gate.available.notify_one();
    }
}

pub(crate) trait AxMessaging {
    type Element;

    fn system_wide_element(&self) -> Self::Element;
    fn set_messaging_timeout(&self, element: &Self::Element, seconds: f32) -> Result<()>;
}

struct TimeoutRestore<'a, A: AxMessaging> {
    ax: &'a A,
    element: &'a A::Element,
}

impl<A: AxMessaging> Drop for TimeoutRestore<'_, A> {
    fn drop(&mut self) {
        let _ = self.ax.set_messaging_timeout(self.element, 0.0);
    }
}

pub(crate) fn with_messaging_timeout_by<A, T>(
    ax: &A,
    deadline: Deadline,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T>
where
    A: AxMessaging,
{
    let _serialized = AX_MESSAGING_TIMEOUT_SCOPE.acquire(deadline)?;
    let Some(remaining) = deadline.remaining() else {
        return operation();
    };
    if remaining.is_zero() {
        return Err(GlassError::deadline_not_started(
            "macOS AX window operation",
        ));
    }

    let element = ax.system_wide_element();
    ax.set_messaging_timeout(&element, remaining.as_secs_f32().max(0.001))?;
    let _restore = TimeoutRestore {
        ax,
        element: &element,
    };
    operation()
}

pub(crate) fn finish_window_operation_by<T>(deadline: Deadline, result: Result<T>) -> Result<T> {
    if deadline.has_passed() {
        let did_not_dispatch = matches!(
            result.as_ref(),
            Err(error)
                if error.bound_owner() == Some(glass_core::Whose::Caller)
                    && error.bound_dispatch() == Some(glass_core::BoundDispatch::NotDispatched)
        );
        if did_not_dispatch {
            result
        } else {
            Err(GlassError::caller_deadline_elapsed(
                "macOS window operation",
            ))
        }
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{AxMessaging, finish_window_operation_by, with_messaging_timeout_by};
    use glass_core::{BoundDispatch, BoundKind, Deadline, GlassError, Result, Whose};
    use std::collections::HashMap;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum Element {
        SystemWide,
        Application(u64),
        Window(u64),
    }

    #[derive(Default)]
    struct State {
        next_id: u64,
        global_timeout: Option<f32>,
        exact_timeouts: HashMap<Element, f32>,
        sets: Vec<(Element, f32)>,
    }

    #[derive(Default)]
    struct FakeAx {
        state: Mutex<State>,
    }

    impl FakeAx {
        fn fresh_element(&self, make: impl FnOnce(u64) -> Element) -> Element {
            let mut state = self.state.lock().unwrap();
            state.next_id += 1;
            make(state.next_id)
        }

        fn window_for(&self, _application: Element) -> Element {
            self.fresh_element(Element::Window)
        }

        fn application_element(&self, _pid: i32) -> Element {
            self.fresh_element(Element::Application)
        }

        fn effective_timeout(&self, element: Element) -> Option<f32> {
            let state = self.state.lock().unwrap();
            state
                .exact_timeouts
                .get(&element)
                .copied()
                .or(state.global_timeout)
        }

        fn active_timeout_count(&self) -> usize {
            let state = self.state.lock().unwrap();
            usize::from(state.global_timeout.is_some()) + state.exact_timeouts.len()
        }

        fn set_count(&self) -> usize {
            self.state.lock().unwrap().sets.len()
        }
    }

    impl AxMessaging for FakeAx {
        type Element = Element;

        fn system_wide_element(&self) -> Self::Element {
            Element::SystemWide
        }

        fn set_messaging_timeout(&self, element: &Self::Element, seconds: f32) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.sets.push((*element, seconds));
            match (*element, seconds == 0.0) {
                (Element::SystemWide, true) => state.global_timeout = None,
                (Element::SystemWide, false) => state.global_timeout = Some(seconds),
                (_, true) => {
                    state.exact_timeouts.remove(element);
                }
                (_, false) => {
                    state.exact_timeouts.insert(*element, seconds);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn exact_window_created_inside_scope_inherits_the_timeout() {
        let ax = FakeAx::default();

        let observed = with_messaging_timeout_by(&ax, Deadline::from_millis(1_000), || {
            let application = ax.application_element(42);
            let window = ax.window_for(application);
            Ok(ax.effective_timeout(window).is_some())
        })
        .unwrap();

        assert!(observed, "the exact window object had no effective timeout");
    }

    #[test]
    fn scoped_timeout_restores_after_operation_error() {
        let ax = FakeAx::default();

        let _ = with_messaging_timeout_by(&ax, Deadline::from_millis(1_000), || {
            Err::<(), _>(GlassError::Backend("operation failed".into()))
        });

        assert_eq!(ax.active_timeout_count(), 0);
    }

    #[test]
    fn scoped_timeout_restores_during_unwind() {
        let ax = FakeAx::default();

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                with_messaging_timeout_by(&ax, Deadline::from_millis(1_000), || -> Result<()> {
                    panic!("operation panic")
                });
        }));

        assert!(panic.is_err());
        assert_eq!(ax.active_timeout_count(), 0);
    }

    #[test]
    fn bounded_scopes_are_serialized_until_restoration() {
        let ax = Arc::new(FakeAx::default());
        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_ax = Arc::clone(&ax);
        let first = thread::spawn(move || {
            with_messaging_timeout_by(first_ax.as_ref(), Deadline::from_millis(2_000), || {
                first_entered_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        first_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let second_ax = Arc::clone(&ax);
        let second = thread::spawn(move || {
            with_messaging_timeout_by(second_ax.as_ref(), Deadline::from_millis(2_000), || {
                second_entered_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        });

        let entered_before_restore = second_entered_rx
            .recv_timeout(Duration::from_millis(100))
            .is_ok();
        release_first_tx.send(()).unwrap();
        first.join().unwrap();
        if !entered_before_restore {
            second_entered_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }
        second.join().unwrap();

        assert!(!entered_before_restore);
    }

    #[test]
    fn unbounded_operation_does_not_inherit_a_concurrent_scoped_timeout() {
        let ax = Arc::new(FakeAx::default());
        let (bounded_entered_tx, bounded_entered_rx) = mpsc::channel();
        let (release_bounded_tx, release_bounded_rx) = mpsc::channel();
        let bounded_ax = Arc::clone(&ax);
        let bounded = thread::spawn(move || {
            with_messaging_timeout_by(bounded_ax.as_ref(), Deadline::from_millis(2_000), || {
                bounded_entered_tx.send(()).unwrap();
                release_bounded_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        bounded_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let (unbounded_observation_tx, unbounded_observation_rx) = mpsc::channel();
        let unbounded_ax = Arc::clone(&ax);
        let unbounded = thread::spawn(move || {
            with_messaging_timeout_by(unbounded_ax.as_ref(), Deadline::UNBOUNDED, || {
                let application = unbounded_ax.application_element(43);
                let window = unbounded_ax.window_for(application);
                unbounded_observation_tx
                    .send(unbounded_ax.effective_timeout(window))
                    .unwrap();
                Ok(())
            })
            .unwrap();
        });

        let observed_before_restore = unbounded_observation_rx
            .recv_timeout(Duration::from_millis(100))
            .ok();
        release_bounded_tx.send(()).unwrap();
        bounded.join().unwrap();
        let observed = observed_before_restore.unwrap_or_else(|| {
            unbounded_observation_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
        });
        unbounded.join().unwrap();

        assert_eq!(observed, None);
    }

    #[test]
    fn bounded_scope_expires_while_waiting_for_an_unbounded_operation() {
        let ax = Arc::new(FakeAx::default());
        let (unbounded_entered_tx, unbounded_entered_rx) = mpsc::channel();
        let (release_unbounded_tx, release_unbounded_rx) = mpsc::channel();
        let unbounded_ax = Arc::clone(&ax);
        let unbounded = thread::spawn(move || {
            with_messaging_timeout_by(unbounded_ax.as_ref(), Deadline::UNBOUNDED, || {
                unbounded_entered_tx.send(()).unwrap();
                release_unbounded_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        unbounded_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let (result_tx, result_rx) = mpsc::channel();
        let bounded_ax = Arc::clone(&ax);
        let bounded = thread::spawn(move || {
            result_tx
                .send(with_messaging_timeout_by(
                    bounded_ax.as_ref(),
                    Deadline::from_millis(25),
                    || Ok(()),
                ))
                .unwrap();
        });

        let returned_before_release = result_rx.recv_timeout(Duration::from_millis(150)).ok();
        let did_return_before_release = returned_before_release.is_some();
        release_unbounded_tx.send(()).unwrap();
        unbounded.join().unwrap();
        let result = returned_before_release
            .unwrap_or_else(|| result_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        bounded.join().unwrap();
        let error = result.unwrap_err();

        assert!(did_return_before_release);
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn unbounded_operation_leaves_the_global_timeout_untouched() {
        let ax = FakeAx::default();

        with_messaging_timeout_by(&ax, Deadline::UNBOUNDED, || Ok(())).unwrap();

        assert_eq!(ax.set_count(), 0);
    }

    #[test]
    fn operation_error_preserves_caller_ownership_and_dispatch() {
        let ax = FakeAx::default();

        let error = with_messaging_timeout_by(&ax, Deadline::from_millis(1_000), || {
            Err::<(), _>(GlassError::caller_deadline_elapsed("exact AX window read"))
        })
        .unwrap_err();

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn finalization_preserves_a_caller_timeout_that_never_dispatched() {
        let error = finish_window_operation_by(
            Deadline::from_millis(0),
            Err::<(), _>(GlassError::deadline_not_started(
                "serialized AX window operation",
            )),
        )
        .unwrap_err();

        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn finalization_rejects_success_returned_after_the_caller_deadline() {
        let error = finish_window_operation_by(Deadline::from_millis(0), Ok(())).unwrap_err();

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }
}
