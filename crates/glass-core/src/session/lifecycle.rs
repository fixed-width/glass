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
            a11y_limits: WalkLimits::DEFAULT,
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
    /// `deadline` is when teardown is expected to be done. Stopping the sessions is held
    /// [`crate::TEARDOWN_HOOK_RESERVE`] short of it: a shared deadline bounds a sequence without
    /// dividing one, so a device that stops answering during `stop_app` would otherwise leave the
    /// hook with nothing, and a step with no time left is not run at all (glass#422).
    ///
    /// A third step between them is bounded by neither — dropping the session reaps the backend
    /// (Xvfb, sway, a Job object) and the log-stream children, each an unbounded `wait()`.
    ///
    /// Written to drain the session set so the future multi-session registry (a
    /// `HashMap` instead of this `Option`) reuses it unchanged — it becomes a `for`
    /// loop with no other change.
    pub fn shutdown(&mut self, deadline: Deadline) {
        if let Some(mut s) = self.active.take() {
            let _ = s
                .platform
                .stop_app_by(deadline.reserving(crate::TEARDOWN_HOOK_RESERVE));
            // `s` drops here: the backend (Xvfb/sway/Job) is torn down.
        }
        if let Some(hook) = self.shutdown_hook.take() {
            hook(deadline);
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

    /// A deadline far enough out that nothing in these tests is bounded by it — they are about
    /// what `shutdown` calls, not about what a spent budget does.
    fn soon() -> Deadline {
        Deadline::at(std::time::Instant::now() + crate::TEARDOWN_BUDGET)
    }

    #[test]
    fn shutdown_runs_the_hook() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let mut g =
            glass_with_factory(Box::new(|_b| Err(GlassError::Backend("no backend".into()))));
        g.set_shutdown_hook(Box::new(move |_| f.store(true, Ordering::SeqCst)));
        g.shutdown(soon());
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
        g.shutdown(soon());
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
        g.shutdown(soon());
        assert_eq!(
            *stops.lock().unwrap(),
            1,
            "no extra stop_app on an empty shutdown"
        );
    }

    /// The budget is only useful if it reaches the code that spends it: a backend that tears down
    /// over a link, and a hook that does the same for the host's own resources.
    #[test]
    fn shutdown_hands_its_deadline_to_both_the_backend_and_the_hook() {
        let at_stop = Arc::new(Mutex::new(None));
        let recorded = at_stop.clone();
        let factory: PlatformFactory = Box::new(move |_backend| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(10, 10).recording_stop_deadline(recorded.clone()),
            )))
        });
        let at_hook = Arc::new(Mutex::new(None));
        let hooked = at_hook.clone();
        let mut g = glass_with_factory(factory);
        g.set_shutdown_hook(Box::new(move |d| *hooked.lock().unwrap() = Some(d)));
        g.start(&spec()).unwrap();

        let deadline = soon();
        g.shutdown(deadline);

        let at_stop = at_stop
            .lock()
            .unwrap()
            .expect("the backend must be stopped through `stop_app_by`, not `stop_app`");
        // Each `remaining()` reads its own now, microseconds apart, so comparing them compares
        // the instants they hold.
        assert!(
            at_stop.remaining().expect("a bounded share") < deadline.remaining().unwrap(),
            "the session's share must stop short of the hook's, or the reserve is not held back"
        );
        assert_eq!(
            *at_hook.lock().unwrap(),
            Some(deadline),
            "the hook gets the whole deadline; the reserve is what the session gave up"
        );
    }

    /// The other half of the split: the reserve is a fixed 750ms, so without the clamp a caller
    /// whose whole budget is smaller hands the sessions a deadline already in the past.
    #[test]
    fn a_budget_smaller_than_the_reserve_still_leaves_the_sessions_time() {
        let at_stop = Arc::new(Mutex::new(None));
        let recorded = at_stop.clone();
        let factory: PlatformFactory = Box::new(move |_backend| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(10, 10).recording_stop_deadline(recorded.clone()),
            )))
        });
        let mut g = glass_with_factory(factory);
        g.start(&spec()).unwrap();

        // A fifth of the reserve, so an unclamped subtraction lands well in the past.
        let budget = crate::TEARDOWN_HOOK_RESERVE / 5;
        let started = std::time::Instant::now();
        g.shutdown(Deadline::at(started + budget));

        // Measured from `started`, not from now: `remaining()` here would subtract whatever the
        // test itself has since spent, and the margin is only tens of milliseconds.
        let given = at_stop
            .lock()
            .unwrap()
            .expect("the backend was stopped")
            .within(std::time::Duration::MAX, started);
        assert!(
            given >= budget / 3,
            "the sessions were given {given:?} of a {budget:?} budget — the reserve was taken \
             whole from a budget too small to pay it"
        );
    }

    /// The property this whole split exists for (glass#422): a session that spends everything it is
    /// given must still leave the hook enough to run. Without the reserve the hook is reached with
    /// a spent deadline, and `run_bounded_until` does not start a command it has no time for — so
    /// every step behind it is skipped rather than merely slow.
    #[test]
    fn a_session_that_burns_its_deadline_still_leaves_the_hook_time_to_run() {
        let factory: PlatformFactory = Box::new(move |_backend| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(10, 10).burning_its_deadline(),
            )))
        });
        let left = Arc::new(Mutex::new(None));
        let recorded = left.clone();
        let mut g = glass_with_factory(factory);
        g.set_shutdown_hook(Box::new(move |d| {
            *recorded.lock().unwrap() = d.remaining();
        }));
        g.start(&spec()).unwrap();

        // A tenth of the real budget, so the test costs what it measures rather than 3s.
        let budget = crate::TEARDOWN_BUDGET / 10;
        g.shutdown(Deadline::at(std::time::Instant::now() + budget));

        let left = left.lock().unwrap().expect("the hook ran at all");
        assert!(
            left > std::time::Duration::ZERO,
            "the hook was handed a spent deadline, so every step behind the session is skipped"
        );
    }

    #[test]
    fn shutdown_without_active_session_is_noop() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.shutdown(soon()); // must not panic and must not error
    }
}
