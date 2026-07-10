//! The `Glass` session manager. The type, its state, and shared helpers live here;
//! its operations are grouped into submodules (each adds an `impl Glass` block).

use crate::accessibility::{
    element_match, Accessibility, AxContext, AxNode, AxNodeId, AxRole, AxTarget, AxTree,
    ElementCondition, ElementInfo, ElementMatch,
};
use crate::baseline::BaselineStore;
use crate::diff::{diff, diff_perceptual, region_satisfied, BBox, DiffResult, RegionUntil};
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

/// First node of `role` in pre-order, or `None`.
fn find_role(node: &AxNode, role: AxRole) -> Option<&AxNode> {
    if node.role == role {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_role(c, role))
}

fn rect_center(r: &crate::accessibility::AxRect) -> (i64, i64) {
    (
        r.x as i64 + r.width as i64 / 2,
        r.y as i64 + r.height as i64 / 2,
    )
}

/// The ComboBox nearest `target` bounds — disambiguates when several combos exist,
/// since ids don't survive a re-snapshot. Falls back to the first ComboBox when
/// bounds are unknown (single-combo apps, the common case).
fn find_combo_near<'a>(
    root: &'a AxNode,
    target: Option<&crate::accessibility::AxRect>,
) -> Option<&'a AxNode> {
    let Some(t) = target else {
        return find_role(root, AxRole::ComboBox);
    };
    let (tx, ty) = rect_center(t);
    fn walk<'a>(node: &'a AxNode, tx: i64, ty: i64, best: &mut Option<(&'a AxNode, i64)>) {
        if node.role == AxRole::ComboBox {
            if let Some(b) = &node.bounds {
                let (cx, cy) = rect_center(b);
                let d = (cx - tx).pow(2) + (cy - ty).pow(2);
                if best.is_none_or(|(_, bd)| d < bd) {
                    *best = Some((node, d));
                }
            }
        }
        for c in &node.children {
            walk(c, tx, ty, best);
        }
    }
    let mut best = None;
    walk(root, tx, ty, &mut best);
    best.map(|(n, _)| n)
        .or_else(|| find_role(root, AxRole::ComboBox))
}

/// The open (expanded) ComboBox, if any — disambiguates the one whose popup is up.
fn find_expanded_combo(node: &AxNode) -> Option<&AxNode> {
    if node.role == AxRole::ComboBox && node.states.expanded {
        return Some(node);
    }
    node.children.iter().find_map(find_expanded_combo)
}

/// A combo's option rows, in order, as `(label, is_selected)`. An open dropdown
/// realizes its options as `ListItem`s, each carrying its text on a nested label.
fn collect_combo_options(combo: &AxNode) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    collect_list_items(combo, &mut out);
    out
}

fn collect_list_items(node: &AxNode, out: &mut Vec<(String, bool)>) {
    if node.role == AxRole::ListItem {
        if let Some(label) = first_label(node) {
            out.push((label, node.states.selected));
        }
        return; // an item's text is a leaf; don't descend for nested items
    }
    for c in &node.children {
        collect_list_items(c, out);
    }
}

/// First non-empty accessible name in this subtree (an option's text lives on a
/// nested label, not the `ListItem` itself).
fn first_label(node: &AxNode) -> Option<String> {
    if let Some(n) = &node.name {
        if !n.is_empty() {
            return Some(n.clone());
        }
    }
    node.children.iter().find_map(first_label)
}

/// The non-active window (from `windows`) whose screen rect contains the projected
/// screen center of `bounds` (an element's window-relative bounds within the active
/// window). Recovers the case where an element's a11y bounds are reported relative to
/// the active window but the element actually renders in a separate popover window
/// (e.g. an open dropdown's option list) — headless a11y backends don't always report
/// bounds relative to the popover's own origin. `None` when no non-active window
/// contains the point; the smallest-area match wins when several do (an outer window
/// fully behind/around a smaller popover shouldn't shadow it). If several windows tie
/// on area, the first one in `windows`' order wins (`min_by_key` keeps the first
/// minimum) — i.e. whatever order the platform's `list_windows` enumerated them in;
/// this doesn't matter in practice since same-area overlapping windows aren't a shape
/// any backend produces.
///
/// Known best-effort limitation: this detection is purely geometric — it has no way to
/// tell "the app's own popover" apart from an unrelated second top-level window of the
/// same app that happens to overlap the element's projected point. The
/// `menu_container_bounds` size-matching gate below guards against that residual case:
/// a genuinely non-popover window is very unlikely to *also* have an ancestor whose size
/// coincidentally matches its own within tolerance, so the common outcome of a
/// mis-detection is a clear `AxElementInUnmappedPopover` error, not a silent click into
/// the wrong window.
fn owning_popover(
    bounds: crate::accessibility::AxRect,
    active: &WindowGeometry,
    windows: &[WindowInfo],
) -> Option<WindowId> {
    let screen_x = active.x + bounds.x + bounds.width as i32 / 2;
    let screen_y = active.y + bounds.y + bounds.height as i32 / 2;
    windows
        .iter()
        .filter(|w| !w.active)
        .filter(|w| {
            let g = &w.geometry;
            screen_x >= g.x
                && screen_x < g.x + g.width as i32
                && screen_y >= g.y
                && screen_y < g.y + g.height as i32
        })
        .min_by_key(|w| w.geometry.width as u64 * w.geometry.height as u64)
        .map(|w| w.id)
}

/// Path of nodes from `root` to `target` (inclusive of both ends), in that order —
/// `None` if `target` isn't in this tree.
fn ancestor_path(root: &AxNode, target: AxNodeId) -> Option<Vec<&AxNode>> {
    if root.id == target {
        return Some(vec![root]);
    }
    for child in &root.children {
        if let Some(mut path) = ancestor_path(child, target) {
            path.insert(0, root);
            return Some(path);
        }
    }
    None
}

/// The bounds of the ancestor of `target` whose size most closely matches `popover`'s
/// window size (within 16px tolerance on each dimension) — the element's realized
/// menu/list container, e.g. a dropdown popup's `List`. Its origin recovers the
/// popover-relative offset of elements inside it, since their own reported bounds are
/// skewed relative to the *active* window rather than the popover. `None` if no
/// ancestor's bounds match (or `target` isn't in `root`'s tree).
///
/// A real widget tree nests the menu container inside several layout wrapper groups
/// (padding/scroll containers) whose bounds are *also* within tolerance of the
/// popover's size — so the nearest matching ancestor to `target` is often one of those
/// wrappers, not the container itself. Scoring every matching ancestor by closeness to
/// the popover's exact size (not proximity to `target`) picks the real container: it
/// tracks the popover's size most tightly, while wrappers trimmed by padding/scrollbars
/// drift further from it. Ties (equal score) break toward the shallower ancestor — the
/// one closer to `root` — since `ancestor_path` walks root-to-target and `min_by_key`
/// keeps the first minimum; in practice two ancestors matching to the exact same pixel
/// is vanishingly rare (padding/scrollbar trims almost always differ by at least 1px).
fn menu_container_bounds(
    root: &AxNode,
    target: AxNodeId,
    popover: &WindowGeometry,
) -> Option<crate::accessibility::AxRect> {
    let path = ancestor_path(root, target)?;
    path.iter()
        .filter_map(|node| {
            let b = node.bounds?;
            let dw = (b.width as i32 - popover.width as i32).abs();
            let dh = (b.height as i32 - popover.height as i32).abs();
            (dw <= 16 && dh <= 16).then_some((b, dw + dh))
        })
        .min_by_key(|&(_, score)| score)
        .map(|(b, _)| b)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessibility::{AxNode, AxRect, AxRole, AxStates, AxTarget, ElementCondition};
    use crate::audit::{Actuation, ActuationContext, AuditOutcome, AuditSink};
    use crate::platform::{SandboxLevel, Segment};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Every `capture_window(id, region)` call `FakePlatform` recorded, for
    /// asserting it (not `capture_frame`) was used, and with what arguments.
    type CaptureWindowLog = Arc<Mutex<Vec<(WindowId, Option<Region>)>>>;

    /// Scriptable in-memory backend for testing the session manager.
    #[derive(Default)]
    struct FakePlatform {
        geometry: WindowGeometry,
        frames: VecDeque<Frame>,
        pending_logs: Vec<(Stream, String)>,
        pointer_events: Vec<PointerEvent>,
        key_events: Vec<KeyEvent>,
        started: bool,
        capture_log: Arc<Mutex<Vec<Option<Region>>>>,
        click_log: Arc<Mutex<Vec<(i32, i32)>>>,
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
        fn new(width: u32, height: u32) -> Self {
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
        fn with_frames(mut self, frames: Vec<Frame>) -> Self {
            self.frames = frames.into();
            self
        }
        fn with_capture_log(mut self, log: Arc<Mutex<Vec<Option<Region>>>>) -> Self {
            self.capture_log = log;
            self
        }
        fn with_click_log(mut self, log: Arc<Mutex<Vec<(i32, i32)>>>) -> Self {
            self.click_log = log;
            self
        }
        fn with_select_log(mut self, log: Arc<Mutex<Vec<WindowId>>>) -> Self {
            self.select_log = log;
            self
        }
        fn counting_stops(mut self, c: Arc<Mutex<u32>>) -> Self {
            self.stop_count = Some(c);
            self
        }
        fn with_logs(mut self, logs: Vec<(Stream, &str)>) -> Self {
            self.pending_logs = logs.into_iter().map(|(s, t)| (s, t.to_string())).collect();
            self
        }
        fn with_windows(mut self, windows: Vec<WindowInfo>) -> Self {
            self.windows = windows;
            self
        }
        fn with_window_frame(mut self, id: WindowId, frame: Frame) -> Self {
            self.window_frames.insert(id, frame);
            self
        }
        fn with_capture_window_log(mut self, log: CaptureWindowLog) -> Self {
            self.capture_window_log = log;
            self
        }
        fn with_failing_list_windows(mut self) -> Self {
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
    struct FakeAccessibility {
        tree: AxTree,
        set_log: std::sync::Arc<std::sync::Mutex<Vec<(AxTarget, String)>>>,
        set_fail: bool,
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
    fn fake_tree() -> AxTree {
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
    fn fake_tree_enabled() -> AxTree {
        let mut t = fake_tree();
        t.root.children[0].states = AxStates {
            enabled: true,
            ..Default::default()
        };
        t
    }

    fn window_info(id: u64, geometry: WindowGeometry, active: bool) -> WindowInfo {
        WindowInfo {
            id: WindowId(id),
            title: None,
            class: None,
            geometry,
            active,
        }
    }

    #[test]
    fn owning_popover_none_when_element_only_in_active_window() {
        let active = WindowGeometry {
            x: 0,
            y: 0,
            width: 340,
            height: 300,
        };
        let bounds = AxRect {
            x: 50,
            y: 50,
            width: 20,
            height: 20,
        };
        let windows = vec![window_info(1, active.clone(), true)];
        assert_eq!(owning_popover(bounds, &active, &windows), None);
    }

    #[test]
    fn owning_popover_finds_containing_non_active_window() {
        // Validated numbers from the real Xvfb spike: an open GtkDropDown's popover
        // window at (-3,220,326,135); the option row "Globex" has a11y bounds (20,248).
        let active = WindowGeometry {
            x: 0,
            y: 0,
            width: 340,
            height: 300,
        };
        let bounds = AxRect {
            x: 20,
            y: 248,
            width: 80,
            height: 27,
        };
        let popover_geo = WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        };
        let windows = vec![
            window_info(1, active.clone(), true),
            window_info(2, popover_geo, false),
        ];
        assert_eq!(owning_popover(bounds, &active, &windows), Some(WindowId(2)));
    }

    #[test]
    fn owning_popover_picks_smallest_area_when_multiple_contain_the_point() {
        let active = WindowGeometry {
            x: 0,
            y: 0,
            width: 340,
            height: 300,
        };
        // Zero-size bounds project exactly to (50,50) — both candidate windows below
        // contain that point.
        let bounds = AxRect {
            x: 50,
            y: 50,
            width: 0,
            height: 0,
        };
        let big = WindowGeometry {
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        };
        let small = WindowGeometry {
            x: 40,
            y: 40,
            width: 20,
            height: 20,
        };
        let windows = vec![
            window_info(1, active.clone(), true),
            window_info(2, big, false),
            window_info(3, small, false),
        ];
        assert_eq!(
            owning_popover(bounds, &active, &windows),
            Some(WindowId(3)),
            "the smallest containing window should win"
        );
    }

    fn ax_node(id: u32, role: AxRole, bounds: Option<AxRect>, children: Vec<AxNode>) -> AxNode {
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

    #[test]
    fn menu_container_bounds_finds_the_list_sized_ancestor() {
        // Target nested under a `List` node sized like the popover window.
        let list_bounds = AxRect {
            x: 0,
            y: 194,
            width: 326,
            height: 129,
        };
        let target = ax_node(
            2,
            AxRole::ListItem,
            Some(AxRect {
                x: 20,
                y: 248,
                width: 80,
                height: 27,
            }),
            vec![],
        );
        let list = ax_node(1, AxRole::List, Some(list_bounds), vec![target]);
        let root = ax_node(
            0,
            AxRole::Window,
            Some(AxRect {
                x: 0,
                y: 0,
                width: 340,
                height: 300,
            }),
            vec![list],
        );
        let popover = WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        };
        assert_eq!(
            menu_container_bounds(&root, AxNodeId(2), &popover),
            Some(list_bounds)
        );
    }

    #[test]
    fn menu_container_bounds_none_without_a_matching_ancestor() {
        // No `List` container this time — target hangs directly off root, and root's
        // own bounds don't match the popover's size.
        let target = ax_node(
            1,
            AxRole::ListItem,
            Some(AxRect {
                x: 20,
                y: 248,
                width: 80,
                height: 27,
            }),
            vec![],
        );
        let root = ax_node(
            0,
            AxRole::Window,
            Some(AxRect {
                x: 0,
                y: 0,
                width: 340,
                height: 300,
            }),
            vec![target],
        );
        let popover = WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        };
        assert_eq!(menu_container_bounds(&root, AxNodeId(1), &popover), None);
    }

    #[test]
    fn menu_container_bounds_prefers_closest_size_over_nearest_ancestor() {
        // Reproduces the real GTK4 widget tree (captured from the Xvfb spike): several
        // layout wrapper `Group`s sit between the option row and the actual menu `List`,
        // and their bounds *also* fall within the 16px tolerance of the popover's size —
        // so picking the ancestor NEAREST `target` returns a wrapper Group, not the real
        // container. The real container (List, id 2) must win because its size is
        // closest to the popover's, even though it's farther up the chain.
        let popover = WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        };
        let container_bounds = AxRect {
            x: 0,
            y: 194,
            width: 326,
            height: 129,
        };
        let target = ax_node(
            6,
            AxRole::ListItem,
            Some(AxRect {
                x: 20,
                y: 248,
                width: 302,
                height: 35,
            }),
            vec![],
        );
        let inner_list = ax_node(
            5,
            AxRole::List,
            Some(AxRect {
                x: 12,
                y: 205,
                width: 302,
                height: 105,
            }),
            vec![target],
        );
        let group3 = ax_node(
            4,
            AxRole::Group,
            Some(AxRect {
                x: 4,
                y: 197,
                width: 318,
                height: 121,
            }),
            vec![inner_list],
        );
        let group2 = ax_node(
            3,
            AxRole::Group,
            Some(AxRect {
                x: 4,
                y: 197,
                width: 318,
                height: 121,
            }),
            vec![group3],
        );
        let group1 = ax_node(
            2,
            AxRole::Group,
            Some(AxRect {
                x: 4,
                y: 197,
                width: 320,
                height: 123,
            }),
            vec![group2],
        );
        let container = ax_node(1, AxRole::List, Some(container_bounds), vec![group1]);
        let root = ax_node(
            0,
            AxRole::ComboBox,
            Some(AxRect {
                x: 0,
                y: 188,
                width: 320,
                height: 34,
            }),
            vec![container],
        );
        assert_eq!(
            menu_container_bounds(&root, AxNodeId(6), &popover),
            Some(container_bounds),
            "the real container (closest in size to the popover) must win over nearer wrapper groups"
        );
    }

    #[test]
    fn menu_container_bounds_prefers_content_container_over_window_root_sized_ancestor() {
        // Disambiguates the two kinds of ancestor that both commonly fall within
        // tolerance of the popover's size: an outer node sized like the popover
        // window's own frame (e.g. the toplevel root, a few px *larger* — decorations/
        // margins), and the inner content container a few px *smaller* (the real
        // GTK4 shape: a `List` a little inside the window's own bounds). Both are
        // "near" the popover size, so this proves the scoring picks whichever is
        // numerically closest — the content container — not whichever is outermost.
        let popover = WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        };
        let content_bounds = AxRect {
            x: 2,
            y: 222,
            width: 322,  // 4px narrower than the popover
            height: 132, // 3px shorter than the popover
        };
        let target = ax_node(
            2,
            AxRole::ListItem,
            Some(AxRect {
                x: 20,
                y: 248,
                width: 80,
                height: 27,
            }),
            vec![],
        );
        let content = ax_node(1, AxRole::List, Some(content_bounds), vec![target]);
        let root = ax_node(
            0,
            AxRole::Window,
            Some(AxRect {
                x: -3,
                y: 220,
                width: 338,  // 12px wider than the popover (outer window-root frame)
                height: 145, // 10px taller than the popover
            }),
            vec![content],
        );
        assert_eq!(
            menu_container_bounds(&root, AxNodeId(2), &popover),
            Some(content_bounds),
            "both root and content are within tolerance, but content is numerically \
             closest to the popover's size and must win over the outer window root"
        );
    }

    fn glass_with(platform: FakePlatform) -> Glass {
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

    fn glass_with_a11y(platform: FakePlatform, tree: AxTree) -> Glass {
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

    fn spec() -> AppSpec {
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

    #[test]
    fn screenshot_returns_backend_frame() {
        let frame = Frame::solid(4, 4, [7, 7, 7, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![frame.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert_eq!(g.screenshot(None, None).unwrap(), frame);
    }

    #[test]
    fn pointer_out_of_bounds_is_rejected_before_backend() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        let err = g.pointer(&PointerEvent::Click {
            x: 10, // valid range is 0..=9
            y: 5,
            button: crate::platform::MouseButton::Left,
            count: 1,
            modifiers: vec![],
        });
        assert!(matches!(
            err.unwrap_err(),
            GlassError::CoordOutOfBounds { .. }
        ));
    }

    #[test]
    fn gesture_out_of_bounds_segment_is_rejected() {
        let mut g = glass_with(FakePlatform::new(100, 80));
        g.start(&spec()).unwrap();
        let ev = PointerEvent::Gesture {
            pointers: vec![
                Segment {
                    from_x: 10,
                    from_y: 10,
                    to_x: 20,
                    to_y: 20,
                },
                Segment {
                    from_x: 10,
                    from_y: 10,
                    to_x: 200,
                    to_y: 20,
                }, // to_x out of 100-wide window
            ],
            duration_ms: 100,
        };
        assert!(matches!(
            g.pointer(&ev),
            Err(GlassError::CoordOutOfBounds { .. })
        ));
    }

    #[test]
    fn window_resize_updates_tracked_geometry() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        let geom = g
            .window(&WindowOp::Resize {
                width: 20,
                height: 30,
            })
            .unwrap();
        assert_eq!(geom.width, 20);
        assert_eq!(geom.height, 30);
        assert_eq!(g.geometry().unwrap().width, 20);
    }

    #[test]
    fn wait_stable_settles_on_repeated_frame() {
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        // a, b, then b repeats forever (FakePlatform repeats the last frame).
        let platform = FakePlatform::new(2, 2).with_frames(vec![a, b.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(outcome.frame, b);
    }

    #[test]
    fn wait_stable_times_out_when_never_settling() {
        // Two alternating frames that never repeat -> never stable.
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [1, 1, 1, 255]);
        let mut frames = Vec::new();
        for _ in 0..50 {
            frames.push(a.clone());
            frames.push(b.clone());
        }
        let platform = FakePlatform::new(2, 2).with_frames(frames);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 5,
                tolerance: 0,
                timeout_ms: 0, // give up after the first non-settling capture
                stability_region: None,
                window: None,
            })
            .unwrap();
        assert!(!outcome.settled);
    }

    fn frame_4x4_corner(corner: [u8; 4]) -> Frame {
        // 4x4 opaque black, with only pixel (3,3) set to `corner`.
        let mut px = vec![0u8; 4 * 4 * 4];
        for i in 0..16 {
            px[i * 4 + 3] = 255; // alpha
        }
        let idx = (3 * 4 + 3) * 4;
        px[idx..idx + 4].copy_from_slice(&corner);
        Frame::new(4, 4, px).unwrap()
    }

    #[test]
    fn wait_stable_settles_using_only_the_stability_region() {
        // The 2x2 top-left region is constant black; only pixel (3,3) changes,
        // so the FULL frames all differ. Settling can only happen if the settle
        // decision looks at the region alone — and the returned frame is full.
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![f0, f1, f2.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(Region {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                window: None,
            })
            .unwrap();
        assert!(
            outcome.settled,
            "constant region should settle despite the changing corner"
        );
        assert_eq!(
            outcome.frame, f2,
            "wait_stable returns the FULL frame, not the cropped region"
        );
    }

    #[test]
    fn wait_stable_polls_only_the_region_and_captures_full_once() {
        // Region constant, corner changing -> settles on the region; the returned
        // frame is a full capture, and every poll captured ONLY the region.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1, f2])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(region),
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(
            (outcome.frame.width, outcome.frame.height),
            (4, 4),
            "returns the full window"
        );
        let calls = log.lock().unwrap();
        let (last, polls) = calls.split_last().expect("at least one capture");
        assert!(
            polls.iter().all(|c| *c == Some(region)),
            "polls capture only the region: {polls:?}"
        );
        assert_eq!(*last, None, "final capture is the full window");
    }

    #[test]
    fn wait_stable_without_region_captures_full_each_poll() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform = FakePlatform::new(2, 2)
            .with_frames(vec![a, b])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().all(|c| c.is_none()),
            "no-region captures are full: {calls:?}"
        );
    }

    #[test]
    fn wait_stable_rejects_out_of_bounds_stability_region() {
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let err = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(Region {
                    x: 0,
                    y: 0,
                    width: 99,
                    height: 1,
                }),
                window: None,
            })
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn wait_stable_with_window_id_uses_capture_window_and_leaves_active_untouched() {
        // Window B is constant, so it settles immediately; watching it must go
        // through capture_window (never capture_frame), and must not disturb the
        // active window (A).
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 100,
                y: 0,
                width: 4,
                height: 4,
            },
            active: false,
        };
        let frame_b = Frame::solid(4, 4, [3, 3, 3, 255]);
        let capture_log = Arc::new(Mutex::new(Vec::new()));
        let capture_window_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_windows(vec![a.clone(), b])
            .with_capture_log(capture_log.clone())
            .with_capture_window_log(capture_window_log.clone())
            .with_window_frame(WindowId(2), frame_b.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.select_window(WindowId(1)).unwrap(); // A is active

        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                window: Some(WindowId(2)),
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(outcome.frame, frame_b);
        assert_eq!(
            g.geometry().unwrap(),
            a.geometry,
            "active window is still A after watching B"
        );
        assert!(
            capture_log.lock().unwrap().is_empty(),
            "watching a specific window must not go through capture_frame"
        );
        assert!(!capture_window_log.lock().unwrap().is_empty());
    }

    #[test]
    fn screenshot_with_region_returns_subrectangle() {
        let frame = Frame::solid(4, 4, [7, 7, 7, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![frame]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let out = g
            .screenshot(
                Some(Region {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                }),
                None,
            )
            .unwrap();
        assert_eq!((out.width, out.height), (2, 2));
    }

    #[test]
    fn screenshot_region_out_of_bounds_is_rejected() {
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let err = g
            .screenshot(
                Some(Region {
                    x: 0,
                    y: 0,
                    width: 9,
                    height: 1,
                }),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn screenshot_with_window_id_captures_that_window_without_changing_active() {
        // Two windows: A (active) and B. screenshot(None, Some(B.id)) must return
        // B's frame — via capture_window, NOT capture_frame — while the session's
        // active window (still A) is left untouched.
        let frame_b = Frame::solid(8, 8, [9, 9, 9, 255]);
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 100,
                y: 0,
                width: 8,
                height: 8,
            },
            active: false,
        };
        let capture_log = Arc::new(Mutex::new(Vec::new()));
        let capture_window_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_windows(vec![a.clone(), b.clone()])
            .with_capture_log(capture_log.clone())
            .with_capture_window_log(capture_window_log.clone())
            .with_window_frame(WindowId(2), frame_b.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.select_window(WindowId(1)).unwrap(); // A is active

        let out = g.screenshot(None, Some(WindowId(2))).unwrap();
        assert_eq!(out, frame_b, "screenshot(window: B) returns B's frame");
        assert_eq!(
            g.geometry().unwrap(),
            a.geometry,
            "active window is still A after capturing B"
        );
        assert!(
            capture_log.lock().unwrap().is_empty(),
            "capturing a specific window must not go through capture_frame"
        );
        assert_eq!(
            *capture_window_log.lock().unwrap(),
            vec![(WindowId(2), None)]
        );
    }

    #[test]
    fn screenshot_with_unknown_window_id_errors() {
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.screenshot(None, Some(WindowId(999))).unwrap_err(),
            GlassError::WindowNotFound
        ));
    }

    #[test]
    fn save_then_diff_baseline_reports_change() {
        let baseline_frame = Frame::solid(2, 2, [0, 0, 0, 255]);
        let mut changed = baseline_frame.clone();
        changed.pixels[0] = 255;
        // capture #1 -> save baseline; capture #2 -> diff against it.
        let platform = FakePlatform::new(2, 2).with_frames(vec![baseline_frame.clone(), changed]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("main").unwrap();
        let result = g.diff_baseline("main", None, 0).unwrap();
        assert_eq!(result.changed_pixels, 1);
    }

    #[test]
    fn diff_missing_baseline_errors() {
        let platform =
            FakePlatform::new(2, 2).with_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.diff_baseline("absent", None, 0).unwrap_err(),
            GlassError::BaselineMissing(_)
        ));
    }

    #[test]
    fn diff_region_scopes_comparison_to_subrectangle() {
        // A single whole baseline is compared against several sub-regions: the
        // baseline is stored whole and cropped per-call, so both operands always
        // cover the same rectangle.
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[(3 * 4 + 3) * 4] = 255; // pixel (3,3)
        let platform = FakePlatform::new(4, 4).with_frames(vec![base, changed]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("m").unwrap();
        let top_left = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let bottom_right = Region {
            x: 2,
            y: 2,
            width: 2,
            height: 2,
        };
        // Region excludes the changed pixel -> no change.
        assert_eq!(
            g.diff_baseline("m", Some(&top_left), 0)
                .unwrap()
                .changed_pixels,
            0
        );
        // Region includes the changed pixel -> sees exactly it.
        assert_eq!(
            g.diff_baseline("m", Some(&bottom_right), 0)
                .unwrap()
                .changed_pixels,
            1
        );
        // Whole-frame diff still sees it.
        assert_eq!(g.diff_baseline("m", None, 0).unwrap().changed_pixels, 1);
    }

    #[test]
    fn diff_region_out_of_bounds_is_rejected() {
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![base.clone(), base]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("m").unwrap();
        let err = g
            .diff_baseline(
                "m",
                Some(&Region {
                    x: 0,
                    y: 0,
                    width: 9,
                    height: 1,
                }),
                0,
            )
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn glass_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Glass>();
    }

    /// Build a `Glass` over a custom factory (for backend-routing tests).
    fn glass_with_factory(factory: PlatformFactory) -> Glass {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
    }

    #[test]
    fn shutdown_runs_the_hook() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let mut g =
            glass_with_factory(Box::new(|_b| Err(GlassError::Backend("no backend".into()))));
        g.set_shutdown_hook(Box::new(move || f.store(true, Ordering::SeqCst)));
        g.shutdown();
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
    fn select_window_switches_active_geometry() {
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 320,
                height: 240,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 400,
                y: 0,
                width: 100,
                height: 80,
            },
            active: false,
        };
        let mut glass = glass_with(FakePlatform::new(320, 240).with_windows(vec![a, b]));
        glass.start(&spec()).unwrap();

        let listed = glass.list_windows().unwrap();
        assert_eq!(listed.len(), 2);

        let geo = glass.select_window(WindowId(2)).unwrap();
        assert_eq!((geo.width, geo.height), (100, 80));
        assert_eq!(glass.geometry().unwrap().width, 100);

        assert!(matches!(
            glass.select_window(WindowId(999)),
            Err(GlassError::WindowNotFound)
        ));
    }

    #[test]
    fn a11y_snapshot_assigns_ids_and_counts() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot().unwrap();
        assert_eq!(tree.count, 2);
        assert_eq!(tree.root.id, AxNodeId(0));
        assert_eq!(tree.root.children[0].id, AxNodeId(1));
    }

    #[test]
    fn snapshot_unsupported_without_reader() {
        let mut g = glass_with(FakePlatform::new(40, 30));
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.a11y_snapshot().unwrap_err(),
            GlassError::AxUnsupported
        ));
    }

    #[test]
    fn click_element_clicks_center_via_pointer_path() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        g.click_element(AxNodeId(1)).unwrap();
        // The Button at (10,10 20x20) → center (20,20), via the normal pointer path.
        assert_eq!(clicks.lock().unwrap().last().copied(), Some((20, 20)));
    }

    #[test]
    fn click_element_without_snapshot_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.click_element(AxNodeId(1)).unwrap_err(),
            GlassError::NoAxSnapshot
        ));
    }

    #[test]
    fn click_element_unknown_id_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        assert!(matches!(
            g.click_element(AxNodeId(99)).unwrap_err(),
            GlassError::AxElementNotFound(99)
        ));
    }

    #[test]
    fn a11y_marks_overlays_and_legends() {
        let platform =
            FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        let (frame, marks) = g.a11y_marks().unwrap();
        // The Button (id 1) is marked; its outline corner is magenta.
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].id, AxNodeId(1));
        let i = (10usize * 100 + 10) * 4;
        assert_eq!(&frame.pixels[i..i + 4], &[255, 0, 255, 255]);
        // The snapshot was cached, so a mark is clickable by id via the normal path.
        g.click_element(AxNodeId(1)).unwrap();
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
        g.shutdown();
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
        g.shutdown();
        assert_eq!(
            *stops.lock().unwrap(),
            1,
            "no extra stop_app on an empty shutdown"
        );
    }

    #[test]
    fn shutdown_without_active_session_is_noop() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.shutdown(); // must not panic and must not error
    }

    #[test]
    fn click_element_without_bounds_errors() {
        let mut tree = fake_tree();
        tree.root.children.push(AxNode {
            id: AxNodeId(0),
            role: AxRole::Label,
            raw_role: "label".into(),
            name: Some("nobounds".into()),
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        });
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), tree);
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        // node #2 is the boundless Label.
        assert!(matches!(
            g.click_element(AxNodeId(2)).unwrap_err(),
            GlassError::AxElementNotClickable(2)
        ));
    }

    /// Builds the tree used by the popover-routing tests: root Window > `List`
    /// (sized like the popover window) > `ListItem` "Globex" (the click target),
    /// with the validated real-Xvfb numbers (see `owning_popover`/
    /// `menu_container_bounds` unit tests above).
    fn fake_tree_with_popover_option() -> AxTree {
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

    #[test]
    fn click_element_without_popover_clicks_clamped_center_and_never_selects_a_window() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let a = window_info(
            1,
            WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            true,
        );
        // A non-active window that does NOT contain the Button's projected center —
        // present so `list_windows` isn't trivially empty, still no routing occurs.
        let b = window_info(
            2,
            WindowGeometry {
                x: 1000,
                y: 1000,
                width: 50,
                height: 50,
            },
            false,
        );
        let platform = FakePlatform::new(100, 100)
            .with_windows(vec![a, b])
            .with_click_log(clicks.clone())
            .with_select_log(select_log.clone());
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        g.click_element(AxNodeId(1)).unwrap(); // the Button at (10,10 20x20)
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            Some((20, 20)),
            "unrouted click still lands on the element's own clamped center"
        );
        assert!(
            select_log.lock().unwrap().is_empty(),
            "no popover routing means no select_window call"
        );
    }

    #[test]
    fn click_element_survives_a_failing_list_windows_and_clicks_normally() {
        // The popover-routing probe (`list_windows`) is best-effort: if the backend's
        // enumeration errors, an ordinary click must still succeed via the unchanged
        // `clamped_center` path rather than propagating the enumeration failure.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100)
            .with_click_log(clicks.clone())
            .with_select_log(select_log.clone())
            .with_failing_list_windows();
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        g.click_element(AxNodeId(1))
            .expect("a failing list_windows must not block an ordinary click");
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            Some((20, 20)),
            "click still lands on the element's own clamped center"
        );
        assert!(
            select_log.lock().unwrap().is_empty(),
            "no popover routing was attempted since the probe's result was treated as empty"
        );
    }

    #[test]
    fn click_element_routes_into_owning_popover_and_restores_active_window() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let a = window_info(
            1,
            WindowGeometry {
                x: 0,
                y: 0,
                width: 340,
                height: 300,
            },
            true,
        );
        let b = window_info(
            2,
            WindowGeometry {
                x: -3,
                y: 220,
                width: 326,
                height: 135,
            },
            false,
        );
        let platform = FakePlatform::new(340, 300)
            .with_windows(vec![a, b])
            .with_click_log(clicks.clone())
            .with_select_log(select_log.clone());
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot().unwrap();
        // assign_ids in pre-order: root=0, List=1, Globex(ListItem)=2.
        let globex_id = tree.root.children[0].children[0].id;
        assert_eq!(globex_id, AxNodeId(2));

        g.click_element(globex_id).unwrap();

        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            Some((20, 54)),
            "click lands at (Globex.bounds - List.bounds), per the validated algorithm"
        );
        assert_eq!(
            *select_log.lock().unwrap(),
            vec![WindowId(2), WindowId(1)],
            "selects the popover to click, then restores the previously-active window"
        );
        assert_eq!(
            g.geometry().unwrap().width,
            340,
            "active window geometry is restored after the routed click"
        );
    }

    #[test]
    fn click_element_in_popover_without_a_mappable_container_errors() {
        // Same popover-owning geometry, but the target has no List-sized ancestor to
        // recover a container origin from — must error, not silently mis-click.
        //
        // This also stands in for the residual `owning_popover` false-positive case
        // documented on that function: a normal element whose projected point happens to
        // land inside another real window is indistinguishable, geometrically, from a
        // genuine popover — the size-matching gate is what turns that misdetection into
        // this clear, catchable error instead of a silent click into the wrong window.
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
            children: vec![globex],
        };
        let tree = AxTree { root, count: 0 };
        let a = window_info(
            1,
            WindowGeometry {
                x: 0,
                y: 0,
                width: 340,
                height: 300,
            },
            true,
        );
        let b = window_info(
            2,
            WindowGeometry {
                x: -3,
                y: 220,
                width: 326,
                height: 135,
            },
            false,
        );
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300)
            .with_windows(vec![a, b])
            .with_click_log(clicks.clone())
            .with_select_log(select_log.clone());
        let mut g = glass_with_a11y(platform, tree);
        g.start(&spec()).unwrap();
        let snapshot = g.a11y_snapshot().unwrap();
        let globex_id = snapshot.root.children[0].id;
        assert!(matches!(
            g.click_element(globex_id).unwrap_err(),
            GlassError::AxElementInUnmappedPopover(id) if id == globex_id.0
        ));
        assert!(
            clicks.lock().unwrap().is_empty(),
            "a detection that can't be resolved to a container must never fall back to \
             clicking anywhere — no click of any kind is recorded"
        );
        assert!(
            select_log.lock().unwrap().is_empty(),
            "the candidate window is never selected either — the container gate runs \
             before select_window, so a mis-detection can't even transiently switch focus"
        );
    }

    #[test]
    fn wait_for_element_matches_state_and_returns_node() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                role: Some(AxRole::Button),
                value_contains: None,
                condition: ElementCondition::Enabled,
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        let e = o.element.expect("matched element");
        assert_eq!(e.id, AxNodeId(1));
        assert_eq!(e.name.as_deref(), Some("Save"));
    }

    #[test]
    fn wait_for_element_times_out_soft() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                role: None,
                value_contains: None,
                condition: ElementCondition::Checked, // never true in the fixed tree
                interval_ms: 0,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!o.matched);
        assert!(o.element.is_none());
    }

    #[test]
    fn wait_for_element_disappears_is_matched_when_absent() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                condition: ElementCondition::Disappears,
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        assert!(o.element.is_none());
    }

    #[test]
    fn wait_for_element_errors_when_a11y_unsupported() {
        let mut g = glass_with(FakePlatform::new(40, 30)); // no accessibility reader
        g.start(&spec()).unwrap();
        let err = g
            .wait_for_element(&WaitElementParams {
                name: Some("x".into()),
                role: None,
                value_contains: None,
                condition: ElementCondition::Appears,
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap_err();
        assert!(matches!(err, GlassError::AxUnsupported));
    }

    #[test]
    fn scroll_to_element_returns_already_visible_without_scrolling() {
        // The target is present in the current view → return it immediately, steps=0,
        // and no scroll is issued.
        let platform = FakePlatform::new(100, 100);
        let mut g = glass_with_a11y(platform, fake_tree()); // fake_tree has Button "Save"
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Save".into()),
                role: None,
                value_contains: None,
                direction: ScrollDirection::Down,
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert_eq!(out.steps, 0);
        assert!(!out.reversed);
        assert_eq!(out.element.unwrap().name.as_deref(), Some("Save"));
    }

    #[test]
    fn scroll_to_element_absent_sweeps_both_ends_then_reports_unmatched() {
        // The target never appears and the a11y tree's outline never changes (the
        // fixture tree is fixed), so each direction saturates after one step. The
        // sweep must terminate (not hang), reversed, matched:false.
        let platform = FakePlatform::new(100, 100);
        let mut g = glass_with_a11y(platform, fake_tree()); // no node named "Ghost"
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                direction: ScrollDirection::Down,
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(!out.matched);
        assert!(out.element.is_none());
        assert!(out.reversed, "must have reversed to sweep the other end");
        // One saturating step per direction: no motion breaks each sweep immediately.
        assert_eq!(out.steps, 2);
    }

    #[test]
    fn wait_for_region_changes_matches_on_divergence() {
        // Reference captured at start = black; next frame = white -> "changes".
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform = FakePlatform::new(2, 2).with_frames(vec![a, b]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                window: None,
            })
            .unwrap();
        assert!(o.matched);
        assert!(o.changed_pct > 0.0);
    }

    #[test]
    fn wait_for_region_changes_times_out_when_static() {
        // One frame, repeated -> reference == every poll -> never changes.
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let platform = FakePlatform::new(2, 2).with_frames(vec![a]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 0,
                window: None,
            })
            .unwrap();
        assert!(!o.matched);
    }

    #[test]
    fn wait_for_region_matches_converges_to_baseline() {
        // save baseline from black; then poll white, then black -> "matches" on black.
        let black = Frame::solid(2, 2, [0, 0, 0, 255]);
        let white = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform =
            FakePlatform::new(2, 2).with_frames(vec![black.clone(), white, black.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("b").unwrap(); // consumes frame #1 (black)
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: Some("b".into()),
                region: None,
                until: RegionUntil::Matches,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                window: None,
            })
            .unwrap();
        assert!(o.matched);
        assert_eq!(o.changed_pct, 0.0);
    }

    #[test]
    fn wait_for_region_with_window_id_uses_capture_window_and_leaves_active_untouched() {
        // Window B is constant, so it matches its own initial capture immediately;
        // watching it must go through capture_window (never capture_frame), and
        // must not disturb the active window (A).
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 100,
                y: 0,
                width: 4,
                height: 4,
            },
            active: false,
        };
        let frame_b = Frame::solid(4, 4, [5, 5, 5, 255]);
        let capture_log = Arc::new(Mutex::new(Vec::new()));
        let capture_window_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_windows(vec![a.clone(), b])
            .with_capture_log(capture_log.clone())
            .with_capture_window_log(capture_window_log.clone())
            .with_window_frame(WindowId(2), frame_b);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.select_window(WindowId(1)).unwrap(); // A is active

        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Matches,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                window: Some(WindowId(2)),
            })
            .unwrap();
        assert!(o.matched);
        assert_eq!(o.changed_pct, 0.0);
        assert_eq!(
            g.geometry().unwrap(),
            a.geometry,
            "active window is still A after watching B"
        );
        assert!(
            capture_log.lock().unwrap().is_empty(),
            "watching a specific window must not go through capture_frame"
        );
        assert!(
            capture_window_log.lock().unwrap().len() >= 2,
            "reference capture + at least one poll"
        );
    }

    #[test]
    fn wait_for_log_matches_existing_from_cursor_zero() {
        let platform =
            FakePlatform::new(10, 10).with_logs(vec![(Stream::Stdout, "export complete")]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "complete".into(),
                stream: None,
                cursor: Some(0), // scan from the beginning
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        let line = o.line.expect("matched line");
        assert_eq!(line.text, "export complete");
        assert_eq!(o.cursor, line.seq + 1);
    }

    #[test]
    fn wait_for_log_default_cursor_skips_old_lines_and_times_out() {
        // The line already in the buffer is "old" (before the default start cursor),
        // so a default-cursor wait does not match it.
        let platform = FakePlatform::new(10, 10).with_logs(vec![(Stream::Stdout, "old line")]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "old line".into(),
                stream: None,
                cursor: None, // default = end-at-start
                interval_ms: 0,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!o.matched);
        assert!(o.line.is_none());
        // Footgun guard: the line WAS in the buffer (seq 0) before the default start
        // cursor, so the timeout must say so and point at cursor:0 — not fail silently.
        let note = o
            .note
            .expect("timeout note when the substring was already buffered");
        assert!(
            note.contains("cursor:0"),
            "note should point at cursor:0, got: {note}"
        );
        assert!(
            note.contains("seq 0"),
            "note should cite the buffered seq, got: {note}"
        );
    }

    #[test]
    fn wait_for_log_match_cursor_resumes_after_matched_line() {
        // Two lines; match the FIRST -> resume cursor is just after it (1), not the end (2).
        let platform = FakePlatform::new(10, 10).with_logs(vec![
            (Stream::Stdout, "first hit"),
            (Stream::Stdout, "second"),
        ]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "first".into(),
                stream: None,
                cursor: Some(0),
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        assert_eq!(o.line.unwrap().seq, 0);
        assert_eq!(
            o.cursor, 1,
            "resume cursor is just after the matched line, not the buffer end"
        );
    }

    #[test]
    fn set_value_no_snapshot_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.set_value(AxNodeId(1), "x").unwrap_err(),
            GlassError::NoAxSnapshot
        ));
    }

    #[test]
    fn set_value_unknown_id_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        assert!(matches!(
            g.set_value(AxNodeId(99), "x").unwrap_err(),
            GlassError::AxElementNotFound(99)
        ));
    }

    #[test]
    fn set_value_unsupported_without_reader() {
        let mut g = glass_with(FakePlatform::new(40, 30)); // no accessibility
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.set_value(AxNodeId(0), "x").unwrap_err(),
            GlassError::AxUnsupported
        ));
    }

    #[test]
    fn set_value_passes_target_and_text_to_backend() {
        // Build a Glass whose fake records set_value calls, keeping the Arc to inspect.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let log2 = log.clone();
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(FakePlatform::new(100, 100)),
            accessibility: Some(Box::new(FakeAccessibility {
                tree: fake_tree(),
                set_log: log2,
                set_fail: false,
            })),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap(); // fake_tree: #1 is Button "Save"
        g.set_value(AxNodeId(1), "hello").unwrap();
        let calls = log.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            AxTarget {
                id: AxNodeId(1),
                role: AxRole::Button,
                name: Some("Save".into()),
                bounds: Some(AxRect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 20
                }),
            }
        );
        assert_eq!(calls[0].1, "hello");
    }

    #[test]
    fn set_value_propagates_backend_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(FakePlatform::new(100, 100)),
            accessibility: Some(Box::new(FakeAccessibility {
                tree: fake_tree(),
                set_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                set_fail: true,
            })),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
        assert!(matches!(
            g.set_value(AxNodeId(1), "x").unwrap_err(),
            GlassError::AxElementNotEditable(1)
        ));
    }

    /// A bare-minimum `Platform` that overrides nothing — every optional method
    /// falls through to the default (erroring) implementation.
    struct BareMinPlatform;
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

    #[test]
    fn default_clipboard_is_unsupported() {
        // A Platform impl with no clipboard override returns Unsupported for both
        // get_clipboard and set_clipboard.
        let mut p = BareMinPlatform;
        let get_err = p.get_clipboard().unwrap_err();
        assert!(
            matches!(get_err, GlassError::Unsupported(_)),
            "get_clipboard: {get_err}"
        );
        let set_err = p.set_clipboard("hello").unwrap_err();
        assert!(
            matches!(set_err, GlassError::Unsupported(_)),
            "set_clipboard: {set_err}"
        );
    }

    #[test]
    fn clipboard_set_get_roundtrip() {
        // FakePlatform has an in-memory clipboard; Glass::set_clipboard/get_clipboard
        // are pass-throughs that require an active session.
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        g.set_clipboard("hello glass").unwrap();
        assert_eq!(g.get_clipboard().unwrap(), "hello glass");
        // Overwrite with a new value.
        g.set_clipboard("updated").unwrap();
        assert_eq!(g.get_clipboard().unwrap(), "updated");
    }

    #[test]
    fn clipboard_requires_active_session() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        // No session started — both ops should return NoActiveSession.
        assert!(matches!(
            g.get_clipboard().unwrap_err(),
            GlassError::NoActiveSession
        ));
        assert!(matches!(
            g.set_clipboard("x").unwrap_err(),
            GlassError::NoActiveSession
        ));
    }

    /// Records `"action:ok"` for each actuation the seam reports.
    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<String>>>);
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

    fn first_button(t: &AxTree) -> AxNodeId {
        fn walk(n: &AxNode) -> Option<AxNodeId> {
            if n.role == AxRole::Button {
                return Some(n.id);
            }
            n.children.iter().find_map(walk)
        }
        walk(&t.root).expect("fake_tree has a Button")
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
        let tree = g.a11y_snapshot().unwrap(); // read (populates last_ax)
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

    #[test]
    fn scroll_direction_opposite_and_dy() {
        assert_eq!(ScrollDirection::Down.opposite(), ScrollDirection::Up);
        assert_eq!(ScrollDirection::Up.opposite(), ScrollDirection::Down);
        // Down = wheel-down = positive notches; Up = negative.
        assert_eq!(ScrollDirection::Down.dy(3), 3);
        assert_eq!(ScrollDirection::Up.dy(3), -3);
        // An absurd step saturates instead of overflowing/panicking.
        assert_eq!(ScrollDirection::Down.dy(u32::MAX), i32::MAX);
        assert_eq!(ScrollDirection::Up.dy(u32::MAX), -i32::MAX);
        assert_eq!(
            ScrollDirection::from_name("DOWN"),
            Some(ScrollDirection::Down)
        );
        assert_eq!(ScrollDirection::from_name("up"), Some(ScrollDirection::Up));
        assert_eq!(ScrollDirection::from_name("sideways"), None);
    }
}
