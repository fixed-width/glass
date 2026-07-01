//! The macOS `Platform` backend. Plan 1 lands the struct + trait surface with the
//! window-server methods stubbed; Plan 2 fills capture + display provisioning, Plan 3
//! input, Plan 4 windows. `new()` runs the TCC preflight so a missing grant fails fast.

use std::sync::{Arc, Mutex};

use glass_core::frame::{Frame, Region};
use glass_core::logbuf::Stream;
use glass_core::platform::{
    AppSpec, KeyEvent, Platform, PointerEvent, WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use glass_core::Result;

use crate::permissions;
use crate::process::LogSink;

/// macOS backend. v1 renders the target app onto a `CGVirtualDisplay` (Plan 2) and
/// drives it with ScreenCaptureKit + CGEvent + AXUIElement.
pub struct MacosPlatform {
    /// Logs drained by `drain_logs`, filled by `process::spawn`'s per-stream reader
    /// threads once `start_app` exists (Plan 2). `Arc<Mutex<_>>` because those threads
    /// push into it concurrently with `drain_logs` reading it here. Empty until
    /// `start_app` launches a child.
    logs: LogSink,
    /// The launched app's root pid; `None` until `start_app`.
    app_pid: Option<u32>,
}

impl MacosPlatform {
    /// Construct the backend, failing fast if a required TCC grant is missing.
    pub fn new() -> Result<Self> {
        permissions::preflight()?;
        Ok(Self { logs: Arc::new(Mutex::new(Vec::new())), app_pid: None })
    }
}

impl Platform for MacosPlatform {
    fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
        unimplemented!("Plan 2: spawn + CGVirtualDisplay + window discovery")
    }
    fn stop_app(&mut self) -> Result<()> {
        unimplemented!("Plan 2: terminate child + tear down provisioning")
    }
    fn capture_frame(&mut self, _region: Option<&Region>) -> Result<Frame> {
        unimplemented!("Plan 2: ScreenCaptureKit per-window capture")
    }
    fn send_pointer(&mut self, _event: &PointerEvent) -> Result<()> {
        unimplemented!("Plan 3: CGEvent pointer")
    }
    fn send_key(&mut self, _event: &KeyEvent) -> Result<()> {
        unimplemented!("Plan 3: CGEvent keyboard")
    }
    fn window(&mut self, _op: &WindowOp) -> Result<WindowGeometry> {
        unimplemented!("Plan 4: AXUIElement window ops")
    }
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        unimplemented!("Plan 4: CGWindowList/SCShareableContent by pid")
    }
    fn select_window(&mut self, _id: WindowId) -> Result<WindowGeometry> {
        unimplemented!("Plan 4: raise + focus + activate")
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        std::mem::take(&mut *self.logs.lock().expect("log buffer mutex"))
    }
    fn app_pid(&self) -> Option<u32> {
        self.app_pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_logs_takes_then_empties() {
        // Build without preflight (which would require grants) by constructing the struct
        // directly — `new()` is exercised in the Mac-gated suite.
        let mut p = MacosPlatform {
            logs: Arc::new(Mutex::new(vec![(Stream::Stdout, "hi".into())])),
            app_pid: Some(42),
        };
        assert_eq!(p.drain_logs().len(), 1);
        assert!(p.drain_logs().is_empty());
    }

    #[test]
    fn app_pid_returns_the_constructed_value() {
        let p = MacosPlatform { logs: Arc::new(Mutex::new(Vec::new())), app_pid: Some(42) };
        assert_eq!(p.app_pid(), Some(42));
    }

    #[test]
    fn new_agrees_with_preflight() {
        // The central invariant: new() must error iff preflight() errors. Guards against a
        // future edit that swallows the missing-grant propagation. On an ungranted CI runner
        // both are Err; on a granted box both are Ok.
        assert_eq!(crate::permissions::preflight().is_err(), MacosPlatform::new().is_err());
    }
}
