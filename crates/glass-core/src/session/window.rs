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

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;

    #[test]
    fn window_resize_updates_tracked_geometry() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        let geom = g
            .window(&WindowOp::Resize {
                width: 20,
                height: 30,
            })
            .unwrap();
        assert_eq!(geom.width, 20);
        assert_eq!(geom.height, 30);
        assert_eq!(g.geometry().unwrap().width, 20);
    }

    #[test]
    fn select_window_switches_active_geometry() {
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 320,
                height: 240,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 400,
                y: 0,
                width: 100,
                height: 80,
            },
            active: false,
        };
        let mut glass = glass_with(FakePlatform::new(320, 240).with_windows(vec![a, b]));
        glass.start(&spec()).unwrap();

        let listed = glass.list_windows().unwrap();
        assert_eq!(listed.len(), 2);

        let geo = glass.select_window(WindowId(2)).unwrap();
        assert_eq!((geo.width, geo.height), (100, 80));
        assert_eq!(glass.geometry().unwrap().width, 100);

        assert!(matches!(
            glass.select_window(WindowId(999)),
            Err(GlassError::WindowNotFound)
        ));
    }
}
