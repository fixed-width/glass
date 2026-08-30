//! Pure deadline sequencing shared by the macOS input backend and host-runnable tests.

use glass_core::{BoundDispatch, Deadline, GlassError, Result, Whose};

pub(crate) fn require_payload_time(deadline: Deadline) -> Result<()> {
    if deadline.has_passed() {
        Err(GlassError::caller_deadline_elapsed("macOS input"))
    } else {
        Ok(())
    }
}

pub(crate) fn after_focus<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.bound_owner() == Some(Whose::Caller)
            && error.bound_dispatch() == Some(BoundDispatch::NotDispatched)
        {
            GlassError::caller_deadline_elapsed("macOS input")
        } else {
            error
        }
    })
}

pub(crate) fn run_scroll_wheel_by<T>(
    deadline: Deadline,
    move_cursor: impl FnOnce() -> Result<()>,
    build_wheel: impl FnOnce() -> Result<T>,
    post_wheel: impl FnOnce(T),
) -> Result<()> {
    move_cursor()?;
    require_payload_time(deadline)?;
    let wheel = build_wheel()?;
    require_payload_time(deadline)?;
    post_wheel(wheel);
    require_payload_time(deadline)
}

#[cfg(test)]
mod tests {
    use super::{after_focus, run_scroll_wheel_by};
    use glass_core::{
        BoundDispatch, BoundKind, ChordSink, Deadline, DragGesture, DragSink, Result, ScrollSink,
        TypeSink, Whose, run_chord_by, run_drag_by, run_scroll_by, run_type_by,
    };
    use std::cell::Cell;
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingSink {
        payload_calls: usize,
    }

    impl DragSink for RecordingSink {
        fn place(&mut self, _x: i32, _y: i32) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }

        fn move_to(&mut self, _x: i32, _y: i32) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }

        fn button(&mut self, _down: bool) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }

        fn modifiers(&mut self, _down: bool) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }
    }

    impl ScrollSink for RecordingSink {
        fn modifiers(&mut self, _down: bool) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }

        fn wheel(&mut self) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }
    }

    impl TypeSink for RecordingSink {
        fn character(&mut self, _character: char) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }
    }

    impl ChordSink for RecordingSink {
        fn modifiers(&mut self, _down: bool) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }

        fn key(&mut self, _down: bool) -> Result<()> {
            self.payload_calls += 1;
            Ok(())
        }
    }

    fn assert_post_focus_timeout(error: glass_core::GlassError) {
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn drag_expiring_after_focus_but_before_driver_start_reports_dispatched() {
        let mut sink = RecordingSink::default();
        let gesture = DragGesture::plan((0, 0), (1, 1), 0);
        let error =
            after_focus(run_drag_by(&mut sink, &gesture, Deadline::from_millis(0))).unwrap_err();

        assert_eq!(sink.payload_calls, 0);
        assert_post_focus_timeout(error);
    }

    #[test]
    fn scroll_expiring_after_focus_but_before_driver_start_reports_dispatched() {
        let mut sink = RecordingSink::default();
        let error =
            after_focus(run_scroll_by(&mut sink, false, Deadline::from_millis(0))).unwrap_err();

        assert_eq!(sink.payload_calls, 0);
        assert_post_focus_timeout(error);
    }

    #[test]
    fn text_expiring_after_focus_but_before_driver_start_reports_dispatched() {
        let mut sink = RecordingSink::default();
        let error = after_focus(run_type_by(
            &mut sink,
            "a",
            Duration::ZERO,
            Deadline::from_millis(0),
        ))
        .unwrap_err();

        assert_eq!(sink.payload_calls, 0);
        assert_post_focus_timeout(error);
    }

    #[test]
    fn chord_expiring_after_focus_but_before_driver_start_reports_dispatched() {
        let mut sink = RecordingSink::default();
        let error = after_focus(run_chord_by(&mut sink, Deadline::from_millis(0))).unwrap_err();

        assert_eq!(sink.payload_calls, 0);
        assert_post_focus_timeout(error);
    }

    #[test]
    fn scroll_expiry_after_cursor_move_prevents_wheel_construction_and_post() {
        let built = Cell::new(false);
        let posted = Cell::new(false);
        let error = run_scroll_wheel_by(
            Deadline::from_millis(1),
            || {
                std::thread::sleep(Duration::from_millis(10));
                Ok(())
            },
            || {
                built.set(true);
                Ok(())
            },
            |_| posted.set(true),
        )
        .unwrap_err();

        assert!(!built.get());
        assert!(!posted.get());
        assert_post_focus_timeout(error);
    }

    #[test]
    fn scroll_expiry_during_wheel_construction_prevents_wheel_post() {
        let posted = Cell::new(false);
        let error = run_scroll_wheel_by(
            Deadline::from_millis(1),
            || Ok(()),
            || {
                std::thread::sleep(Duration::from_millis(10));
                Ok(())
            },
            |_| posted.set(true),
        )
        .unwrap_err();

        assert!(!posted.get());
        assert_post_focus_timeout(error);
    }

    #[test]
    fn scroll_expiry_during_wheel_post_is_reported_after_dispatch() {
        let posted = Cell::new(false);
        let error = run_scroll_wheel_by(
            Deadline::from_millis(1),
            || Ok(()),
            || Ok(()),
            |_| {
                posted.set(true);
                std::thread::sleep(Duration::from_millis(10));
            },
        )
        .unwrap_err();

        assert!(posted.get());
        assert_post_focus_timeout(error);
    }

    #[test]
    fn post_focus_mapping_preserves_non_deadline_errors() {
        let error = after_focus(Err::<(), _>(glass_core::GlassError::InvalidKey(
            "bad".into(),
        )))
        .unwrap_err();

        assert!(matches!(error, glass_core::GlassError::InvalidKey(_)));
    }

    #[test]
    fn post_focus_mapping_preserves_caller_errors_that_already_report_dispatch() {
        let error = after_focus(Err::<(), _>(
            glass_core::GlassError::caller_deadline_elapsed("payload"),
        ))
        .unwrap_err();

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }
}
