//! `WindowsA11y`: the UI Automation `Accessibility` reader. Runs UIA on a fresh
//! per-snapshot thread (COM-isolated, like the AT-SPI reader's private thread),
//! finds the app's top-level window by PID (geometry fallback), and walks the bounded Control view
//! into an `AxTree`. Never returns a stub: failures are `AccessibilityUnavailable`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use glass_core::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTarget, AxTree, GlassError, Result,
    TruncationLimit, WalkBudget,
};
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIRangeValuePattern, UISelectionItemPattern, UITogglePattern,
    UIValuePattern,
};
use uiautomation::types::{ExpandCollapseState, Handle, Rect, ToggleState};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};

/// Hard cap so a hung UIA provider can't block the calling tool forever.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-edge tolerance (px) for the set_value bounds-fingerprint check. Window-relative
/// bounds are stable for a static element across snapshot→set_value (window moves cancel),
/// so this only absorbs sub-pixel/timing jitter; a different element that drift landed on
/// the id sits far enough away to be rejected. Generous to avoid false-rejects.
const SET_VALUE_BOUNDS_TOL: i64 = 12;
/// How long `run_set_value` polls the read-back for the value to change before declaring the
/// write a no-op. A real numeric set lands within a frame or two; well under the 10s outer cap.
const SET_VALUE_VERIFY_MS: u64 = 800;

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
    let window = find_app_window(&automation, ctx)?;

    let origin = (ctx.window.x, ctx.window.y);
    let mut budget = WalkBudget::with_limits(ctx.limits);
    let root_node = walk(&walker, &window, origin, 0, &mut budget)?;
    let mut tree = AxTree::new(root_node);
    tree.truncated = budget.truncation();
    tree.assign_ids();
    Ok(tree)
}

/// Bind a UIA element to glass's adopted window via its handle (`AxContext::window_handle`, set by
/// the backend from its active `HWND`). a11y reads the *exact* window glass drives — the same handle
/// `send_pointer`/`window` operate on — so it never enumerates the desktop or queries a peer app's
/// UIA provider (a foreign provider that blocks cross-process calls on the worker thread could
/// otherwise wedge the whole snapshot). `element_from_handle` touches only the target's provider.
fn find_app_window(automation: &UIAutomation, ctx: &AxContext) -> Result<UIElement> {
    let handle = ctx.window_handle.ok_or_else(|| {
        GlassError::AccessibilityUnavailable(
            "no active window handle in the a11y context (the backend adopted no window)".into(),
        )
    })?;
    automation
        .element_from_handle(Handle::from(handle as isize))
        .map_err(uia_err)
}

/// Recursively build a normalized node, bounded by [`WalkBudget`] (node count, nesting depth,
/// and per-level sibling scan) so a pathological tree can't burn the outer [`SNAPSHOT_TIMEOUT`]
/// with no tree to show for it.
fn walk(
    walker: &UITreeWalker,
    el: &UIElement,
    origin: (i32, i32),
    depth: usize,
    budget: &mut WalkBudget,
) -> Result<AxNode> {
    budget.visit();
    let ct_id = el.get_control_type().map_err(uia_err)? as i32 as u32;
    let raw_role = el.get_localized_control_type().unwrap_or_default();
    let name = nonempty(el.get_name().unwrap_or_default());
    let bounds = window_relative_bounds(el, origin);
    let (facts, value) = gather(el, ct_id);
    let states = crate::mapping::map_states(&facts);

    let mut children = Vec::new();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let first_child = walker.get_first_child(el).ok();
    // Tests only whether a first child exists, before `is_offscreen` filters it. A node whose
    // children are all offscreen, reached once the budget is spent, still records a truncation
    // though nothing real was declined. Pre-filtering would mean walking the whole
    // `get_first_child`/`get_next_sibling` chain — the unbounded scan `MAX_SIBLINGS` bounds.
    if first_child.is_some() && may_explore_children(budget, depth) {
        // Offscreen children are skipped without entering, so they never count against
        // `MAX_NODES` — a virtualized list of thousands (or a cyclic `get_next_sibling`
        // chain) would otherwise scan this level forever. `MAX_SIBLINGS` bounds the
        // per-level scan regardless of how many are skipped.
        let mut child = first_child;
        let mut siblings = 0usize;
        while let Some(c) = child {
            // Checked before processing each child (not after) so the child that merely
            // completes the tree doesn't get mistaken for one the walk declined to visit.
            if budget.nodes_exhausted() {
                budget.hit(TruncationLimit::Nodes);
                break;
            }
            siblings += 1;
            if siblings > budget.max_siblings() {
                budget.hit(TruncationLimit::Siblings);
                break;
            }
            if !c.is_offscreen().unwrap_or(false) {
                children.push(walk(walker, &c, origin, depth + 1, budget)?);
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
    // Fetch the Toggle pattern once: its mere presence is `checkable` (the control exposes
    // on/off semantics at all), independent of whether we can also read its current state.
    let toggle_pattern = matches!(ct_id, 50000 | 50002 | 50011 | 50031) // Button/CheckBox/MenuItem/SplitButton
        .then(|| el.get_pattern::<UITogglePattern>().ok())
        .flatten();
    let checkable = toggle_pattern.is_some();
    let toggled_on = toggle_pattern
        .and_then(|p| p.get_toggle_state().ok())
        .map(|s| s == ToggleState::On)
        .unwrap_or(false);
    let selected = matches!(ct_id, 50007 | 50019 | 50024 | 50029) // ListItem/TabItem/TreeItem/DataItem
        && el.get_pattern::<UISelectionItemPattern>().ok()
            .and_then(|p| p.is_selected().ok()).unwrap_or(false);
    let expanded = matches!(ct_id, 50003 | 50009 | 50011 | 50023 | 50024 | 50026 | 50033) // ComboBox/Menu/MenuItem/Tree/TreeItem/Group/Pane
        && el.get_pattern::<UIExpandCollapsePattern>().ok()
            .and_then(|p| p.get_state().ok())
            .map(|s| s == ExpandCollapseState::Expanded).unwrap_or(false);
    // Value pattern: one fetch for both the value string and read-only (Edit/ComboBox/Document)
    let (value_text, readonly) = if matches!(ct_id, 50003 | 50004 | 50030) {
        match el.get_pattern::<UIValuePattern>() {
            Ok(p) => (p.get_value().ok().and_then(nonempty), p.is_readonly().ok()),
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    // RangeValue pattern: a Slider/Spinner/ProgressBar exposes its numeric position here, never
    // via ValuePattern, so read it (gated by control type — `get_pattern` is a COM round-trip) so
    // `value_contains`/`wait_for_element` can match the number.
    let value = value_text.or_else(|| {
        matches!(ct_id, 50012 | 50015 | 50016) // ProgressBar/Slider/Spinner
            .then(|| {
                el.get_pattern::<UIRangeValuePattern>()
                    .ok()
                    .and_then(|p| p.get_value().ok())
                    .map(crate::mapping::format_range_value)
            })
            .flatten()
    });
    // Editable iff a writable ValuePattern is present — for ANY value-bearing
    // control (Edit/ComboBox/Document), not just Edit; otherwise a writable
    // ComboBox/Document reports editable=false while set_value would succeed on
    // it. `readonly` is only `Some` for those three types (gated above), so the
    // match keeps non-value controls non-editable.
    let editable =
        matches!(ct_id, 50003 | 50004 | 50030) && readonly.map(|ro| !ro).unwrap_or(false);
    let facts = crate::mapping::StateFacts {
        enabled: el.is_enabled().unwrap_or(false),
        offscreen: el.is_offscreen().unwrap_or(false),
        focused: el.has_keyboard_focus().unwrap_or(false),
        focusable: el.is_keyboard_focusable().unwrap_or(false),
        selected,
        toggled_on,
        expanded,
        editable,
        checkable,
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
    let window = find_app_window(&automation, ctx)?;

    // Start at 0 so find_nth's pre-order numbering matches snapshot's walk +
    // assign_ids (root id = 0); the role+name verify backstops any drift.
    let mut budget = WalkBudget::with_limits(ctx.limits);
    let el = find_nth(&walker, &window, 0, &mut budget, target.id.0)
        .ok_or(GlassError::AxElementChanged(target.id.0))?;

    // Verify role + name + bounds (guards a stale id / tree drift). role+name
    // alone isn't unique (many controls share a role and an empty name), so if
    // drift lands a different same-role+name element on this pre-order id, the
    // bounds fingerprint — the element sits elsewhere — rejects it. A target
    // without captured bounds falls back to role+name only.
    let role = crate::mapping::map_role(el.get_control_type().map_err(uia_err)? as i32 as u32);
    let name = nonempty(el.get_name().unwrap_or_default());
    let bounds = window_relative_bounds(&el, (ctx.window.x, ctx.window.y));
    if !target.matches(role, name.as_deref())
        || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
    {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    let pat = el
        .get_pattern::<UIValuePattern>()
        .map_err(|_| GlassError::AxElementNotEditable(target.id.0))?;
    // Pre-write value: the baseline for the "changed" check. `None` (a failed pre-read) means the
    // baseline is unknown — the confirmation below then requires an exact match rather than
    // trusting a "differs from before" signal it cannot compute.
    let before = pat.get_value().ok();
    pat.set_value(text)
        .map_err(|_| GlassError::AxElementNotEditable(target.id.0))?;
    // Verify the write took, error-aware. egui/accesskit read-only editables accept SetValue
    // without error but never apply it (false success). Poll the value back — a real numeric set
    // lands a frame later. `.ok()` maps a failed read to `None`, which never confirms, so neither
    // a failed post-read nor a failed pre-read can masquerade as a successful change.
    let deadline = Instant::now() + Duration::from_millis(SET_VALUE_VERIFY_MS);
    loop {
        if read_back_confirms(pat.get_value().ok().as_deref(), before.as_deref(), text) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(GlassError::AxValueNotApplied(target.id.0));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether this node's children may be explored, recording the bound that stopped the walk
/// when they may not. Callers only consult this once they already know the child list is
/// non-empty — calling it for a childless node would record a truncation for declining to
/// explore a list that was never going to be walked anyway.
///
/// `walk` and `find_nth` MUST consult this one function at the same point in their
/// traversal. They assign a node's id by arrival order, and `set_value` re-walks to a
/// caller-supplied id — so a bound applied in one traversal but not the other resolves the
/// id against a different tree and writes to the wrong element. Sharing the decision makes
/// that divergence impossible to introduce by editing only one of them.
fn may_explore_children(budget: &mut WalkBudget, depth: usize) -> bool {
    if budget.depth_exhausted(depth) {
        budget.hit(TruncationLimit::Depth);
        return false;
    }
    if budget.nodes_exhausted() {
        budget.hit(TruncationLimit::Nodes);
        return false;
    }
    true
}

/// Pre-order DFS to the node at index `target`, mirroring `walk` exactly: visit the node (its
/// id is the arrival count), then recurse each unskipped child in tree-walker order — **and
/// stopping at the same depth/node/sibling bounds**. The bounds must stay in lockstep with
/// `walk`: if this traversal visited nodes `walk` skipped, a `set_value` id would resolve
/// against a different tree and write to the wrong element.
fn find_nth(
    walker: &UITreeWalker,
    el: &UIElement,
    depth: usize,
    budget: &mut WalkBudget,
    target: u32,
) -> Option<UIElement> {
    if budget.nodes_walked() == target as usize {
        return Some(el.clone());
    }
    budget.visit();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let first_child = walker.get_first_child(el).ok();
    // Same gap as `walk`: only tests whether a first child exists, before `is_offscreen` runs.
    // A node whose children are all offscreen, reached once the budget is spent, still records
    // a truncation though nothing real was declined — left as-is for the same reason: it would
    // mean walking the whole sibling chain, exactly the scan `MAX_SIBLINGS` exists to bound.
    if first_child.is_none() || !may_explore_children(budget, depth) {
        return None;
    }
    let mut child = first_child;
    let mut siblings = 0usize;
    while let Some(c) = child {
        // Checked before processing each child (not after) so the child that merely
        // completes the tree doesn't get mistaken for one the walk declined to visit.
        if budget.nodes_exhausted() {
            budget.hit(TruncationLimit::Nodes);
            break;
        }
        siblings += 1;
        if siblings > budget.max_siblings() {
            budget.hit(TruncationLimit::Siblings);
            break; // same per-level bound as walk(), so find_nth can't spin either
        }
        if !c.is_offscreen().unwrap_or(false) {
            if let Some(found) = find_nth(walker, &c, depth + 1, budget, target) {
                return Some(found);
            }
        }
        child = walker.get_next_sibling(&c).ok();
    }
    None
}

/// Whether a `set_value` write actually took, judged from the value read back. egui-style
/// read-only editables (`TextEdit`) accept UIA `SetValue` without error but never change the
/// buffer — a misleading success; a real set (`Slider`/`DragValue`) changes the value, possibly
/// to a reformatted string. So it took iff the read-back equals the request OR differs from the
/// pre-set value.
pub(crate) fn set_value_took(before: &str, after: &str, requested: &str) -> bool {
    after == requested || after != before
}

/// Whether a read-back poll can *confirm* a `set_value` write took. `read_back` is the value
/// read after the write (`None` if that read failed); `before` is the pre-write baseline
/// (`None` if the pre-read failed — baseline unknown). Confirms only when it can prove the
/// write landed:
/// - a failed post-write read (`read_back == None`) is inconclusive → never confirms (the
///   caller keeps polling to its deadline, then reports `AxValueNotApplied`);
/// - with a known baseline, delegates to [`set_value_took`] (equals request, or changed from it);
/// - with an unknown baseline, only an exact match with the request confirms — "changed from
///   before" is meaningless without a trustworthy baseline.
///
/// This is the honesty guard against a *failed read* masquerading as a change: the pre-fix loop
/// collapsed both reads to `""` via `unwrap_or_default()`, so a failed read looked like a value
/// that "differs from before" and reported false success. Mirrors the macOS reader.
fn read_back_confirms(read_back: Option<&str>, before: Option<&str>, requested: &str) -> bool {
    match (read_back, before) {
        (None, _) => false,
        (Some(after), Some(before)) => set_value_took(before, after, requested),
        (Some(after), None) => after == requested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{MAX_DEPTH, MAX_NODES};

    #[test]
    fn below_the_caps_children_may_be_explored_and_nothing_is_recorded() {
        let mut budget = WalkBudget::new();
        assert!(may_explore_children(&mut budget, 0));
        assert!(budget.truncation().is_none());
    }

    #[test]
    fn at_max_depth_the_depth_bound_is_recorded_and_children_may_not_be_explored() {
        let mut budget = WalkBudget::new();
        assert!(!may_explore_children(&mut budget, MAX_DEPTH));
        assert_eq!(
            budget.truncation().map(|t| t.limit),
            Some(TruncationLimit::Depth)
        );
    }

    #[test]
    fn with_the_node_budget_spent_the_nodes_bound_is_recorded_and_children_may_not_be_explored() {
        let mut budget = WalkBudget::new();
        for _ in 0..MAX_NODES {
            budget.visit();
        }
        assert!(!may_explore_children(&mut budget, 0));
        assert_eq!(
            budget.truncation().map(|t| t.limit),
            Some(TruncationLimit::Nodes)
        );
    }

    #[test]
    fn when_both_bounds_are_exhausted_the_recorded_limit_is_depth() {
        let mut budget = WalkBudget::new();
        for _ in 0..MAX_NODES {
            budget.visit();
        }
        assert!(!may_explore_children(&mut budget, MAX_DEPTH));
        assert_eq!(
            budget.truncation().map(|t| t.limit),
            Some(TruncationLimit::Depth)
        );
    }

    #[test]
    fn noop_is_not_taken() {
        // read-only TextEdit: value unchanged AND not the requested text.
        assert!(!set_value_took("#000000", "#000000", "#12AA34"));
    }
    #[test]
    fn exact_match_took() {
        assert!(set_value_took("#000000", "#12AA34", "#12AA34"));
    }
    #[test]
    fn reformatted_numeric_change_took() {
        // a slider set to "50" may read back "50.0" — changed from before, so it took.
        assert!(set_value_took("0", "50.0", "50"));
    }
    #[test]
    fn setting_current_value_is_taken() {
        // edge: requesting the value it already holds → equals request → taken (acceptable).
        assert!(set_value_took("50", "50", "50"));
    }
    #[test]
    fn read_back_rejects_a_failed_post_read() {
        // A failed post-write read (None) is inconclusive — never a false success.
        assert!(!read_back_confirms(None, Some("hello"), "world"));
    }
    #[test]
    fn read_back_confirms_change_against_known_baseline() {
        // Known baseline + value changed from it → took (delegates to set_value_took).
        assert!(read_back_confirms(Some("50.0"), Some("0"), "50"));
    }
    #[test]
    fn read_back_rejects_unconfirmable_change_when_baseline_unknown() {
        // Regression: pre-fix a failed pre-read defaulted to "", so a no-op that reads back its
        // real (non-empty) value looked "changed" → false success. An unknown baseline must not
        // confirm a mere difference; only an exact match can.
        assert!(!read_back_confirms(Some("hello"), None, "world"));
    }
    #[test]
    fn read_back_confirms_exact_match_when_baseline_unknown() {
        // Unknown baseline, but the read-back equals the request → definitively took.
        assert!(read_back_confirms(Some("world"), None, "world"));
    }
}
