//! `Glass` window ops: move/resize, list, and select.
use super::*;

impl Glass {
    pub fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let t = std::time::Instant::now();
        let result = self.window_inner(op);
        if !matches!(op, WindowOp::Geometry) {
            self.emit_audit(
                &crate::audit::Actuation::Window { op },
                crate::audit::AuditOutcome::from_result(&result),
                t.elapsed(),
            );
        }
        result
    }

    fn window_inner(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let s = self.active_mut()?;
        let geometry = s.platform.window(op)?;
        s.geometry = geometry.clone();
        s.pump();
        Ok(geometry)
    }

    /// All top-level windows of the active app.
    pub fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        self.active_mut()?.platform.list_windows()
    }

    /// Make `id` the active window; subsequent capture/input/window ops target
    /// it. Updates the cached active-window geometry.
    pub fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        let s = self.active_mut()?;
        let geometry = s.platform.select_window(id)?;
        s.geometry = geometry.clone();
        s.active_window = Some(crate::audit::WindowRef {
            id: id.0,
            title: None,
        });
        s.pump();
        Ok(geometry)
    }
}
