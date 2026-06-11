//! Process-exit teardown: route every graceful shutdown through one bounded,
//! best-effort `Glass::shutdown()`, plus a cross-platform termination signal.

use std::sync::Arc;
use std::time::Duration;

use glass_core::Glass;
use tokio::sync::Mutex;

/// Best-effort, time-bounded teardown of all sessions for process exit. The backend
/// teardown blocks (it waits on the child), so it runs off the async reactor via
/// `spawn_blocking`; after `budget` we stop waiting and let the OS reap whatever is
/// left — we are exiting regardless.
pub async fn run_shutdown(sessions: Arc<Mutex<Glass>>, budget: Duration) {
    let task = tokio::task::spawn_blocking(move || {
        // On a `spawn_blocking` thread, `blocking_lock` is allowed (it would panic on
        // a reactor worker thread).
        sessions.blocking_lock().shutdown();
    });
    if tokio::time::timeout(budget, task).await.is_err() {
        eprintln!("glass: shutdown exceeded {budget:?}; exiting anyway");
    }
}

/// Resolves when a graceful termination signal arrives (SIGTERM/SIGINT on Unix;
/// Ctrl-C / console-close / shutdown on Windows). Installing the handlers also stops
/// the default-terminate behavior, so the select in `main` can run teardown first.
#[cfg(unix)]
pub async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
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
            Ok(WindowGeometry { x: 0, y: 0, width: 10, height: 10 })
        }
        fn stop_app(&mut self) -> Result<()> {
            // 2s >> the 200ms budget below — long enough to prove `run_shutdown`
            // returns on the timeout rather than waiting for stop_app, but short
            // enough that the runtime's wait for this detached blocking thread at
            // test teardown doesn't bloat `cargo test`.
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        }
        fn capture_frame(&mut self, _r: Option<&Region>) -> Result<Frame> { unimplemented!() }
        fn send_pointer(&mut self, _e: &PointerEvent) -> Result<()> { unimplemented!() }
        fn send_key(&mut self, _e: &KeyEvent) -> Result<()> { unimplemented!() }
        fn window(&mut self, _o: &WindowOp) -> Result<WindowGeometry> { unimplemented!() }
        fn list_windows(&mut self) -> Result<Vec<WindowInfo>> { unimplemented!() }
        fn select_window(&mut self, _id: WindowId) -> Result<WindowGeometry> { unimplemented!() }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> { vec![] }
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
