//! The `Glass` session manager. The type, its state, and shared helpers live here;
//! its operations are grouped into submodules (each adds an `impl Glass` block).

use crate::accessibility::{
    element_match, Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxTarget, AxTree,
    ClickMethod, ElementCondition, ElementInfo, ElementMatch, WalkLimits,
};
use crate::baseline::BaselineStore;
use crate::diff::{
    diff_perceptual_with_mask, diff_with_mask, region_satisfied, BBox, DiffResult, IgnoreMask,
    RegionUntil,
};
use crate::error::{GlassError, Result};
use crate::frame::{Frame, Region};
use crate::logbuf::{LogBuffer, LogLine, Stream};
use crate::marks::Mark;
use crate::platform::{
    AppSpec, KeyEvent, MouseButton, Platform, PointerEvent, WindowGeometry, WindowId, WindowInfo,
    WindowOp,
};
use crate::stability::StabilityTracker;

mod a11y;
mod baseline;
mod capture;
mod clipboard;
mod input;
mod lifecycle;
mod wait;
mod window;

pub use wait::{
    ScrollDirection, ScrollToElementOutcome, ScrollToElementParams, WaitElementOutcome,
    WaitElementParams, WaitLogOutcome, WaitLogParams, WaitRegionOutcome, WaitRegionParams,
    WaitStableOutcome, WaitStableParams, SCROLL_TO_DEFAULT_STEP, SCROLL_TO_DEFAULT_TIMEOUT_MS,
};

struct ActiveSession {
    platform: Box<dyn Platform + Send>,
    // Held here so the session owns the backend's accessibility reader and the
    // last-captured tree (read by the a11y tools).
    accessibility: Option<Box<dyn Accessibility + Send>>,
    last_ax: Option<AxTree>,
    /// Limits the most recent `a11y_snapshot` used; reused by `set_value` so ids from a
    /// raised-cap snapshot stay resolvable. Defaults to `WalkLimits::DEFAULT`.
    a11y_limits: WalkLimits,
    geometry: WindowGeometry,
    logs: LogBuffer,
    /// Best-effort active window for audit attribution (id from list_windows/select_window).
    active_window: Option<crate::audit::WindowRef>,
}

impl ActiveSession {
    /// Drain the backend's captured logs into the session buffer.
    fn pump(&mut self) {
        for (stream, text) in self.platform.drain_logs() {
            self.logs.push(stream, text);
        }
    }
}

/// A constructed backend: the display `Platform` plus an optional per-OS
/// accessibility reader. The factory returns this so a backend can supply both
/// halves while `glass-core` stays platform-agnostic.
pub struct Backend {
    pub platform: Box<dyn Platform + Send>,
    pub accessibility: Option<Box<dyn Accessibility + Send>>,
}

impl Backend {
    /// A backend with no accessibility support (tools return `AxUnsupported`).
    pub fn display_only(platform: Box<dyn Platform + Send>) -> Self {
        Self {
            platform,
            accessibility: None,
        }
    }
}

/// Builds a backend by name (e.g. `"x11"`/`"wayland"`). Supplied by the binary
/// (glass-mcp) — the only layer that knows the concrete backends — so glass-core
/// stays platform-agnostic.
pub type PlatformFactory = Box<dyn FnMut(&str) -> Result<Backend> + Send>;

/// The session manager: builds the active app's backend on demand, owns its
/// geometry/logs and the baseline store, and routes tool ops to the backend with
/// validation and log pumping. One active session at a time (v1); the backend is
/// chosen per session via the factory.
pub struct Glass {
    factory: PlatformFactory,
    default_backend: String,
    baselines: BaselineStore,
    log_capacity: usize,
    active: Option<ActiveSession>,
    audit: Option<Box<dyn crate::audit::AuditSink>>,
    shutdown_hook: Option<Box<dyn FnOnce() + Send>>,
}

impl Glass {
    pub fn new(
        factory: PlatformFactory,
        default_backend: String,
        baselines: BaselineStore,
        log_capacity: usize,
    ) -> Self {
        Self {
            factory,
            default_backend,
            baselines,
            log_capacity: log_capacity.max(1),
            active: None,
            audit: None,
            shutdown_hook: None,
        }
    }

    /// Install the audit sink. Every subsequent actuation is recorded through it.
    pub fn set_audit_sink(&mut self, sink: Box<dyn crate::audit::AuditSink>) {
        self.audit = Some(sink);
    }

    /// Install a teardown callback run once at the end of `shutdown()` — used by the host
    /// (glass-mcp) for resource cleanup it owns (e.g. stopping a glass-booted emulator).
    pub fn set_shutdown_hook(&mut self, hook: Box<dyn FnOnce() + Send>) {
        self.shutdown_hook = Some(hook);
    }

    fn emit_audit(
        &self,
        act: &crate::audit::Actuation,
        outcome: crate::audit::AuditOutcome,
        dur: std::time::Duration,
    ) {
        if let Some(sink) = &self.audit {
            let window = self.active.as_ref().and_then(|s| s.active_window.clone());
            sink.record(
                act,
                &crate::audit::ActuationContext { window },
                &outcome,
                dur,
            );
        }
    }

    fn element_ref(&self, id: AxNodeId) -> crate::audit::ElementRef {
        let (role, name) = self
            .active
            .as_ref()
            .and_then(|s| s.last_ax.as_ref())
            .and_then(|t| t.find(id))
            .map(|n| (Some(format!("{:?}", n.role)), n.name.clone()))
            .unwrap_or((None, None));
        crate::audit::ElementRef {
            id: id.0,
            role,
            name,
        }
    }

    fn require_active(&self) -> Result<&ActiveSession> {
        self.active.as_ref().ok_or(GlassError::NoActiveSession)
    }

    fn active_mut(&mut self) -> Result<&mut ActiveSession> {
        self.active.as_mut().ok_or(GlassError::NoActiveSession)
    }

    pub fn logs(
        &mut self,
        cursor: u64,
        max: usize,
        stream: Option<Stream>,
        contains: Option<&str>,
    ) -> Result<(Vec<LogLine>, u64)> {
        let s = self.active_mut()?;
        s.pump();
        Ok(s.logs.read(cursor, max, stream, contains))
    }
}

/// Build the ignore mask for a comparison over a `frame_w`×`frame_h` frame,
/// optionally scoped to `region`. Without a region the window-relative rects mask
/// the frame directly; with one they are intersected with it and translated into
/// region-local space — the space the scoped comparison runs in — so callers
/// always pass window-relative rects regardless of scoping. Picking the right
/// [`IgnoreMask`] constructor here keeps each one unambiguous: [`IgnoreMask::new`]
/// sizes from the frame, [`IgnoreMask::for_region`] from the region.
fn mask_for(
    ignore: &[Region],
    region: Option<&Region>,
    frame_w: u32,
    frame_h: u32,
) -> Result<IgnoreMask> {
    match region {
        Some(r) => IgnoreMask::for_region(ignore, r),
        None => IgnoreMask::new(ignore, frame_w, frame_h),
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::*;

    #[test]
    fn glass_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Glass>();
    }

    #[test]
    fn seam_records_actuations_skips_reads_and_geometry() {
        let sink = RecordingSink::default();
        let frame = Frame::solid(100, 100, [0, 0, 0, 255]);
        let mut g = glass_with_a11y(
            FakePlatform::new(100, 100).with_frames(vec![frame.clone(), frame]),
            fake_tree(),
        );
        g.set_audit_sink(Box::new(sink.clone()));

        g.start(&spec()).unwrap();
        let _ = g.screenshot(None, None).unwrap(); // read
        let tree = g.a11y_snapshot(None).unwrap(); // read (populates last_ax)
        g.pointer(&PointerEvent::Click {
            x: 1,
            y: 2,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        })
        .unwrap();
        g.key(&KeyEvent::Text("hi".into())).unwrap();
        let _ = g.window(&WindowOp::Geometry).unwrap(); // read → no record
        g.window(&WindowOp::Focus).unwrap(); // actuation
        g.click_element(first_button(&tree)).unwrap();
        g.stop().unwrap();

        let got = sink.0.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["launch:true", "click:true", "type:true", "window:true", "click_element:true", "stop:true"],
            "reads (screenshot, a11y_snapshot, window-geometry) produce no records; click_element records ONCE (not also as click)"
        );
    }

    #[test]
    fn seam_records_failed_actuation_ok_false() {
        let sink = RecordingSink::default();
        let mut g =
            glass_with(FakePlatform::new(50, 50).with_frames(vec![Frame::solid(50, 50, [0; 4])]));
        g.set_audit_sink(Box::new(sink.clone()));
        g.start(&spec()).unwrap();
        // Out-of-bounds click fails check_bounds → still recorded as ok:false.
        let _ = g.pointer(&PointerEvent::Click {
            x: 999,
            y: 0,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        });
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(got, vec!["launch:true", "click:false"]);
    }

    #[test]
    fn no_sink_means_no_behavior_change() {
        let mut g =
            glass_with(FakePlatform::new(10, 10).with_frames(vec![Frame::solid(10, 10, [0; 4])]));
        g.start(&spec()).unwrap();
        g.pointer(&PointerEvent::Click {
            x: 0,
            y: 0,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        })
        .unwrap();
    }
}
