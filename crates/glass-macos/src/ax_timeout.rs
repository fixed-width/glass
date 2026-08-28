//! macOS window-operation AX timeout facade and whole-operation finalization.

#[cfg(target_os = "macos")]
use glass_a11y_macos::messaging_timeout::AX_MESSAGING_TIMEOUT_OWNER;
use glass_a11y_macos::messaging_timeout::{AxMessaging, MessageScope, TimeoutOwner};
use glass_core::{Deadline, GlassError, Result};

/// Enter the AX owner after a successful ScreenCaptureKit window query.
///
/// The query is external work, so expiry while waiting for the owner is a dispatched caller
/// timeout, never `NotStarted`/`NotDispatched`.
pub(crate) fn with_window_query_on<A, Q, T>(
    owner: &TimeoutOwner,
    ax: &A,
    deadline: Deadline,
    query: impl FnOnce() -> Result<Q>,
    operation: impl FnOnce(Q, &mut MessageScope<'_, '_, A>) -> Result<T>,
) -> Result<T>
where
    A: AxMessaging,
{
    let query_result = query()?;
    owner.with_deadline_by(ax, deadline, true, |scope| operation(query_result, scope))
}

#[cfg(target_os = "macos")]
pub(crate) fn with_window_query_by<A, Q, T>(
    ax: &A,
    deadline: Deadline,
    query: impl FnOnce() -> Result<Q>,
    operation: impl FnOnce(Q, &mut MessageScope<'_, '_, A>) -> Result<T>,
) -> Result<T>
where
    A: AxMessaging,
{
    with_window_query_on(&AX_MESSAGING_TIMEOUT_OWNER, ax, deadline, query, operation)
}

pub(crate) fn finish_window_operation_by<T>(deadline: Deadline, result: Result<T>) -> Result<T> {
    if result
        .as_ref()
        .is_err_and(glass_a11y_macos::messaging_timeout::is_contamination_error)
    {
        return result;
    }
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
    use super::{finish_window_operation_by, with_window_query_on};
    use glass_a11y_macos::messaging_timeout::{AxMessaging, TimeoutOwner};
    use glass_core::{BoundDispatch, BoundKind, Deadline, GlassError, Result, Whose};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    struct FakeAx;

    impl AxMessaging for FakeAx {
        type Element = ();

        fn system_wide_element(&self) -> Self::Element {}

        fn set_messaging_timeout(&self, _element: &Self::Element, _seconds: f32) -> Result<()> {
            Ok(())
        }
    }

    struct FailingRestoreAx;

    impl AxMessaging for FailingRestoreAx {
        type Element = ();

        fn system_wide_element(&self) -> Self::Element {}

        fn set_messaging_timeout(&self, _element: &Self::Element, seconds: f32) -> Result<()> {
            if seconds == 0.0 {
                Err(GlassError::Backend("synthetic restore failure".into()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn successful_screen_capture_query_makes_later_gate_expiry_dispatched() {
        let owner = Arc::new(TimeoutOwner::new());
        let ax = Arc::new(FakeAx);
        let (blocker_entered_tx, blocker_entered_rx) = mpsc::channel();
        let (release_blocker_tx, release_blocker_rx) = mpsc::channel();
        let blocker_owner = Arc::clone(&owner);
        let blocker_ax = Arc::clone(&ax);
        let blocker = thread::spawn(move || {
            blocker_owner
                .with_doctor_timeout_by(blocker_ax.as_ref(), |scope| {
                    scope.message("blocking doctor probe", || {
                        blocker_entered_tx.send(()).unwrap();
                        release_blocker_rx.recv().unwrap();
                        Ok(())
                    })
                })
                .unwrap();
        });
        blocker_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let query_ran = Mutex::new(false);
        let ax_operation_ran = Mutex::new(false);
        let error = with_window_query_on(
            owner.as_ref(),
            ax.as_ref(),
            Deadline::from_millis(25),
            || {
                *query_ran.lock().unwrap() = true;
                Ok(())
            },
            |(), scope| {
                scope.message("window AX read", || {
                    *ax_operation_ran.lock().unwrap() = true;
                    Ok(())
                })
            },
        )
        .unwrap_err();

        assert!(*query_ran.lock().unwrap());
        assert!(!*ax_operation_ran.lock().unwrap());
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        release_blocker_tx.send(()).unwrap();
        blocker.join().unwrap();
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

    #[test]
    fn finalization_does_not_hide_a_process_global_restore_failure() {
        let owner = TimeoutOwner::new();
        let restore = owner
            .with_deadline_by(
                &FailingRestoreAx,
                Deadline::from_millis(1_000),
                false,
                |scope| scope.message("window AX read", || Ok(())),
            )
            .unwrap_err();

        let error = finish_window_operation_by(Deadline::from_millis(0), Err::<(), _>(restore))
            .unwrap_err();

        assert!(error.to_string().contains("contaminated"), "{error}");
        assert!(error.to_string().contains("restart"), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }
}
