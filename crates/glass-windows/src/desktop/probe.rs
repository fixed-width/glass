use glass_core::{Deadline, GlassError, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
    TranslateMessage, WS_DISABLED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

/// Prove capture in the current desktop with a short-lived, nonactivating owned window.
pub fn probe_graphical_desktop(deadline: Deadline) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("glass-desktop-readiness".into())
        .spawn(move || {
            // SAFETY: the system STATIC class needs no custom procedure or application instance.
            let window = match unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("Glass desktop readiness"),
                    WS_POPUP | WS_VISIBLE | WS_DISABLED,
                    20,
                    20,
                    128,
                    96,
                    None,
                    None,
                    None,
                    None,
                )
            } {
                Ok(window) => window,
                Err(error) => {
                    let _ = sender.send(Err(GlassError::Backend(format!(
                        "child desktop readiness window: {error}"
                    ))));
                    return;
                }
            };
            let _ = sender.send(Ok(window.0 as i64));
            while !stopped.load(Ordering::Acquire) {
                let mut message = MSG::default();
                // SAFETY: this thread owns the window and pumps only its own message queue.
                unsafe {
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: the creating thread exclusively owns the window and destroys it once.
            unsafe {
                let _ = DestroyWindow(window);
            }
        })
        .map_err(|error| GlassError::Backend(format!("start child desktop probe: {error}")))?;
    let result = (|| {
        let window = receiver
            .recv_timeout(
                deadline
                    .remaining()
                    .unwrap_or(Duration::from_secs(5))
                    .min(Duration::from_secs(5)),
            )
            .map_err(|error| {
                GlassError::Backend(format!("child readiness window did not answer: {error}"))
            })??;
        let frame = crate::capture::capture_window_by(HWND(window as *mut _), None, deadline)?;
        if frame.width == 0 || frame.height == 0 {
            return Err(GlassError::CaptureFailed(
                "child readiness capture was empty".into(),
            ));
        }
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    thread
        .join()
        .map_err(|_| GlassError::Backend("child readiness thread panicked".into()))?;
    result
}
