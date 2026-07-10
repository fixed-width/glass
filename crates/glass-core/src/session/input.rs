//! `Glass` input actuation: pointer and key events with bounds checks.
use super::*;

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
        let t = std::time::Instant::now();
        let result = self.pointer_inner(event);
        self.emit_audit(
            &crate::audit::Actuation::Pointer { event },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    // pub(super): also used by the `a11y` submodule to actuate element clicks.
    pub(super) fn pointer_inner(&mut self, event: &PointerEvent) -> Result<()> {
        self.check_bounds(event)?;
        let s = self.active_mut()?;
        s.platform.send_pointer(event)?;
        s.pump();
        Ok(())
    }

    pub fn key(&mut self, event: &KeyEvent) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.key_inner(event);
        self.emit_audit(
            &crate::audit::Actuation::Key { event },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn key_inner(&mut self, event: &KeyEvent) -> Result<()> {
        let s = self.active_mut()?;
        s.platform.send_key(event)?;
        s.pump();
        Ok(())
    }
}
