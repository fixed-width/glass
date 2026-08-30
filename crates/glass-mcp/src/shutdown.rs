//! Process-exit teardown: route every graceful shutdown through one bounded,
//! best-effort `Glass::shutdown()`, plus a cross-platform termination signal.

use std::sync::Arc;
use std::time::{Duration, Instant};

use glass_core::{Deadline, Glass};
use tokio::sync::Mutex;

/// How long a tool call may hold the session before teardown says so.
///
/// The lock is held for a whole tool body and some legitimately outlast the budget — `am start`
/// gets 60s — which is a different failure from a step that ran and overran, and reads the same
/// without a line of its own.
const LOCK_WAIT_WORTH_SAYING: Duration = Duration::from_millis(250);

/// Best-effort, time-bounded teardown of all sessions for process exit. The backend
/// teardown blocks (it waits on the child), so it runs off the async reactor via
/// `spawn_blocking`; after `budget` we stop waiting and let the OS reap whatever is
/// left — we are exiting regardless.
///
/// Two bounds, for two failures. `Glass::shutdown`'s deadline is what its steps spend, held
/// [`glass_core::TEARDOWN_REAP_HEADROOM`] short of `budget` so killing a wedged step — up to
/// `bounded::KILL_REAP` past its deadline — still lands inside. This `timeout` is the backstop for
/// a step that ignores the deadline: a `spawn_blocking` task cannot be cancelled, so that step is
/// still running when the process exits, and its child is orphaned.
pub async fn run_shutdown(sessions: Arc<Mutex<Glass>>, budget: Duration) {
    let task = tokio::task::spawn_blocking(move || {
        // Started after the lock: a tool call in flight holds the session, and a deadline
        // measured across that wait arrives spent — reported downstream as every step skipped,
        // when what happened is that teardown never got to start.
        let waiting = Instant::now();
        // On a `spawn_blocking` thread, `blocking_lock` is allowed (it would panic on
        // a reactor worker thread).
        let mut glass = sessions.blocking_lock();
        let waited = waiting.elapsed();
        if waited > LOCK_WAIT_WORTH_SAYING {
            eprintln!("glass: a tool call held the session for {waited:?} before teardown started");
        }
        glass.shutdown(Deadline::at(
            Instant::now() + budget - glass_core::TEARDOWN_REAP_HEADROOM,
        ));
    });
    match tokio::time::timeout(budget, task).await {
        Ok(Ok(())) => {}
        // A panic inside teardown: the steps behind it did not run, and the process is about to
        // exit past it.
        Ok(Err(e)) => eprintln!("glass: teardown panicked ({e}); exiting anyway"),
        Err(_) => eprintln!("glass: shutdown exceeded {budget:?}; exiting anyway"),
    }
}

/// Resolves when a graceful termination signal arrives (SIGTERM/SIGINT on Unix;
/// Ctrl-C / console-close / shutdown on Windows). Installing the handlers also stops
/// the default-terminate behavior, so the select in `main` can run teardown first.
#[cfg(unix)]
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(windows)]
pub async fn shutdown_signal() {
    use tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_shutdown};
    let mut c = ctrl_c().expect("install Ctrl-C handler");
    let mut close = ctrl_close().expect("install Ctrl-Close handler");
    let mut shut = ctrl_shutdown().expect("install Ctrl-Shutdown handler");
    tokio::select! {
        _ = c.recv() => {}
        _ = close.recv() => {}
        _ = shut.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use glass_core::{
        AppSpec, Backend, BaselineStore, Frame, Glass, KeyEvent, Platform, PlatformFactory,
        PointerEvent, Region, Result, Stream, WindowGeometry, WindowId, WindowInfo, WindowOp,
    };
    use tokio::sync::Mutex;

    /// A backend whose `stop_app` blocks far longer than any test budget, to prove
    /// `run_shutdown` is time-bounded and does not block on a wedged teardown.
    struct BlockingBackend;
    impl Platform for BlockingBackend {
        fn start_app(&mut self, _s: &AppSpec) -> Result<WindowGeometry> {
            Ok(WindowGeometry {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            })
        }
        fn stop_app_by(&mut self, _deadline: glass_core::Deadline) -> Result<()> {
            // 2s >> the 200ms budget below — long enough to prove `run_shutdown`
            // returns on the timeout rather than waiting for stop_app, but short
            // enough that the runtime's wait for this detached blocking thread at
            // test teardown doesn't bloat `cargo test`.
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        }
        fn capture_frame_by(
            &mut self,
            _r: Option<&Region>,
            _deadline: glass_core::Deadline,
        ) -> Result<Frame> {
            unimplemented!()
        }
        fn capture_window_by(
            &mut self,
            _id: WindowId,
            _region: Option<&Region>,
            deadline: glass_core::Deadline,
        ) -> Result<Frame> {
            if deadline.has_passed() {
                return Err(glass_core::GlassError::deadline_not_started(
                    "window capture",
                ));
            }
            Err(glass_core::GlassError::Unsupported(
                "capture_window is not supported by this backend".into(),
            ))
        }
        fn send_pointer_by(
            &mut self,
            _e: &PointerEvent,
            _deadline: glass_core::Deadline,
        ) -> Result<()> {
            unimplemented!()
        }
        fn send_key_by(&mut self, _e: &KeyEvent, _deadline: glass_core::Deadline) -> Result<()> {
            unimplemented!()
        }
        fn window_by(
            &mut self,
            _o: &WindowOp,
            _deadline: glass_core::Deadline,
        ) -> Result<WindowGeometry> {
            unimplemented!()
        }
        // start_on() lists windows (best-effort) to attribute audit records, so this
        // must answer rather than panic; no windows is fine for the shutdown test.
        fn list_windows_by(&mut self, _deadline: glass_core::Deadline) -> Result<Vec<WindowInfo>> {
            Ok(vec![])
        }
        fn select_window_by(
            &mut self,
            _id: WindowId,
            _deadline: glass_core::Deadline,
        ) -> Result<WindowGeometry> {
            unimplemented!()
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            vec![]
        }
    }

    fn glass_with_blocking_backend() -> Glass {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir); // keep the temp dir alive for the test
        let factory: PlatformFactory =
            Box::new(|_backend| Ok(Backend::display_only(Box::new(BlockingBackend))));
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
    }

    fn spec() -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1000,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        }
    }

    /// Nothing else checks that `run_shutdown` subtracts the headroom that pays for killing a
    /// wedged step — up to `KILL_REAP` past its deadline.
    #[tokio::test]
    async fn the_deadline_the_backend_gets_leaves_room_to_kill_a_wedged_step() {
        struct RecordingBackend(Arc<std::sync::Mutex<Option<Duration>>>);
        impl Platform for RecordingBackend {
            fn start_app(&mut self, _s: &AppSpec) -> Result<WindowGeometry> {
                Ok(WindowGeometry {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                })
            }
            fn stop_app(&mut self) -> Result<()> {
                Ok(())
            }
            fn stop_app_by(&mut self, deadline: glass_core::Deadline) -> Result<()> {
                *self.0.lock().unwrap() = deadline.remaining();
                Ok(())
            }
            fn capture_frame_by(
                &mut self,
                _r: Option<&Region>,
                _deadline: glass_core::Deadline,
            ) -> Result<Frame> {
                unimplemented!()
            }
            fn capture_window_by(
                &mut self,
                _id: WindowId,
                _region: Option<&Region>,
                deadline: glass_core::Deadline,
            ) -> Result<Frame> {
                if deadline.has_passed() {
                    return Err(glass_core::GlassError::deadline_not_started(
                        "window capture",
                    ));
                }
                Err(glass_core::GlassError::Unsupported(
                    "capture_window is not supported by this backend".into(),
                ))
            }
            fn send_pointer_by(
                &mut self,
                _e: &PointerEvent,
                _deadline: glass_core::Deadline,
            ) -> Result<()> {
                unimplemented!()
            }
            fn send_key_by(
                &mut self,
                _e: &KeyEvent,
                _deadline: glass_core::Deadline,
            ) -> Result<()> {
                unimplemented!()
            }
            fn window_by(
                &mut self,
                _o: &WindowOp,
                _deadline: glass_core::Deadline,
            ) -> Result<WindowGeometry> {
                unimplemented!()
            }
            fn list_windows_by(
                &mut self,
                _deadline: glass_core::Deadline,
            ) -> Result<Vec<WindowInfo>> {
                Ok(vec![])
            }
            fn select_window_by(
                &mut self,
                _id: WindowId,
                _deadline: glass_core::Deadline,
            ) -> Result<WindowGeometry> {
                unimplemented!()
            }
            fn drain_logs(&mut self) -> Vec<(Stream, String)> {
                vec![]
            }
        }

        let seen = Arc::new(std::sync::Mutex::new(None));
        let recorded = seen.clone();
        let dir = tempfile::tempdir().unwrap();
        let factory: PlatformFactory = Box::new(move |_b| {
            Ok(Backend::display_only(Box::new(RecordingBackend(
                recorded.clone(),
            ))))
        });
        let mut glass = Glass::new(
            factory,
            "x11".into(),
            BaselineStore::new(dir.path().join("baselines")),
            100,
        );
        glass.start(&spec()).unwrap();

        let budget = Duration::from_secs(3);
        run_shutdown(Arc::new(Mutex::new(glass)), budget).await;

        let got = seen.lock().unwrap().expect("the backend was stopped");
        // With a margin: without the subtraction the backend still comes up a few microseconds
        // short of the budget, which a bare `<` would read as headroom.
        assert!(
            got + Duration::from_millis(100) < budget - glass_core::TEARDOWN_REAP_HEADROOM,
            "the backend was handed {got:?} of a {budget:?} budget — the headroom that pays for \
             killing a wedged step was not subtracted"
        );
    }

    #[tokio::test]
    async fn run_shutdown_is_bounded_when_teardown_blocks() {
        let mut glass = glass_with_blocking_backend();
        glass.start(&spec()).unwrap();
        let sessions = Arc::new(Mutex::new(glass));
        let start = Instant::now();
        run_shutdown(sessions, Duration::from_millis(200)).await;
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "run_shutdown must return within the budget, not block on a wedged stop_app"
        );
    }
}
