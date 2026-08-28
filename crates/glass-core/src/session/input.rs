//! `Glass` input actuation: pointer and key events with bounds checks.
use super::*;

fn drag_fits_at(
    deadline: Deadline,
    required: std::time::Duration,
    now: std::time::Instant,
) -> bool {
    deadline
        .remaining_at(now)
        .is_none_or(|left| left >= required)
}

impl Glass {
    /// Validate that any window-relative coordinates in `event` fall inside the
    /// current window.
    fn check_bounds(&self, event: &PointerEvent) -> Result<()> {
        let g = self.require_active()?;
        let (w, h) = (g.geometry.width as i32, g.geometry.height as i32);
        let check = |x: i32, y: i32| -> Result<()> {
            if x < 0 || y < 0 || x >= w || y >= h {
                Err(GlassError::CoordOutOfBounds {
                    x,
                    y,
                    width: g.geometry.width,
                    height: g.geometry.height,
                })
            } else {
                Ok(())
            }
        };
        match *event {
            PointerEvent::Move { x, y } => check(x, y),
            PointerEvent::Click { x, y, .. } => check(x, y),
            PointerEvent::Scroll { x, y, .. } => check(x, y),
            PointerEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            } => {
                check(from_x, from_y)?;
                check(to_x, to_y)
            }
            PointerEvent::Gesture { ref pointers, .. } => {
                for p in pointers {
                    check(p.from_x, p.from_y)?;
                    check(p.to_x, p.to_y)?;
                }
                Ok(())
            }
        }
    }

    pub fn pointer(&mut self, event: &PointerEvent) -> Result<()> {
        self.pointer_by(event, Deadline::UNBOUNDED)
    }

    pub fn pointer_by(&mut self, event: &PointerEvent, deadline: Deadline) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.pointer_inner_by(event, deadline);
        self.emit_audit(
            &crate::audit::Actuation::Pointer { event },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    pub(super) fn pointer_inner_by(
        &mut self,
        event: &PointerEvent,
        deadline: Deadline,
    ) -> Result<()> {
        self.check_bounds(event)?;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("pointer input"));
        }
        if let PointerEvent::Drag { duration_ms, .. } = event {
            let required = std::time::Duration::from_millis(*duration_ms)
                .saturating_add(std::time::Duration::from_millis(48));
            if !drag_fits_at(deadline, required, std::time::Instant::now()) {
                return Err(GlassError::deadline_not_started("drag"));
            }
        }
        let s = self.active_mut()?;
        s.platform.send_pointer_by(event, deadline)?;
        s.pump();
        Ok(())
    }

    pub fn key(&mut self, event: &KeyEvent) -> Result<()> {
        self.key_by(event, Deadline::UNBOUNDED)
    }

    pub fn key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.key_inner_by(event, deadline);
        self.emit_audit(
            &crate::audit::Actuation::Key { event },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn key_inner_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        let s = self.active_mut()?;
        s.platform.send_key_by(event, deadline)?;
        s.pump();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pointer_by_spent_deadline_rejects_before_backend_and_audits_failure() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with(FakePlatform::new(10, 10).with_pointer_deadline_log(log.clone()));
        let audit = RecordingSink::default();
        let records = audit.0.clone();
        g.set_audit_sink(Box::new(audit));
        g.start(&spec()).unwrap();
        records.lock().unwrap().clear();
        let event = PointerEvent::Move { x: 1, y: 1 };
        assert!(g.pointer_by(&event, Deadline::from_millis(0)).is_err());
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(&*records.lock().unwrap(), &["move:false"]);
    }

    #[test]
    fn key_by_passes_the_exact_deadline_to_platform() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with(FakePlatform::new(10, 10).with_key_deadline_log(log.clone()));
        g.start(&spec()).unwrap();
        let deadline = Deadline::from_millis(1_000);
        g.key_by(&KeyEvent::Chord("enter".into()), deadline)
            .unwrap();
        assert_eq!(&*log.lock().unwrap(), &[deadline]);
    }

    #[test]
    fn drag_that_cannot_fit_is_refused_before_pointer_down() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with(FakePlatform::new(100, 100).with_drag_log(log.clone()));
        g.start(&spec()).unwrap();
        let event = PointerEvent::Drag {
            from_x: 1,
            from_y: 1,
            to_x: 50,
            to_y: 50,
            button: crate::platform::MouseButton::Left,
            modifiers: vec![],
            duration_ms: 200,
        };
        assert!(g.pointer_by(&event, Deadline::from_millis(247)).is_err());
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn drag_preflight_accepts_exactly_the_required_budget() {
        let now = std::time::Instant::now();
        let required = std::time::Duration::from_millis(248);
        let deadline = Deadline::at(now + required);
        assert!(super::drag_fits_at(deadline, required, now));
    }

    #[test]
    fn accepted_drag_passes_the_exact_deadline_to_platform() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with(FakePlatform::new(100, 100).with_pointer_deadline_log(log.clone()));
        g.start(&spec()).unwrap();
        let event = PointerEvent::Drag {
            from_x: 1,
            from_y: 1,
            to_x: 50,
            to_y: 50,
            button: crate::platform::MouseButton::Left,
            modifiers: vec![],
            duration_ms: 200,
        };
        let deadline = Deadline::from_millis(1_000);
        g.pointer_by(&event, deadline).unwrap();
        assert_eq!(&*log.lock().unwrap(), &[deadline]);
    }

    #[test]
    fn unbounded_pointer_and_key_keep_existing_behavior() {
        let pointer_log = Arc::new(Mutex::new(Vec::new()));
        let key_log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with(
            FakePlatform::new(10, 10)
                .with_scroll_log(pointer_log.clone())
                .with_key_log(key_log.clone()),
        );
        g.start(&spec()).unwrap();
        g.pointer(&PointerEvent::Scroll {
            x: 1,
            y: 1,
            dx: 0,
            dy: 1,
            modifiers: vec![],
        })
        .unwrap();
        g.key(&KeyEvent::Chord("enter".into())).unwrap();
        assert_eq!(pointer_log.lock().unwrap().len(), 1);
        assert_eq!(key_log.lock().unwrap().len(), 1);
    }

    #[test]
    fn pointer_out_of_bounds_is_rejected_before_backend() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        let err = g.pointer(&PointerEvent::Click {
            x: 10, // valid range is 0..=9
            y: 5,
            button: crate::platform::MouseButton::Left,
            count: 1,
            modifiers: vec![],
        });
        assert!(matches!(
            err.unwrap_err(),
            GlassError::CoordOutOfBounds { .. }
        ));
    }

    #[test]
    fn negative_pointer_coordinate_is_rejected() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        let err = g.pointer(&PointerEvent::Click {
            x: -1,
            y: 5,
            button: crate::platform::MouseButton::Left,
            count: 1,
            modifiers: vec![],
        });
        assert!(matches!(
            err.unwrap_err(),
            GlassError::CoordOutOfBounds { .. }
        ));
    }

    #[test]
    fn gesture_out_of_bounds_segment_is_rejected() {
        let mut g = glass_with(FakePlatform::new(100, 80));
        g.start(&spec()).unwrap();
        let ev = PointerEvent::Gesture {
            pointers: vec![
                Segment {
                    from_x: 10,
                    from_y: 10,
                    to_x: 20,
                    to_y: 20,
                },
                Segment {
                    from_x: 10,
                    from_y: 10,
                    to_x: 200,
                    to_y: 20,
                }, // to_x out of 100-wide window
            ],
            duration_ms: 100,
        };
        assert!(matches!(
            g.pointer(&ev),
            Err(GlassError::CoordOutOfBounds { .. })
        ));
    }
}
