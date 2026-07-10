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
