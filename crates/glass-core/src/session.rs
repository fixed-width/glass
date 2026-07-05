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

/// Parameters for [`Glass::wait_stable`].
#[derive(Clone, Debug)]
pub struct WaitStableParams {
    pub interval_ms: u64,
    pub settle_frames: u32,
    pub tolerance: u8,
    pub timeout_ms: u64,
    /// When set, the settle decision compares only this sub-rectangle of each
    /// frame; the returned frame is still the full window.
    pub stability_region: Option<Region>,
    /// When set, watch this window's own region instead of the active window's —
    /// without changing which window is active.
    pub window: Option<WindowId>,
}

/// Outcome of a wait-until-stable: the final frame and whether it settled
/// before the timeout.
#[derive(Clone, Debug)]
pub struct WaitStableOutcome {
    pub frame: Frame,
    pub settled: bool,
    /// Whether any frame-to-frame change was seen while watching. `settled:true` with
    /// `saw_motion:false` over a short `observed_ms` is a *brief* quiet window — a slow
    /// animation can still hide under it, so use `wait_for_region {until:"changes"}` to
    /// positively assert motion. `settled:true` with `saw_motion:true` means it was moving
    /// and then quieted.
    pub saw_motion: bool,
    /// How long (ms) frames were observed before settling or timing out.
    pub observed_ms: u64,
}

/// Parameters for [`Glass::wait_for_element`].
#[derive(Clone, Debug)]
pub struct WaitElementParams {
    pub name: Option<String>,
    pub role: Option<AxRole>,
    pub value_contains: Option<String>,
    pub condition: ElementCondition,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Outcome of [`Glass::wait_for_element`].
#[derive(Clone, Debug)]
pub struct WaitElementOutcome {
    pub matched: bool,
    /// The matched element (absent on timeout, and for a satisfied `disappears`).
    pub element: Option<ElementInfo>,
    /// Wall-clock milliseconds elapsed when the wait returned.
    pub elapsed_ms: u64,
}

/// Parameters for [`Glass::wait_for_region`].
#[derive(Clone, Debug)]
pub struct WaitRegionParams {
    /// Saved baseline to compare against; `None` uses the frame at call start.
    pub baseline: Option<String>,
    /// Window-relative sub-rectangle to watch; `None` watches the whole window.
    pub region: Option<Region>,
    pub until: RegionUntil,
    /// `true` = perceptual diff (use `threshold`); `false` = exact (use `tolerance`).
    pub perceptual: bool,
    pub threshold: f32,
    pub tolerance: u8,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    /// When set, watch this window's own region instead of the active window's —
    /// without changing which window is active.
    pub window: Option<WindowId>,
}

/// Outcome of [`Glass::wait_for_region`]. `frame` is the last captured region
/// (window when no region), for the optional image at the MCP layer.
#[derive(Clone, Debug)]
pub struct WaitRegionOutcome {
    /// Whether the region condition held before the timeout.
    pub matched: bool,
    /// Percent of the watched region that differed from the reference at the last poll.
    pub changed_pct: f32,
    /// Bounding box of the changed area at the last poll (None if nothing changed).
    pub bbox: Option<BBox>,
    /// The last captured region frame (the watched window when no region) — source for the optional image at the tool layer.
    pub frame: Frame,
    /// Wall-clock milliseconds elapsed when the wait returned.
    pub elapsed_ms: u64,
}

/// Parameters for [`Glass::wait_for_log`].
#[derive(Clone, Debug)]
pub struct WaitLogParams {
    /// Substring to wait for (required by the tool layer to be non-empty).
    pub contains: String,
    pub stream: Option<Stream>,
    /// Start scanning from this cursor; `None` = the buffer's end at call start
    /// (so only newly-appended lines count).
    pub cursor: Option<u64>,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Outcome of [`Glass::wait_for_log`].
#[derive(Clone, Debug)]
pub struct WaitLogOutcome {
    pub matched: bool,
    pub line: Option<LogLine>,
    /// Cursor to resume from: just past the matched line, or the buffer end on timeout.
    pub cursor: u64,
    pub elapsed_ms: u64,
    /// Set on a timeout when the substring was already in the buffer *before* this call's
    /// start cursor (the default-cursor footgun: a fast-boot line is otherwise skipped).
    /// Points the caller at `cursor:0` instead of failing silently.
    pub note: Option<String>,
}

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

    /// Validate that any window-relative coordinates in `event` fall inside the
    /// current window.
    fn check_bounds(&self, event: &PointerEvent) -> Result<()> {
        let g = self.require_active()?;
        let (w, h) = (g.geometry.width as i32, g.geometry.height as i32);
        let check = |x: i32, y: i32| -> Result<()> {
            if x < 0 || y < 0 || x >= w || y >= h {
                Err(GlassError::CoordOutOfBounds {
                    x,
                    y,
                    width: g.geometry.width,
                    height: g.geometry.height,
                })
            } else {
                Ok(())
            }
        };
        match *event {
            PointerEvent::Move { x, y } => check(x, y),
            PointerEvent::Click { x, y, .. } => check(x, y),
            PointerEvent::Scroll { x, y, .. } => check(x, y),
            PointerEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            } => {
                check(from_x, from_y)?;
                check(to_x, to_y)
            }
            PointerEvent::Gesture { ref pointers, .. } => {
                for p in pointers {
                    check(p.from_x, p.from_y)?;
                    check(p.to_x, p.to_y)?;
                }
                Ok(())
            }
        }
    }

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
    /// Written to drain the session set so the future multi-session registry (a
    /// `HashMap` instead of this `Option`) reuses it unchanged — it becomes a `for`
    /// loop with no other change.
    pub fn shutdown(&mut self) {
        if let Some(mut s) = self.active.take() {
            let _ = s.platform.stop_app();
            // `s` drops here: the backend (Xvfb/sway/Job) is torn down.
        }
        if let Some(hook) = self.shutdown_hook.take() {
            hook();
        }
    }

    pub fn geometry(&self) -> Result<WindowGeometry> {
        Ok(self.require_active()?.geometry.clone())
    }

    /// Capture the active window, or — when `window` is set — a different
    /// window's region WITHOUT changing which window is active (unlike
    /// `select_window`). `region` is relative to whichever window is captured.
    pub fn screenshot(
        &mut self,
        region: Option<Region>,
        window: Option<WindowId>,
    ) -> Result<Frame> {
        self.capture(window, region.as_ref())
    }

    /// Capture `window`'s region (or, when `None`, the active window's), pumping
    /// logs afterward either way. A specific window's own geometry governs its
    /// capture — the backend validates `id` and any region against it — so the
    /// active window's cached `s.geometry` is only consulted for the `None` case.
    fn capture(&mut self, window: Option<WindowId>, region: Option<&Region>) -> Result<Frame> {
        let s = self.active_mut()?;
        let frame = match window {
            Some(id) => s.platform.capture_window(id, region)?,
            None => {
                if let Some(r) = region {
                    r.check_fits(s.geometry.width, s.geometry.height)?;
                }
                s.platform.capture_frame(region)?
            }
        };
        s.pump();
        Ok(frame)
    }

    pub fn pointer(&mut self, event: &PointerEvent) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.pointer_inner(event);
        self.emit_audit(
            &crate::audit::Actuation::Pointer { event },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn pointer_inner(&mut self, event: &PointerEvent) -> Result<()> {
        self.check_bounds(event)?;
        let s = self.active_mut()?;
        s.platform.send_pointer(event)?;
        s.pump();
        Ok(())
    }

    pub fn key(&mut self, event: &KeyEvent) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.key_inner(event);
        self.emit_audit(
            &crate::audit::Actuation::Key { event },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn key_inner(&mut self, event: &KeyEvent) -> Result<()> {
        let s = self.active_mut()?;
        s.platform.send_key(event)?;
        s.pump();
        Ok(())
    }

    pub fn get_clipboard(&mut self) -> Result<String> {
        self.active_mut()?.platform.get_clipboard()
    }

    pub fn set_clipboard(&mut self, text: &str) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.set_clipboard_inner(text);
        self.emit_audit(
            &crate::audit::Actuation::ClipboardSet { text },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn set_clipboard_inner(&mut self, text: &str) -> Result<()> {
        self.active_mut()?.platform.set_clipboard(text)
    }

    pub fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let t = std::time::Instant::now();
        let result = self.window_inner(op);
        if !matches!(op, WindowOp::Geometry) {
            self.emit_audit(
                &crate::audit::Actuation::Window { op },
                crate::audit::AuditOutcome::from_result(&result),
                t.elapsed(),
            );
        }
        result
    }

    fn window_inner(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let s = self.active_mut()?;
        let geometry = s.platform.window(op)?;
        s.geometry = geometry.clone();
        s.pump();
        Ok(geometry)
    }

    /// All top-level windows of the active app.
    pub fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        self.active_mut()?.platform.list_windows()
    }

    /// Make `id` the active window; subsequent capture/input/window ops target
    /// it. Updates the cached active-window geometry.
    pub fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        let s = self.active_mut()?;
        let geometry = s.platform.select_window(id)?;
        s.geometry = geometry.clone();
        s.active_window = Some(crate::audit::WindowRef {
            id: id.0,
            title: None,
        });
        s.pump();
        Ok(geometry)
    }

    /// Snapshot the active window's accessibility tree (normalized, window-
    /// relative, ids assigned by the core). Caches it for `click_element`.
    /// `AxUnsupported` if the backend has no accessibility reader.
    pub fn a11y_snapshot(&mut self) -> Result<AxTree> {
        let s = self.active_mut()?;
        let pids = s.platform.app_pids();
        let window = s.geometry.clone();
        let window_handle = s.platform.active_window_handle();
        let a11y_bus_addr = s.platform.a11y_bus_addr();
        let acc = s.accessibility.as_mut().ok_or(GlassError::AxUnsupported)?;
        let mut tree = acc.snapshot(&AxContext {
            pids,
            window,
            window_handle,
            a11y_bus_addr,
        })?;
        tree.assign_ids();
        s.last_ax = Some(tree.clone());
        s.pump();
        Ok(tree)
    }

    /// Capture the active window and overlay numbered marks on its interactable
    /// accessibility elements. Returns the annotated frame and the marks legend.
    /// Caches the snapshot, so `click_element` resolves a mark's id afterward.
    pub fn a11y_marks(&mut self) -> Result<(Frame, Vec<Mark>)> {
        let frame = self.screenshot(None, None)?;
        let tree = self.a11y_snapshot()?;
        Ok(crate::marks::render(&frame, &tree))
    }

    /// Click the element with id `id` from the most recent `a11y_snapshot`
    /// (clicks the center of its bounds, via the normal pointer path).
    pub fn click_element(&mut self, id: AxNodeId) -> Result<()> {
        let t = std::time::Instant::now();
        let element = self.element_ref(id);
        let result = self.click_element_inner(id);
        self.emit_audit(
            &crate::audit::Actuation::ClickElement { element },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn click_element_inner(&mut self, id: AxNodeId) -> Result<()> {
        let (x, y) = {
            let s = self.require_active()?;
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let bounds = node.bounds.ok_or(GlassError::AxElementNotClickable(id.0))?;
            bounds
                .clamped_center(s.geometry.width, s.geometry.height)
                .ok_or(GlassError::AxElementNotClickable(id.0))?
        };
        self.pointer_inner(&PointerEvent::Click {
            x,
            y,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        })
    }

    /// Set the value/text of element `id` (from the latest `a11y_snapshot`) via the
    /// platform a11y API. Errors `NoAxSnapshot`/`AxElementNotFound` (id not in the
    /// cached snapshot), `AxUnsupported` (no reader), or — from the backend —
    /// `AxElementNotEditable`/`AxElementChanged`.
    pub fn set_value(&mut self, id: AxNodeId, text: &str) -> Result<()> {
        let t = std::time::Instant::now();
        let element = self.element_ref(id);
        let result = self.set_value_inner(id, text);
        self.emit_audit(
            &crate::audit::Actuation::SetValue { element, text },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn set_value_inner(&mut self, id: AxNodeId, text: &str) -> Result<()> {
        let (target, ctx) = {
            let s = self.require_active()?;
            // Check for reader availability before consulting the snapshot, so that
            // `AxUnsupported` takes precedence when there is no accessibility backend.
            if s.accessibility.is_none() {
                return Err(GlassError::AxUnsupported);
            }
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let target = AxTarget {
                id,
                role: node.role,
                name: node.name.clone(),
                bounds: node.bounds,
            };
            let ctx = AxContext {
                pids: s.platform.app_pids(),
                window: s.geometry.clone(),
                window_handle: s.platform.active_window_handle(),
                a11y_bus_addr: s.platform.a11y_bus_addr(),
            };
            (target, ctx)
        };
        // A dropdown/combo has no committing accessibility write: its `Selection`
        // interface only moves the popup's *preview* selection, and the model commits
        // only on row activation (Enter/click). So drive it like a person does —
        // open it, keyboard-navigate to the option, and press Enter.
        if target.role == AxRole::ComboBox {
            return self.set_combo_value(id, &target, text);
        }
        let s = self.active_mut()?;
        s.accessibility
            .as_mut()
            .ok_or(GlassError::AxUnsupported)?
            .set_value(&ctx, &target, text)?;
        s.pump();
        Ok(())
    }

    /// Select an option in a dropdown/combo by label (case-insensitive). Opens the
    /// popup, arrow-navigates from the current selection to the target, and presses
    /// Enter to commit — verifying the button label changed (else `AxValueNotApplied`).
    fn set_combo_value(&mut self, id: AxNodeId, target: &AxTarget, text: &str) -> Result<()> {
        let want = text.trim();
        // Already showing it? (the combo's name is its current selection label)
        if target
            .name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(want))
        {
            return Ok(());
        }
        // Open the popup (the combo button is in the main window, so this click lands).
        self.click_element(id)?;
        self.settle_for_popup();
        // Re-read the realized options + which one is currently selected. The open
        // combo is `expanded`; when several combos exist, ids don't survive the
        // re-snapshot, so fall back to the one nearest the target's bounds.
        let tree = self.a11y_snapshot()?;
        let combo = find_expanded_combo(&tree.root)
            .or_else(|| find_combo_near(&tree.root, target.bounds.as_ref()))
            .ok_or(GlassError::AxElementChanged(id.0))?;
        let options = collect_combo_options(combo);
        if options.is_empty() {
            return Err(GlassError::AxElementNotEditable(id.0));
        }
        let target_idx = options
            .iter()
            .position(|(label, _)| label.eq_ignore_ascii_case(want));
        let Some(target_idx) = target_idx else {
            // Unknown option — dismiss the popup so the UI is left neutral, then report.
            let _ = self.key(&KeyEvent::Chord("Escape".to_string()));
            let choices = options
                .iter()
                .map(|(l, _)| l.clone())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(GlassError::AxOptionNotFound(
                id.0,
                text.to_string(),
                choices,
            ));
        };
        // Opening focuses the current selection; step from it to the target, then Enter.
        let current_idx = options.iter().position(|(_, sel)| *sel).unwrap_or(0);
        let delta = target_idx as i32 - current_idx as i32;
        let chord = if delta >= 0 { "Down" } else { "Up" };
        for _ in 0..delta.unsigned_abs() {
            self.key(&KeyEvent::Chord(chord.to_string()))?;
        }
        self.key(&KeyEvent::Chord("Return".to_string()))?;
        self.settle_for_popup();
        // Verify the model actually committed — the *target* combo (matched by bounds,
        // now closed so nothing is `expanded`) must read the wanted label.
        let tree = self.a11y_snapshot()?;
        let ok = find_combo_near(&tree.root, target.bounds.as_ref())
            .and_then(|c| c.name.as_deref())
            .is_some_and(|n| n.eq_ignore_ascii_case(want));
        if ok {
            Ok(())
        } else {
            Err(GlassError::AxValueNotApplied(id.0))
        }
    }

    /// Let a just-opened/closed popup realize in the a11y tree before re-reading.
    fn settle_for_popup(&self) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    pub fn wait_stable(&mut self, params: &WaitStableParams) -> Result<WaitStableOutcome> {
        let active = self.require_active()?;
        // The active window's cached geometry only bounds a stability_region when
        // watching the active window itself; a specific `window` is validated by
        // the backend against its own geometry instead (see `capture`).
        if params.window.is_none() {
            let geo = active.geometry.clone();
            if let Some(r) = &params.stability_region {
                r.check_fits(geo.width, geo.height)?;
            }
        }
        let mut tracker = StabilityTracker::new(params.settle_frames, params.tolerance);
        let region = params.stability_region;
        let window = params.window;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            // Poll only the watched region (cheap) when one is set; else the full window.
            let frame = self.capture(window, region.as_ref())?;
            let settled = tracker.observe(frame)?;
            Ok(if settled { Some(()) } else { None })
        })?;
        let settled = outcome.value.is_some();
        // Return the full window: a fresh capture if we were polling a sub-region
        // (the genuinely-settled state), else the just-observed full frame.
        let frame = match region {
            Some(_) => self.capture(window, None)?,
            None => tracker.last().cloned().expect("a frame was just observed"),
        };
        Ok(WaitStableOutcome {
            frame,
            settled,
            saw_motion: tracker.saw_change(),
            observed_ms: outcome.elapsed_ms,
        })
    }

    /// Block until a precise accessibility-element condition holds, re-snapshotting
    /// each tick. Text-only outcome. The final snapshot is cached (so the returned
    /// element id is immediately usable with `click_element`). Errors immediately if
    /// the backend has no accessibility reader (the first snapshot fails).
    pub fn wait_for_element(&mut self, params: &WaitElementParams) -> Result<WaitElementOutcome> {
        self.require_active()?; // fail fast; a11y_snapshot rechecks inside the loop
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let tree = self.a11y_snapshot()?; // fresh snapshot; assigns ids, caches, pumps
            Ok(
                match element_match(
                    &tree,
                    params.name.as_deref(),
                    params.role,
                    params.value_contains.as_deref(),
                    params.condition,
                ) {
                    ElementMatch::Satisfied(node) => Some(node.map(ElementInfo::from_node)),
                    ElementMatch::Pending => None,
                },
            )
        })?;
        Ok(WaitElementOutcome {
            matched: outcome.value.is_some(),
            element: outcome.value.flatten(),
            elapsed_ms: outcome.elapsed_ms,
        })
    }

    /// Block until a watched region diverges from / converges to a reference.
    /// Compares in-memory each tick (no WebP encode). Text-only outcome; the last
    /// captured frame is returned for an optional image at the tool layer.
    /// If `baseline` is set and `region` is `None`, the baseline must match the
    /// current window size — a size change since it was saved returns `SizeMismatch`;
    /// crop to a stable `region` to avoid this.
    pub fn wait_for_region(&mut self, params: &WaitRegionParams) -> Result<WaitRegionOutcome> {
        let active = self.require_active()?;
        // As in `wait_stable`: the active window's cached geometry only bounds
        // `region` when watching the active window; a specific `window` is
        // validated by the backend against its own geometry instead.
        if params.window.is_none() {
            let geo = active.geometry.clone();
            if let Some(r) = &params.region {
                r.check_fits(geo.width, geo.height)?;
            }
        }
        // Reference: a saved baseline (cropped to the region) or the current frame.
        let reference: Frame = match &params.baseline {
            Some(name) => {
                let base = self.baselines.load(name)?;
                match &params.region {
                    Some(r) => base.crop(r)?,
                    None => base,
                }
            }
            None => self.capture(params.window, params.region.as_ref())?,
        };
        let (perceptual, threshold, tolerance, until, region, window) = (
            params.perceptual,
            params.threshold,
            params.tolerance,
            params.until,
            params.region,
            params.window,
        );
        let mut last: Option<(f32, Option<BBox>, Frame)> = None;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let current = self.capture(window, region.as_ref())?;
            let d = if perceptual {
                diff_perceptual(&reference, &current, threshold)?
            } else {
                diff(&reference, &current, tolerance)?
            };
            let satisfied = region_satisfied(&d, until);
            last = Some((d.changed_pct, d.bbox, current));
            Ok(if satisfied { Some(()) } else { None })
        })?;
        let (changed_pct, bbox, frame) = last.expect("at least one poll ran");
        Ok(WaitRegionOutcome {
            matched: outcome.value.is_some(),
            changed_pct,
            bbox,
            frame,
            elapsed_ms: outcome.elapsed_ms,
        })
    }

    /// Block until a log line matching `contains` (and optional stream) appears,
    /// scanning from `cursor` (default: the buffer end at call start, so only new
    /// lines count). Returns the matched line and a resume cursor; on timeout
    /// returns `{matched:false}` with the current end cursor.
    pub fn wait_for_log(&mut self, params: &WaitLogParams) -> Result<WaitLogOutcome> {
        let start_cursor = {
            let s = self.active_mut()?;
            s.pump();
            params.cursor.unwrap_or_else(|| s.logs.end_cursor())
        };
        let (contains, stream) = (params.contains.clone(), params.stream);
        let mut scan_cursor = start_cursor;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let s = self.active_mut()?;
            s.pump();
            let (lines, next) = s.logs.read(scan_cursor, 1, stream, Some(&contains));
            scan_cursor = next; // advance past already-examined lines so we don't re-scan
            Ok(lines.into_iter().next())
        })?;
        let s = self.active_mut()?;
        s.pump();
        let end = s.logs.end_cursor();
        Ok(match outcome.value {
            Some(line) => WaitLogOutcome {
                cursor: line.seq + 1,
                line: Some(line),
                matched: true,
                elapsed_ms: outcome.elapsed_ms,
                note: None,
            },
            None => {
                // The default cursor is the buffer end at call start, so a line emitted
                // *before* this call (e.g. a fast-boot "ready") is skipped and we time out.
                // If the substring is already in the buffer before our start cursor, say so
                // rather than failing silently — point the caller at cursor:0.
                let note = if params.cursor.is_none() {
                    let (earlier, _) = s.logs.read(0, 1, stream, Some(&contains));
                    earlier
                        .into_iter()
                        .next()
                        .filter(|l| l.seq < start_cursor)
                        .map(|l| {
                            format!(
                                "{contains:?} was already in the log at seq {} (before this call); \
                                 pass cursor:0 to match already-buffered lines",
                                l.seq
                            )
                        })
                } else {
                    None
                };
                WaitLogOutcome {
                    matched: false,
                    line: None,
                    cursor: end,
                    elapsed_ms: outcome.elapsed_ms,
                    note,
                }
            }
        })
    }

    pub fn save_baseline(&mut self, name: &str) -> Result<()> {
        let frame = {
            let s = self.active_mut()?;
            let frame = s.platform.capture_frame(None)?;
            s.pump();
            frame
        };
        self.baselines.save(name, &frame)
    }

    /// Load the named baseline and capture the current window frame, both scoped
    /// to `region` when set (the whole window otherwise). Baselines are stored
    /// whole and cropped here, so one saved baseline can be compared against any
    /// sub-region — and both operands are always cropped consistently, never
    /// silently mismatched.
    fn baseline_and_current(
        &mut self,
        name: &str,
        region: Option<&Region>,
    ) -> Result<(Frame, Frame)> {
        if let Some(r) = region {
            let geo = self.require_active()?.geometry.clone();
            r.check_fits(geo.width, geo.height)?;
        }
        let baseline = {
            let base = self.baselines.load(name)?;
            match region {
                Some(r) => base.crop(r)?,
                None => base,
            }
        };
        let current = {
            let s = self.active_mut()?;
            let frame = s.platform.capture_frame(region)?;
            s.pump();
            frame
        };
        Ok((baseline, current))
    }

    /// Exact per-channel diff of the current frame against a saved baseline.
    /// `region` scopes the comparison to a window-relative sub-rectangle.
    pub fn diff_baseline(
        &mut self,
        name: &str,
        region: Option<&Region>,
        tolerance: u8,
    ) -> Result<DiffResult> {
        self.diff_baseline_with_frame(name, region, tolerance)
            .map(|(r, _)| r)
    }

    /// Like [`diff_baseline`] but also returns the current frame that was compared.
    pub fn diff_baseline_with_frame(
        &mut self,
        name: &str,
        region: Option<&Region>,
        tolerance: u8,
    ) -> Result<(DiffResult, Frame)> {
        let (baseline, current) = self.baseline_and_current(name, region)?;
        let r = diff(&baseline, &current, tolerance)?;
        Ok((r, current))
    }

    /// Perceptual diff (YIQ + anti-alias suppression) against a saved baseline —
    /// the default for regression, robust to anti-aliasing / sub-pixel / GPU-font
    /// rendering noise. `threshold` ∈ [0,1] (smaller = stricter). `region` scopes
    /// the comparison to a window-relative sub-rectangle.
    pub fn diff_baseline_perceptual(
        &mut self,
        name: &str,
        region: Option<&Region>,
        threshold: f32,
    ) -> Result<DiffResult> {
        self.diff_baseline_perceptual_with_frame(name, region, threshold)
            .map(|(r, _)| r)
    }

    /// Like [`diff_baseline_perceptual`] but also returns the current frame compared.
    pub fn diff_baseline_perceptual_with_frame(
        &mut self,
        name: &str,
        region: Option<&Region>,
        threshold: f32,
    ) -> Result<(DiffResult, Frame)> {
        let (baseline, current) = self.baseline_and_current(name, region)?;
        let r = diff_perceptual(&baseline, &current, threshold)?;
        Ok((r, current))
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
}
