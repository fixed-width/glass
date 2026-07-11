//! Shared `#[cfg(test)]` scaffolding for the session submodules: the scriptable
//! `FakePlatform`/`FakeAccessibility` backends and the `glass_with*` builders.
#![cfg(test)]

pub(crate) use super::*;
pub(crate) use crate::accessibility::{
    AxNode, AxRect, AxRole, AxStates, AxTarget, ElementCondition,
};
pub(crate) use crate::audit::{Actuation, ActuationContext, AuditOutcome, AuditSink};
pub(crate) use crate::platform::{SandboxLevel, Segment};
pub(crate) use std::collections::VecDeque;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::Duration;

/// Every `capture_window(id, region)` call `FakePlatform` recorded, for
/// asserting it (not `capture_frame`) was used, and with what arguments.
pub(crate) type CaptureWindowLog = Arc<Mutex<Vec<(WindowId, Option<Region>)>>>;

/// Scriptable in-memory backend for testing the session manager.
#[derive(Default)]
pub(crate) struct FakePlatform {
    geometry: WindowGeometry,
    frames: VecDeque<Frame>,
    pending_logs: Vec<(Stream, String)>,
    pointer_events: Vec<PointerEvent>,
    key_events: Vec<KeyEvent>,
    started: bool,
    capture_log: Arc<Mutex<Vec<Option<Region>>>>,
    click_log: Arc<Mutex<Vec<(i32, i32)>>>,
    scroll_log: Arc<Mutex<Vec<PointerEvent>>>,
    stop_count: Option<Arc<Mutex<u32>>>,
    windows: Vec<WindowInfo>,
    clipboard: String,
    /// Frames `capture_window` serves, keyed by window id — independent of
    /// `frames` (the active-window `capture_frame` script).
    window_frames: std::collections::HashMap<WindowId, Frame>,
    /// Every `capture_window(id, region)` call, for asserting it (not
    /// `capture_frame`) was used, and with what arguments.
    capture_window_log: CaptureWindowLog,
    /// Every `select_window(id)` call, in order — for asserting popover routing
    /// selects the popover then restores the previously-active window.
    select_log: Arc<Mutex<Vec<WindowId>>>,
    /// When set, `list_windows` errors instead of returning its scripted list — for
    /// proving a failed popover-probe enumeration degrades to the normal click path
    /// instead of propagating.
    fail_list_windows: bool,
}

impl FakePlatform {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width,
                height,
            },
            ..Default::default()
        }
    }
    pub(crate) fn with_frames(mut self, frames: Vec<Frame>) -> Self {
        self.frames = frames.into();
        self
    }
    pub(crate) fn with_capture_log(mut self, log: Arc<Mutex<Vec<Option<Region>>>>) -> Self {
        self.capture_log = log;
        self
    }
    pub(crate) fn with_click_log(mut self, log: Arc<Mutex<Vec<(i32, i32)>>>) -> Self {
        self.click_log = log;
        self
    }
    pub(crate) fn with_scroll_log(mut self, log: Arc<Mutex<Vec<PointerEvent>>>) -> Self {
        self.scroll_log = log;
        self
    }
    pub(crate) fn with_select_log(mut self, log: Arc<Mutex<Vec<WindowId>>>) -> Self {
        self.select_log = log;
        self
    }
    pub(crate) fn counting_stops(mut self, c: Arc<Mutex<u32>>) -> Self {
        self.stop_count = Some(c);
        self
    }
    pub(crate) fn with_logs(mut self, logs: Vec<(Stream, &str)>) -> Self {
        self.pending_logs = logs.into_iter().map(|(s, t)| (s, t.to_string())).collect();
        self
    }
    pub(crate) fn with_windows(mut self, windows: Vec<WindowInfo>) -> Self {
        self.windows = windows;
        self
    }
    pub(crate) fn with_window_frame(mut self, id: WindowId, frame: Frame) -> Self {
        self.window_frames.insert(id, frame);
        self
    }
    pub(crate) fn with_capture_window_log(mut self, log: CaptureWindowLog) -> Self {
        self.capture_window_log = log;
        self
    }
    pub(crate) fn with_failing_list_windows(mut self) -> Self {
        self.fail_list_windows = true;
        self
    }
}

impl Platform for FakePlatform {
    fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
        self.started = true;
        Ok(self.geometry.clone())
    }
    fn stop_app(&mut self) -> Result<()> {
        self.started = false;
        if let Some(c) = &self.stop_count {
            *c.lock().unwrap() += 1;
        }
        Ok(())
    }
    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        self.capture_log.lock().unwrap().push(region.copied());
        let frame = match self.frames.pop_front() {
            Some(f) => {
                if self.frames.is_empty() {
                    self.frames.push_back(f.clone()); // repeat the last frame forever
                }
                f
            }
            None => return Err(GlassError::CaptureFailed("no scripted frames".into())),
        };
        match region {
            Some(r) => frame.crop(r),
            None => Ok(frame),
        }
    }
    fn capture_window(&mut self, id: WindowId, region: Option<&Region>) -> Result<Frame> {
        self.capture_window_log
            .lock()
            .unwrap()
            .push((id, region.copied()));
        let frame = self
            .window_frames
            .get(&id)
            .cloned()
            .ok_or(GlassError::WindowNotFound)?;
        match region {
            Some(r) => frame.crop(r),
            None => Ok(frame),
        }
    }
    fn send_pointer(&mut self, event: &PointerEvent) -> Result<()> {
        if let PointerEvent::Click { x, y, .. } = event {
            self.click_log.lock().unwrap().push((*x, *y));
        }
        if let PointerEvent::Scroll { .. } = event {
            self.scroll_log.lock().unwrap().push(event.clone());
        }
        self.pointer_events.push(event.clone());
        Ok(())
    }
    fn app_pid(&self) -> Option<u32> {
        Some(4242)
    }
    fn send_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.key_events.push(event.clone());
        Ok(())
    }
    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        match *op {
            WindowOp::Resize { width, height } => {
                self.geometry.width = width;
                self.geometry.height = height;
            }
            WindowOp::Move { x, y } => {
                self.geometry.x = x;
                self.geometry.y = y;
            }
            WindowOp::Focus | WindowOp::Geometry => {}
        }
        Ok(self.geometry.clone())
    }
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        if self.fail_list_windows {
            return Err(GlassError::Backend("list_windows unavailable".into()));
        }
        if self.windows.is_empty() {
            Ok(vec![WindowInfo {
                id: WindowId(0),
                title: None,
                class: None,
                geometry: self.geometry.clone(),
                active: true,
            }])
        } else {
            Ok(self.windows.clone())
        }
    }
    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        self.select_log.lock().unwrap().push(id);
        if self.windows.is_empty() {
            return if id == WindowId(0) {
                Ok(self.geometry.clone())
            } else {
                Err(GlassError::WindowNotFound)
            };
        }
        let w = self
            .windows
            .iter()
            .find(|w| w.id == id)
            .ok_or(GlassError::WindowNotFound)?;
        self.geometry = w.geometry.clone();
        Ok(self.geometry.clone())
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        std::mem::take(&mut self.pending_logs)
    }
    fn get_clipboard(&mut self) -> Result<String> {
        Ok(self.clipboard.clone())
    }
    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        self.clipboard = text.to_string();
        Ok(())
    }
}

/// A scriptable `Accessibility` returning a fixed tree.
pub(crate) struct FakeAccessibility {
    pub(crate) tree: AxTree,
    pub(crate) set_log: std::sync::Arc<std::sync::Mutex<Vec<(AxTarget, String)>>>,
    pub(crate) set_fail: bool,
}

impl Accessibility for FakeAccessibility {
    fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
        Ok(self.tree.clone())
    }
    fn set_value(&mut self, _ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        if self.set_fail {
            return Err(GlassError::AxElementNotEditable(target.id.0));
        }
        self.set_log
            .lock()
            .unwrap()
            .push((target.clone(), text.to_string()));
        Ok(())
    }
}

/// A two-node tree: Window #0 containing a Button "Save" at (10,10 20x20).
pub(crate) fn fake_tree() -> AxTree {
    let button = AxNode {
        id: AxNodeId(0),
        role: AxRole::Button,
        raw_role: "push button".into(),
        name: Some("Save".into()),
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        }),
        children: vec![],
    };
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "frame".into(),
        name: Some("Win".into()),
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }),
        children: vec![button],
    };
    AxTree { root, count: 0 }
}

/// Like `fake_tree` but the Button "Save" is enabled.
pub(crate) fn fake_tree_enabled() -> AxTree {
    let mut t = fake_tree();
    t.root.children[0].states = AxStates {
        enabled: true,
        ..Default::default()
    };
    t
}

pub(crate) fn window_info(id: u64, geometry: WindowGeometry, active: bool) -> WindowInfo {
    WindowInfo {
        id: WindowId(id),
        title: None,
        class: None,
        geometry,
        active,
    }
}

pub(crate) fn ax_node(
    id: u32,
    role: AxRole,
    bounds: Option<AxRect>,
    children: Vec<AxNode>,
) -> AxNode {
    AxNode {
        id: AxNodeId(id),
        role,
        raw_role: format!("{role:?}"),
        name: None,
        value: None,
        states: AxStates::default(),
        bounds,
        children,
    }
}

pub(crate) fn glass_with(platform: FakePlatform) -> Glass {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    // Keep the temp dir alive for the test's lifetime (no deprecated API).
    std::mem::forget(dir);
    // Factory yields the pre-scripted platform once (tests start a session once).
    let mut held: Option<Box<dyn Platform + Send>> = Some(Box::new(platform));
    let factory: PlatformFactory = Box::new(move |_backend| {
        let platform = held
            .take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))?;
        Ok(Backend::display_only(platform))
    });
    Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
}

pub(crate) fn glass_with_a11y(platform: FakePlatform, tree: AxTree) -> Glass {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut held: Option<Backend> = Some(Backend {
        platform: Box::new(platform),
        accessibility: Some(Box::new(FakeAccessibility {
            tree,
            set_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            set_fail: false,
        })),
    });
    let factory: PlatformFactory = Box::new(move |_backend| {
        held.take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
    });
    Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
}

/// An `Accessibility` that returns a scripted sequence of trees — one per
/// `snapshot()` call, repeating the last — so a test can model rows/columns
/// realizing on-screen as the container scrolls.
pub(crate) struct SeqAccessibility {
    trees: Vec<AxTree>,
    idx: usize,
}

impl Accessibility for SeqAccessibility {
    fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
        let t = self.trees[self.idx.min(self.trees.len() - 1)].clone();
        self.idx += 1;
        Ok(t)
    }
    fn set_value(&mut self, _ctx: &AxContext, _t: &AxTarget, _s: &str) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn glass_with_a11y_seq(platform: FakePlatform, trees: Vec<AxTree>) -> Glass {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut held: Option<Backend> = Some(Backend {
        platform: Box::new(platform),
        accessibility: Some(Box::new(SeqAccessibility { trees, idx: 0 })),
    });
    let factory: PlatformFactory = Box::new(move |_backend| {
        held.take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
    });
    Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
}

pub(crate) fn named_node(id: u32, role: AxRole, name: &str, bounds: AxRect) -> AxNode {
    AxNode {
        id: AxNodeId(id),
        role,
        raw_role: format!("{role:?}"),
        name: Some(name.into()),
        value: None,
        states: AxStates::default(),
        bounds: Some(bounds),
        children: vec![],
    }
}

pub(crate) fn tree_with(win_w: u32, win_h: u32, children: Vec<AxNode>) -> AxTree {
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "frame".into(),
        name: Some("Win".into()),
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: win_w,
            height: win_h,
        }),
        children,
    };
    AxTree { root, count: 0 }
}

pub(crate) fn spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["app".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    }
}

pub(crate) fn frame_4x4_corner(corner: [u8; 4]) -> Frame {
    // 4x4 opaque black, with only pixel (3,3) set to `corner`.
    let mut px = vec![0u8; 4 * 4 * 4];
    for i in 0..16 {
        px[i * 4 + 3] = 255; // alpha
    }
    let idx = (3 * 4 + 3) * 4;
    px[idx..idx + 4].copy_from_slice(&corner);
    Frame::new(4, 4, px).unwrap()
}

/// Build a `Glass` over a custom factory (for backend-routing tests).
pub(crate) fn glass_with_factory(factory: PlatformFactory) -> Glass {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
}

/// Builds the tree used by the popover-routing tests: root Window > `List`
/// (sized like the popover window) > `ListItem` "Globex" (the click target),
/// with the validated real-Xvfb numbers (see the `owning_popover`/
/// `menu_container_bounds` unit tests in the `a11y` submodule).
pub(crate) fn fake_tree_with_popover_option() -> AxTree {
    let globex = AxNode {
        id: AxNodeId(0),
        role: AxRole::ListItem,
        raw_role: "list item".into(),
        name: Some("Globex".into()),
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 20,
            y: 248,
            width: 80,
            height: 27,
        }),
        children: vec![],
    };
    let list = AxNode {
        id: AxNodeId(0),
        role: AxRole::List,
        raw_role: "list".into(),
        name: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 194,
            width: 326,
            height: 129,
        }),
        children: vec![globex],
    };
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "frame".into(),
        name: Some("Win".into()),
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: 340,
            height: 300,
        }),
        children: vec![list],
    };
    AxTree { root, count: 0 }
}

/// A bare-minimum `Platform` that overrides nothing — every optional method
/// falls through to the default (erroring) implementation.
pub(crate) struct BareMinPlatform;
impl Platform for BareMinPlatform {
    fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
        Ok(WindowGeometry::default())
    }
    fn stop_app(&mut self) -> Result<()> {
        Ok(())
    }
    fn capture_frame(&mut self, _region: Option<&crate::frame::Region>) -> Result<Frame> {
        Err(GlassError::CaptureFailed("bare".into()))
    }
    fn send_pointer(&mut self, _event: &PointerEvent) -> Result<()> {
        Ok(())
    }
    fn send_key(&mut self, _event: &KeyEvent) -> Result<()> {
        Ok(())
    }
    fn window(&mut self, _op: &WindowOp) -> Result<WindowGeometry> {
        Ok(WindowGeometry::default())
    }
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        Ok(vec![])
    }
    fn select_window(&mut self, _id: WindowId) -> Result<WindowGeometry> {
        Err(GlassError::WindowNotFound)
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        vec![]
    }
}

/// Records `"action:ok"` for each actuation the seam reports.
#[derive(Clone, Default)]
pub(crate) struct RecordingSink(pub(crate) Arc<Mutex<Vec<String>>>);
impl AuditSink for RecordingSink {
    fn record(&self, act: &Actuation, _ctx: &ActuationContext, o: &AuditOutcome, _d: Duration) {
        let action = match act {
            Actuation::Launch { .. } => "launch",
            Actuation::Stop => "stop",
            Actuation::Pointer { event } => match event {
                PointerEvent::Move { .. } => "move",
                PointerEvent::Click { .. } => "click",
                PointerEvent::Drag { .. } => "drag",
                PointerEvent::Scroll { .. } => "scroll",
                PointerEvent::Gesture { .. } => "gesture",
            },
            Actuation::Key { event } => match event {
                KeyEvent::Text(_) => "type",
                KeyEvent::Chord(_) => "key",
            },
            Actuation::ClipboardSet { .. } => "clipboard_set",
            Actuation::Window { .. } => "window",
            Actuation::ClickElement { .. } => "click_element",
            Actuation::SetValue { .. } => "set_value",
        };
        self.0.lock().unwrap().push(format!("{action}:{}", o.ok));
    }
}

pub(crate) fn first_button(t: &AxTree) -> AxNodeId {
    fn walk(n: &AxNode) -> Option<AxNodeId> {
        if n.role == AxRole::Button {
            return Some(n.id);
        }
        n.children.iter().find_map(walk)
    }
    walk(&t.root).expect("fake_tree has a Button")
}
