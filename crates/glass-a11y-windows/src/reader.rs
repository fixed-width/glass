//! `WindowsA11y`: the UI Automation `Accessibility` reader. Runs UIA on a fresh
//! per-snapshot thread (COM-isolated, like the AT-SPI reader's private thread),
//! finds the app's top-level window by PID (geometry fallback), and walks the bounded Control view
//! into an `AxTree`. Never returns a stub: failures are `AccessibilityUnavailable`.

use std::sync::mpsc;
use std::time::Duration;

use glass_core::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTarget, AxTree, GlassError, Result,
};
use uiautomation::patterns::{
    UIExpandCollapsePattern, UISelectionItemPattern, UITogglePattern, UIValuePattern,
};
use uiautomation::types::{ExpandCollapseState, Rect, ToggleState};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};

/// Hard cap so a hung UIA provider can't block the calling tool forever.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds so a pathological tree can't make a snapshot unbounded (tunable; sized on the box).
const MAX_DEPTH: usize = 30;
const MAX_NODES: usize = 1500;

#[derive(Default)]
pub struct WindowsA11y;

impl WindowsA11y {
    pub fn new() -> Self {
        Self
    }
}

impl Accessibility for WindowsA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        // UIA is COM and thread-affine; run it on a fresh OS thread, fully decoupled
        // from the caller's (possibly tokio) thread — mirrors the AT-SPI reader.
        std::thread::spawn(move || {
            let _ = tx.send(run_snapshot(&ctx));
        });
        match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(GlassError::AccessibilityUnavailable(
                "accessibility snapshot timed out (UIA not responding)".into(),
            )),
        }
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let ctx = ctx.clone();
        let target = target.clone();
        let text = text.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_set_value(&ctx, &target, &text));
        });
        match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(GlassError::AccessibilityUnavailable(
                "accessibility set_value timed out (UIA not responding)".into(),
            )),
        }
    }
}

fn uia_err(e: impl std::fmt::Display) -> GlassError {
    GlassError::AccessibilityUnavailable(format!("UI Automation error: {e}"))
}

fn run_snapshot(ctx: &AxContext) -> Result<AxTree> {
    // UIAutomation::new() initializes COM (MTA) on this thread.
    let automation = UIAutomation::new().map_err(|e| {
        GlassError::AccessibilityUnavailable(format!("UI Automation unavailable: {e}"))
    })?;
    let walker = automation.get_control_view_walker().map_err(uia_err)?;
    let root = automation.get_root_element().map_err(uia_err)?;

    let window = find_app_window(&walker, &root, ctx)?;

    let origin = (ctx.window.x, ctx.window.y);
    let mut count = 0usize;
    let root_node = walk(&walker, &window, origin, 0, &mut count)?;
    let mut tree = AxTree { root: root_node, count: 0 };
    tree.assign_ids();
    Ok(tree)
}

/// Find the launched app's top-level window among the desktop's children. Prefers a PID match
/// against `ctx.pids` — the launched app's process *set* (root + the descendants the backend's Job
/// enumerates), so a multi-process app (Electron/Edge) whose top-level window is owned by a
/// DESCENDANT process still matches by pid. Falls back to the window whose rect best matches
/// `ctx.window` only when no pid matches (a secondary last resort). `AccessibilityUnavailable`
/// if nothing matches.
fn find_app_window(walker: &UITreeWalker, root: &UIElement, ctx: &AxContext) -> Result<UIElement> {
    // rect distance to ctx.window (top-left + size), generous because the DWM extended-frame-bounds
    // glass reports differs from the UIA window rect by the invisible resize border (~7px/side).
    let dist_to = |win: &UIElement| -> i64 {
        win.get_bounding_rectangle()
            .ok()
            .map(|r| {
                (r.get_left() - ctx.window.x).abs() as i64
                    + (r.get_top() - ctx.window.y).abs() as i64
                    + (r.get_width() - ctx.window.width as i32).abs() as i64
                    + (r.get_height() - ctx.window.height as i32).abs() as i64
            })
            .unwrap_or(i64::MAX)
    };
    let mut by_pid: Option<(UIElement, i64)> = None; // closest among pid-matches
    let mut by_geom: Option<(UIElement, i64)> = None; // closest overall (fallback)
    let mut child = walker.get_first_child(root).ok();
    while let Some(win) = child {
        let dist = dist_to(&win);
        if by_geom.as_ref().map(|(_, d)| dist < *d).unwrap_or(true) {
            by_geom = Some((win.clone(), dist));
        }
        let pid_ok = match win.get_process_id() {
            Ok(got) => ctx.pids.is_empty() || ctx.pids.contains(&got),
            Err(_) => ctx.pids.is_empty(),
        };
        if pid_ok && by_pid.as_ref().map(|(_, d)| dist < *d).unwrap_or(true) {
            by_pid = Some((win.clone(), dist));
        }
        child = walker.get_next_sibling(&win).ok();
    }
    if let Some((w, _)) = by_pid {
        return Ok(w);
    }
    // No pid match: accept the geometry-closest window only if it's genuinely close (reject a wrong
    // window). Tolerance is generous for the border delta; tuned on the box (Task 5).
    const GEOM_TOLERANCE: i64 = 120;
    if let Some((w, d)) = by_geom {
        if d <= GEOM_TOLERANCE {
            return Ok(w);
        }
    }
    Err(GlassError::AccessibilityUnavailable(
        "the app exposes no top-level UI Automation window matching its pid or geometry (custom-drawn? fall back to screenshots)".into(),
    ))
}

/// Recursively build a normalized node, bounded by depth + global node count.
fn walk(
    walker: &UITreeWalker,
    el: &UIElement,
    origin: (i32, i32),
    depth: usize,
    count: &mut usize,
) -> Result<AxNode> {
    *count += 1;
    let ct_id = el.get_control_type().map_err(uia_err)? as i32 as u32;
    let raw_role = el.get_localized_control_type().unwrap_or_default();
    let name = nonempty(el.get_name().unwrap_or_default());
    let bounds = window_relative_bounds(el, origin);
    let (facts, value) = gather(el, ct_id);
    let states = crate::mapping::map_states(&facts);

    let mut children = Vec::new();
    if depth < MAX_DEPTH && *count < MAX_NODES {
        let mut child = walker.get_first_child(el).ok();
        while let Some(c) = child {
            if !c.is_offscreen().unwrap_or(false) {
                children.push(walk(walker, &c, origin, depth + 1, count)?);
            }
            if *count >= MAX_NODES {
                break;
            }
            child = walker.get_next_sibling(&c).ok();
        }
    }

    Ok(AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role: crate::mapping::map_role(ct_id),
        raw_role,
        name,
        value,
        states,
        bounds,
        children,
    })
}

/// Gather state facts + the value string in one pass, gating each pattern probe by control type
/// so we don't make a live cross-process `get_pattern` call for a pattern the control can't support
/// (UIA is chatty — each probe is an out-of-process COM round-trip).
fn gather(el: &UIElement, ct_id: u32) -> (crate::mapping::StateFacts, Option<String>) {
    let toggled_on = matches!(ct_id, 50000 | 50002 | 50011 | 50031) // Button/CheckBox/MenuItem/SplitButton
        && el.get_pattern::<UITogglePattern>().ok()
            .and_then(|p| p.get_toggle_state().ok())
            .map(|s| s == ToggleState::On).unwrap_or(false);
    let selected = matches!(ct_id, 50007 | 50019 | 50024 | 50029) // ListItem/TabItem/TreeItem/DataItem
        && el.get_pattern::<UISelectionItemPattern>().ok()
            .and_then(|p| p.is_selected().ok()).unwrap_or(false);
    let expanded = matches!(ct_id, 50003 | 50009 | 50011 | 50023 | 50024 | 50026 | 50033) // ComboBox/Menu/MenuItem/Tree/TreeItem/Group/Pane
        && el.get_pattern::<UIExpandCollapsePattern>().ok()
            .and_then(|p| p.get_state().ok())
            .map(|s| s == ExpandCollapseState::Expanded).unwrap_or(false);
    // Value pattern: one fetch for both the value string and read-only (Edit/ComboBox/Document)
    let (value, readonly) = if matches!(ct_id, 50003 | 50004 | 50030) {
        match el.get_pattern::<UIValuePattern>() {
            Ok(p) => (p.get_value().ok().and_then(nonempty), p.is_readonly().ok()),
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    let editable = ct_id == 50004 && readonly.map(|ro| !ro).unwrap_or(false);
    let facts = crate::mapping::StateFacts {
        enabled: el.is_enabled().unwrap_or(false),
        offscreen: el.is_offscreen().unwrap_or(false),
        focused: el.has_keyboard_focus().unwrap_or(false),
        focusable: el.is_keyboard_focusable().unwrap_or(false),
        selected,
        toggled_on,
        expanded,
        editable,
    };
    (facts, value)
}

/// UIA `BoundingRectangle` (screen) → window-relative `AxRect`, or `None` for zero-area.
fn window_relative_bounds(el: &UIElement, origin: (i32, i32)) -> Option<AxRect> {
    let r: Rect = el.get_bounding_rectangle().ok()?;
    let (w, h) = (r.get_width(), r.get_height());
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(AxRect {
        x: r.get_left() - origin.0,
        y: r.get_top() - origin.1,
        width: w as u32,
        height: h as u32,
    })
}

fn nonempty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn run_set_value(ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
    let automation = UIAutomation::new().map_err(|e| {
        GlassError::AccessibilityUnavailable(format!("UI Automation unavailable: {e}"))
    })?;
    let walker = automation.get_control_view_walker().map_err(uia_err)?;
    let root = automation.get_root_element().map_err(uia_err)?;
    let window = find_app_window(&walker, &root, ctx)?;

    // Start at 0 so find_nth's pre-order numbering matches snapshot's walk +
    // assign_ids (root id = 0); the role+name verify backstops any drift.
    let mut count = 0usize;
    let el = find_nth(&walker, &window, 0, &mut count, target.id.0)
        .ok_or(GlassError::AxElementChanged(target.id.0))?;

    // Verify role + name (guards a stale id / mirror drift).
    let role = crate::mapping::map_role(el.get_control_type().map_err(uia_err)? as i32 as u32);
    let name = nonempty(el.get_name().unwrap_or_default());
    if !target.matches(role, name.as_deref()) {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    let pat = el
        .get_pattern::<UIValuePattern>()
        .map_err(|_| GlassError::AxElementNotEditable(target.id.0))?;
    pat.set_value(text).map_err(|_| GlassError::AxElementNotEditable(target.id.0))?;
    Ok(())
}

/// Pre-order walk mirroring `walk`'s traversal (offscreen skip + depth/MAX_NODES
/// bounds) to find the element at pre-order index `target`. A single `count`
/// serves as both the id (a node's id is `count` at arrival) and the MAX_NODES
/// bound — identical accounting to `walk`, so ids line up with `assign_ids`.
fn find_nth(
    walker: &UITreeWalker,
    el: &UIElement,
    depth: usize,
    count: &mut usize,
    target: u32,
) -> Option<UIElement> {
    let my_id = *count as u32;
    *count += 1;
    if my_id == target {
        return Some(el.clone());
    }
    if depth < MAX_DEPTH && *count < MAX_NODES {
        let mut child = walker.get_first_child(el).ok();
        while let Some(c) = child {
            if !c.is_offscreen().unwrap_or(false) {
                if let Some(found) = find_nth(walker, &c, depth + 1, count, target) {
                    return Some(found);
                }
            }
            if *count >= MAX_NODES {
                break;
            }
            child = walker.get_next_sibling(&c).ok();
        }
    }
    None
}
