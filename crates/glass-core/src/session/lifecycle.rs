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
        let protection_mode =
            platform.configure_protected_host_paths(&self.protected_host_paths)?;
        let geometry = platform.start_app(spec)?;
        let host_path_access = match (protection_mode, spec.sandbox) {
            (HostPathProtectionMode::SeparateFilesystem, _) => {
                HostPathAccess::HostFilesystemUnreachable
            }
            (HostPathProtectionMode::SandboxRules, SandboxLevel::Off) => {
                HostPathAccess::NotGuaranteedSandboxOff
            }
            (
                HostPathProtectionMode::SandboxRules,
                SandboxLevel::Default | SandboxLevel::Strict,
            ) => HostPathAccess::DeniedBySandbox,
        };
        let mut session = ActiveSession {
            platform,
            accessibility,
            last_ax: None,
            a11y_limits: WalkLimits::DEFAULT,
            geometry: geometry.clone(),
            logs: LogBuffer::new(self.log_capacity),
            active_window: None,
            host_path_access,
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
    /// a no-op when nothing is active. Errors are swallowed — we are exiting, so a failed
    /// `stop_app` must not prevent releasing the rest. Distinct from `stop()`, which reports
    /// errors to a tool caller.
    ///
    /// `deadline` is when teardown is expected to be done. Stopping the sessions is held
    /// [`crate::TEARDOWN_HOOK_RESERVE`] short of it: a shared deadline bounds a sequence without
    /// dividing one, so a device that stops answering during `stop_app` would otherwise leave the
    /// hook with nothing, and a step with no time left is not run at all (glass#422).
    ///
    /// A third step between them is bounded by neither — dropping the session reaps the backend
    /// (Xvfb, sway, a Job object) and the log-stream children, each an unbounded `wait()`.
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
    use crate::ProtectedHostPathKind;
    use crate::session::test_support::*;
    use std::path::PathBuf;

    struct UnawarePlatform {
        starts: Arc<AtomicUsize>,
    }

    impl Platform for UnawarePlatform {
        fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(WindowGeometry {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            })
        }
        fn stop_app_by(&mut self, _deadline: Deadline) -> Result<()> {
            Ok(())
        }
        fn capture_frame_by(
            &mut self,
            _region: Option<&Region>,
            _deadline: Deadline,
        ) -> Result<Frame> {
            unimplemented!()
        }
        fn capture_window_by(
            &mut self,
            _id: WindowId,
            _region: Option<&Region>,
            _deadline: Deadline,
        ) -> Result<Frame> {
            unimplemented!()
        }
        fn send_pointer_by(&mut self, _event: &PointerEvent, _deadline: Deadline) -> Result<()> {
            unimplemented!()
        }
        fn send_key_by(&mut self, _event: &KeyEvent, _deadline: Deadline) -> Result<()> {
            unimplemented!()
        }
        fn window_by(&mut self, _op: &WindowOp, _deadline: Deadline) -> Result<WindowGeometry> {
            unimplemented!()
        }
        fn list_windows_by(&mut self, _deadline: Deadline) -> Result<Vec<WindowInfo>> {
            Ok(Vec::new())
        }
        fn select_window_by(
            &mut self,
            _id: WindowId,
            _deadline: Deadline,
        ) -> Result<WindowGeometry> {
            unimplemented!()
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            Vec::new()
        }
    }

    fn glass_with_unaware(starts: Arc<AtomicUsize>) -> Glass {
        glass_with_factory(Box::new(move |_| {
            Ok(Backend::display_only(Box::new(UnawarePlatform {
                starts: starts.clone(),
            })))
        }))
    }

    #[test]
    fn protected_host_path_constructors_preserve_paths_and_kinds() {
        let directory_path = PathBuf::from("relative/../directory");
        let file_path = PathBuf::from("file-with-non-normalized/../name");

        assert_eq!(
            ProtectedHostPath::directory(directory_path.clone()),
            ProtectedHostPath {
                path: directory_path,
                kind: ProtectedHostPathKind::Directory
            }
        );
        assert_eq!(
            ProtectedHostPath::file(file_path.clone()),
            ProtectedHostPath {
                path: file_path,
                kind: ProtectedHostPathKind::File
            }
        );
    }

    #[test]
    fn nonempty_protected_host_paths_fail_closed_for_unaware_backend() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut g = glass_with_unaware(starts.clone());
        g.set_protected_host_paths(vec![ProtectedHostPath::directory("secret")])
            .unwrap();

        let error = g.start(&spec()).unwrap_err();
        assert!(matches!(&error, GlassError::SandboxUnavailable(_)));
        assert!(!error.to_string().contains("secret"));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);
    }

    #[test]
    fn empty_protected_host_paths_remain_compatible_with_unaware_backend() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut g = glass_with_unaware(starts.clone());

        g.start(&spec()).unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn host_path_access_for_sandbox_rules_follows_successful_launch_sandbox_level() {
        for (sandbox, expected) in [
            (SandboxLevel::Default, HostPathAccess::DeniedBySandbox),
            (SandboxLevel::Strict, HostPathAccess::DeniedBySandbox),
            (SandboxLevel::Off, HostPathAccess::NotGuaranteedSandboxOff),
        ] {
            let mut launch = spec();
            launch.sandbox = sandbox;
            let mut g = glass_with(FakePlatform::new(10, 10));
            assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);
            g.start(&launch).unwrap();
            assert_eq!(g.host_path_access(), expected);
        }
    }

    #[test]
    fn host_path_access_for_separate_filesystem_is_unreachable_at_every_sandbox_level() {
        for sandbox in [
            SandboxLevel::Default,
            SandboxLevel::Strict,
            SandboxLevel::Off,
        ] {
            let mut launch = spec();
            launch.sandbox = sandbox;
            let mut g = glass_with(
                FakePlatform::new(10, 10)
                    .with_protection_mode(HostPathProtectionMode::SeparateFilesystem),
            );
            g.start(&launch).unwrap();
            assert_eq!(
                g.host_path_access(),
                HostPathAccess::HostFilesystemUnreachable
            );
        }
    }

    #[test]
    fn protected_host_paths_cannot_change_during_active_session() {
        let stops = Arc::new(Mutex::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let configured = Arc::new(Mutex::new(Vec::new()));
        let factory_stops = stops.clone();
        let factory_log = log.clone();
        let factory_configured = configured.clone();
        let factory: PlatformFactory = Box::new(move |_| {
            Ok(Backend::display_only(Box::new(
                FakePlatform::new(10, 10)
                    .counting_stops(factory_stops.clone())
                    .with_lifecycle_log(factory_log.clone())
                    .with_protected_paths_log(factory_configured.clone()),
            )))
        });
        let mut g = glass_with_factory(factory);
        let original = vec![ProtectedHostPath::directory("original")];
        g.set_protected_host_paths(original.clone()).unwrap();
        g.start(&spec()).unwrap();

        assert!(matches!(
            g.set_protected_host_paths(vec![ProtectedHostPath::file("replacement")])
                .unwrap_err(),
            GlassError::ProtectedPathsWhileActive
        ));
        assert_eq!(*stops.lock().unwrap(), 0);
        assert_eq!(*log.lock().unwrap(), vec!["configure", "start"]);

        g.stop().unwrap();
        g.start(&spec()).unwrap();
        assert_eq!(
            *configured.lock().unwrap(),
            vec![original.clone(), original]
        );
    }

    #[test]
    fn host_path_access_resets_after_stop_and_failed_stop() {
        let mut stopped = glass_with(FakePlatform::new(10, 10));
        stopped.start(&spec()).unwrap();
        stopped.stop().unwrap();
        assert_eq!(stopped.host_path_access(), HostPathAccess::NoActiveTarget);

        let mut failed = glass_with(FakePlatform::new(10, 10).failing_stop());
        failed.start(&spec()).unwrap();
        assert!(failed.stop().is_err());
        assert_eq!(failed.host_path_access(), HostPathAccess::NoActiveTarget);
    }

    #[test]
    fn host_path_access_resets_after_shutdown() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        g.shutdown(soon());
        assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);
    }

    #[test]
    fn host_path_access_resets_after_failed_configure_and_start_and_allows_reconfiguration() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_factory = calls.clone();
        let factory: PlatformFactory = Box::new(move |_| {
            let call = calls_for_factory.fetch_add(1, Ordering::SeqCst);
            let platform = match call {
                0 => FakePlatform::new(10, 10),
                1 => FakePlatform::new(10, 10).failing_protected_path_configuration(),
                2 => FakePlatform::new(10, 10).failing_start(),
                _ => FakePlatform::new(10, 10),
            };
            Ok(Backend::display_only(Box::new(platform)))
        });
        let mut g = glass_with_factory(factory);
        g.start(&spec()).unwrap();
        assert_eq!(
            g.host_path_access(),
            HostPathAccess::NotGuaranteedSandboxOff
        );

        g.set_protected_host_paths(vec![ProtectedHostPath::directory("first")])
            .unwrap_err();
        assert!(g.start(&spec()).is_err());
        assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);
        g.set_protected_host_paths(vec![ProtectedHostPath::file("second")])
            .unwrap();
        assert!(g.start(&spec()).is_err());
        assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);
        g.set_protected_host_paths(vec![ProtectedHostPath::directory("third")])
            .unwrap();
        g.start(&spec()).unwrap();
        assert_eq!(
            g.host_path_access(),
            HostPathAccess::NotGuaranteedSandboxOff
        );
    }

    #[test]
    fn protected_host_path_configuration_runs_before_every_start_and_failure_prevents_launch() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_log = log.clone();
        let factory_calls = calls.clone();
        let factory: PlatformFactory = Box::new(move |_| {
            let call = factory_calls.fetch_add(1, Ordering::SeqCst);
            let platform = FakePlatform::new(10, 10).with_lifecycle_log(factory_log.clone());
            let platform = if call == 1 {
                platform.failing_protected_path_configuration()
            } else {
                platform
            };
            Ok(Backend::display_only(Box::new(platform)))
        });
        let mut g = glass_with_factory(factory);
        g.start(&spec()).unwrap();
        assert!(g.start(&spec()).is_err());
        assert_eq!(
            *log.lock().unwrap(),
            vec!["configure", "start", "configure"]
        );
        assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);
    }

    #[test]
    fn host_path_access_resets_after_replacement_factory_failure_and_new_paths_reach_restart() {
        let stops = Arc::new(Mutex::new(0u32));
        let configured = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_stops = stops.clone();
        let factory_configured = configured.clone();
        let factory_calls = calls.clone();
        let factory: PlatformFactory =
            Box::new(
                move |_| match factory_calls.fetch_add(1, Ordering::SeqCst) {
                    1 => Err(GlassError::Backend("scripted factory failure".into())),
                    _ => Ok(Backend::display_only(Box::new(
                        FakePlatform::new(10, 10)
                            .counting_stops(factory_stops.clone())
                            .with_protected_paths_log(factory_configured.clone()),
                    ))),
                },
            );
        let mut g = glass_with_factory(factory);
        let old_paths = vec![ProtectedHostPath::directory("old")];
        let new_paths = vec![ProtectedHostPath::file("new")];
        g.set_protected_host_paths(old_paths.clone()).unwrap();
        g.start(&spec()).unwrap();

        assert!(matches!(
            g.start(&spec()).unwrap_err(),
            GlassError::Backend(message) if message == "scripted factory failure"
        ));
        assert_eq!(*stops.lock().unwrap(), 1);
        assert_eq!(g.host_path_access(), HostPathAccess::NoActiveTarget);

        g.set_protected_host_paths(new_paths.clone()).unwrap();
        g.start(&spec()).unwrap();

        assert_eq!(*configured.lock().unwrap(), vec![old_paths, new_paths]);
        assert_eq!(
            g.host_path_access(),
            HostPathAccess::NotGuaranteedSandboxOff
        );
    }

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

    /// A deadline far enough out that nothing in these tests is bounded by it.
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

    /// The budget is only useful if it reaches the code that spends it.
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

    /// The reserve is a fixed 750ms, so without the clamp a caller whose whole budget is smaller
    /// hands the sessions a deadline already in the past.
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
            .remaining_at(started)
            .expect("a bounded share");
        assert!(
            given >= budget / 3,
            "the sessions were given {given:?} of a {budget:?} budget — the reserve was taken \
             whole from a budget too small to pay it"
        );
    }

    /// The property this whole split exists for (glass#422): a session that spends everything it is
    /// given must still leave the hook enough to run.
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
