//! The macOS `Platform` backend. Plan 1 lands the struct + trait surface with the
//! window-server methods stubbed; Plan 2 fills capture + display provisioning, Plan 3
//! input, Plan 4 windows. `new()` runs the TCC preflight so a missing grant fails fast.

use glass_core::frame::{Frame, Region};
use glass_core::logbuf::Stream;
use glass_core::platform::{
    AppSpec, KeyEvent, Platform, PointerEvent, WindowGeometry, WindowId, WindowInfo, WindowOp,
};
use glass_core::Result;

use crate::permissions;

/// macOS backend. v1 renders the target app onto a `CGVirtualDisplay` (Plan 2) and
/// drives it with ScreenCaptureKit + CGEvent + AXUIElement.
pub struct MacosPlatform {
    /// Logs drained by `drain_logs`, filled by the per-stream readers once `start_app`
    /// exists (Plan 2). Empty until then.
    logs: Vec<(Stream, String)>,
    /// The launched app's root pid; `None` until `start_app`.
    app_pid: Option<u32>,
}

impl MacosPlatform {
    /// Construct the backend, failing fast if a required TCC grant is missing.
    pub fn new() -> Result<Self> {
        permissions::preflight()?;
        Ok(Self { logs: Vec::new(), app_pid: None })
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
        std::mem::take(&mut self.logs)
    }
    fn app_pid(&self) -> Option<u32> {
        self.app_pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_logs_is_empty_then_drains() {
        // Build without preflight (which would require grants) by constructing the struct
        // directly — `new()` is exercised in the Mac-gated suite.
        let mut p = MacosPlatform { logs: vec![(Stream::Stdout, "hi".into())], app_pid: Some(42) };
        assert_eq!(p.app_pid(), Some(42));
        assert_eq!(p.drain_logs().len(), 1);
        assert!(p.drain_logs().is_empty());
    }
}
