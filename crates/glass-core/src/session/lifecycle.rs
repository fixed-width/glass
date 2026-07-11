//! `Glass` session lifecycle: start/stop/shutdown and geometry.
use super::*;

impl Glass {
    /// Start with the default backend.
    pub fn start(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        let backend = self.default_backend.clone();
        self.start_on(&backend, spec)
    }

    /// Start with an explicit backend, constructing it via the factory.
    pub fn start_on(&mut self, backend: &str, spec: &AppSpec) -> Result<WindowGeometry> {
        let t = std::time::Instant::now();
        let result = self.start_on_inner(backend, spec);
        self.emit_audit(
            &crate::audit::Actuation::Launch { spec, backend },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn start_on_inner(&mut self, backend: &str, spec: &AppSpec) -> Result<WindowGeometry> {
        // One active session: tear down any current one first.
        if let Some(mut s) = self.active.take() {
            let _ = s.platform.stop_app();
        }
        let Backend {
            mut platform,
            accessibility,
        } = (self.factory)(backend)?;
        let geometry = platform.start_app(spec)?;
        let mut session = ActiveSession {
            platform,
            accessibility,
            last_ax: None,
            geometry: geometry.clone(),
            logs: LogBuffer::new(self.log_capacity),
            active_window: None,
        };
        session.pump();
        session.active_window = session
            .platform
            .list_windows()
            .ok()
            .and_then(|ws| ws.iter().find(|w| w.active).or_else(|| ws.first()).cloned())
            .map(|w| crate::audit::WindowRef {
                id: w.id.0,
                title: w.title,
            });
        self.active = Some(session);
        Ok(geometry)
    }

    pub fn stop(&mut self) -> Result<()> {
        let t = std::time::Instant::now();
        // Snapshot the window BEFORE stop_inner, which drops self.active — so this
        // records on the dedicated path rather than emit_audit (which would see None
        // after teardown). Keep this ordering if refactoring, or window attribution breaks.
        let window = self.active.as_ref().and_then(|s| s.active_window.clone());
        let result = self.stop_inner();
        if let Some(sink) = &self.audit {
            sink.record(
                &crate::audit::Actuation::Stop,
                &crate::audit::ActuationContext { window },
                &crate::audit::AuditOutcome::from_result(&result),
                t.elapsed(),
            );
        }
        result
    }

    fn stop_inner(&mut self) -> Result<()> {
        let mut s = self.active.take().ok_or(GlassError::NoActiveSession)?;
        s.platform.stop_app()
        // `s` drops here, tearing down the spawned backend (Xvfb/sway).
    }

    /// Best-effort teardown of **all** active sessions for process exit. Idempotent:
    /// a no-op when nothing is active. Errors are swallowed — we are exiting, so a
    /// failed `stop_app` must not prevent releasing the rest (the OS reaps anything
    /// left). Distinct from `stop()`, which reports errors to a tool caller.
    ///
    /// Written to drain the session set so the future multi-session registry (a
    /// `HashMap` instead of this `Option`) reuses it unchanged — it becomes a `for`
    /// loop with no other change.
    pub fn shutdown(&mut self) {
        if let Some(mut s) = self.active.take() {
            let _ = s.platform.stop_app();
            // `s` drops here: the backend (Xvfb/sway/Job) is torn down.
        }
        if let Some(hook) = self.shutdown_hook.take() {
            hook();
        }
    }

    pub fn geometry(&self) -> Result<WindowGeometry> {
        Ok(self.require_active()?.geometry.clone())
    }
}

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;

    #[test]
    fn operations_require_an_active_session() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        assert!(matches!(
            g.screenshot(None, None).unwrap_err(),
            GlassError::NoActiveSession
        ));
        assert!(matches!(g.stop().unwrap_err(), GlassError::NoActiveSession));
        assert!(matches!(
            g.key(&KeyEvent::Chord("ctrl+s".into())).unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    #[test]
    fn start_sets_geometry_and_buffers_initial_logs() {
        let platform = FakePlatform::new(80, 60).with_logs(vec![(Stream::Stdout, "ready")]);
        let mut g = glass_with(platform);
        let geom = g.start(&spec()).unwrap();
        assert_eq!(
            geom,
            WindowGeometry {
                x: 0,
                y: 0,
                width: 80,
                height: 60
            }
        );
        let (lines, _) = g.logs(0, 10, None, None).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "ready");
    }

    #[test]
    fn shutdown_runs_the_hook() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let mut g =
            glass_with_factory(Box::new(|_b| Err(GlassError::Backend("no backend".into()))));
        g.set_shutdown_hook(Box::new(move || f.store(true, Ordering::SeqCst)));
        g.shutdown();
        assert!(
            fired.load(Ordering::SeqCst),
            "shutdown should invoke the hook"
        );
    }

    #[test]
    fn start_on_passes_backend_name_to_factory() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let factory: PlatformFactory = Box::new(move |backend| {
            seen2.lock().unwrap().push(backend.to_string());
            Ok(Backend::display_only(Box::new(FakePlatform::new(10, 10))))
        });
        let mut g = glass_with_factory(factory);
        g.start(&spec()).unwrap(); // default ("x11")
        g.start_on("wayland", &spec()).unwrap(); // explicit
        assert_eq!(*seen.lock().unwrap(), vec!["x11", "wayland"]);
    }

    #[test]
    fn second_start_stops_the_first_backend() {
        let stops = Arc::new(Mutex::new(0u32));
        let stops2 = stops.clone();
        let factory: PlatformFactory = Box::new(move |_backend| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(10, 10).counting_stops(stops2.clone()),
            )))
        });
        let mut g = glass_with_factory(factory);
        g.start(&spec()).unwrap();
        g.start(&spec()).unwrap(); // should stop the first backend
        assert_eq!(*stops.lock().unwrap(), 1);
    }

    #[test]
    fn shutdown_stops_active_session_and_is_idempotent() {
        let stops = Arc::new(Mutex::new(0u32));
        let stops2 = stops.clone();
        let factory: PlatformFactory = Box::new(move |_backend| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(10, 10).counting_stops(stops2.clone()),
            )))
        });
        let mut g = glass_with_factory(factory);
        g.start(&spec()).unwrap();
        g.shutdown();
        assert_eq!(
            *stops.lock().unwrap(),
            1,
            "shutdown calls stop_app exactly once"
        );
        assert!(
            matches!(g.stop().unwrap_err(), GlassError::NoActiveSession),
            "the session is cleared after shutdown"
        );
        // Idempotent: a second shutdown with nothing active is a harmless no-op.
        g.shutdown();
        assert_eq!(
            *stops.lock().unwrap(),
            1,
            "no extra stop_app on an empty shutdown"
        );
    }

    #[test]
    fn shutdown_without_active_session_is_noop() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.shutdown(); // must not panic and must not error
    }
}
