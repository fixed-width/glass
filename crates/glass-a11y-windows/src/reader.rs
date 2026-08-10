//! `WindowsA11y`: the UI Automation `Accessibility` reader. Runs UIA on a fresh
//! per-snapshot thread (COM-isolated, like the AT-SPI reader's private thread),
//! finds the app's top-level window by PID (geometry fallback), and walks the bounded Control view
//! into an `AxTree`. Never returns a stub: failures are `AccessibilityUnavailable`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use glass_core::{
    Accessibility, AxContext, AxDeadline, AxNode, AxNodeId, AxRect, AxTarget, AxTree, ChangeSignal,
    GlassError, Result, WalkBudget, normalize_description, read_back_confirms,
    write_took_no_effect,
};
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UIRangeValuePattern, UISelectionItemPattern,
    UITogglePattern, UIValuePattern,
};
use uiautomation::types::{ExpandCollapseState, Handle, Rect, ToggleState};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};

/// Hard cap so a hung UIA provider can't block the calling tool forever.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a read may block for its worker, and whether the caller's deadline — not
/// [`SNAPSHOT_TIMEOUT`] — is what ends the wait.
///
/// Both come from one comparison, decided before the wait: a read the *caller* cut short is a
/// spent budget, a read that used the whole ceiling is UIA that stopped answering, and inferring
/// which afterwards is the mistake glass#341 recorded.
fn bounded_wait(deadline: AxDeadline) -> (Duration, bool) {
    let own = Instant::now() + SNAPSHOT_TIMEOUT;
    (
        deadline.cap(own).saturating_duration_since(Instant::now()),
        deadline.governs(own),
    )
}

/// What a read reports when it never answered: the caller's own deadline ending it is
/// `AccessibilityNotReady`, which [`glass_core::Glass::wait_for_element`] polls through, where UIA
/// going quiet for a whole [`SNAPSHOT_TIMEOUT`] is not.
fn never_answered(by_caller: bool) -> GlassError {
    if by_caller {
        return GlassError::AccessibilityNotReady(
            "no accessibility tree within the time this call allowed".into(),
        );
    }
    GlassError::AccessibilityUnavailable(
        "accessibility snapshot timed out (UIA not responding)".into(),
    )
}
/// Per-edge tolerance (px) for the set_value bounds-fingerprint check. Window-relative
/// bounds are stable for a static element across snapshot→set_value (window moves cancel),
/// so this only absorbs sub-pixel/timing jitter; a different element that drift landed on
/// the id sits far enough away to be rejected. Generous to avoid false-rejects.
const SET_VALUE_BOUNDS_TOL: i64 = 12;
/// How long `run_set_value` polls the read-back for the value to change before declaring the
/// write a no-op — also the bound `run_invoke`'s Toggle rung gives the state to flip. A real
/// numeric set lands within a frame or two; well under the 10s outer cap.
const SET_VALUE_VERIFY_MS: u64 = 800;
/// Interval between read-backs while waiting for a write / toggle to land.
const VERIFY_POLL_MS: u64 = 20;

/// What a write that took no effect looks like on this backend: the accessibility API accepts a
/// value the toolkit never applies. Twin of the const in `glass-a11y-macos/src/reader.rs`.
const READ_ONLY_PROJECTION: &str = "this element's accessibility value may be a read-only projection that accepts a write \
     without applying it — focus the element and type into it instead";

#[derive(Default)]
pub struct WindowsA11y;

impl WindowsA11y {
    pub fn new() -> Self {
        Self
    }
}

impl Accessibility for WindowsA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        // Checked before the spawn: the worker below is detached and holds its own COM apartment,
        // so one started for a caller that has stopped waiting outlives the answer nobody reads.
        if ctx.deadline.has_passed() {
            return Err(never_answered(true));
        }
        let (wait, by_caller) = bounded_wait(ctx.deadline);
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        // UIA is COM and thread-affine; run it on a fresh OS thread, fully decoupled
        // from the caller's (possibly tokio) thread — mirrors the AT-SPI reader.
        std::thread::spawn(move || {
            let _ = tx.send(run_snapshot(&ctx));
        });
        match rx.recv_timeout(wait) {
            Ok(r) => r,
            Err(_) => Err(never_answered(by_caller)),
        }
    }

    fn subscribe_changes(&mut self, ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        // Unlike `snapshot` above and `set_value`/`invoke` below there is no timeout wrapper: the
        // subscription's own thread is the long-lived one, and `subscribe` already bounds how
        // long it will wait for the registrations to land.
        crate::events::subscribe(ctx)
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

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let ctx = ctx.clone();
        let target = target.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_invoke(&ctx, &target));
        });
        match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
            // This reader actuates the element it resolved, so it never substitutes another.
            Ok(r) => r.map(|()| None),
            // The worker thread outlives this timeout, so the pattern call may already have
            // been dispatched — say so. This error is NOT fallback-eligible (see
            // `GlassError::invoke_fallback_eligible`), so no pointer click is layered on top.
            Err(_) => Err(GlassError::AccessibilityUnavailable(
                "accessibility invoke timed out (UIA not responding); the action may still \
                 land — re-snapshot before retrying"
                    .into(),
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
    tree.unreadable = budget.unreadable();
    tree.assign_ids();
    Ok(tree)
}

/// Bind a UIA element to glass's adopted window via its handle (`AxContext::window_handle`, set by
/// the backend from its active `HWND`). a11y reads the *exact* window glass drives — the same handle
/// `send_pointer`/`window` operate on — so it never enumerates the desktop or queries a peer app's
/// UIA provider (a foreign provider that blocks cross-process calls on the worker thread could
/// otherwise wedge the whole snapshot). `element_from_handle` touches only the target's provider.
pub(crate) fn find_app_window(automation: &UIAutomation, ctx: &AxContext) -> Result<UIElement> {
    let handle = ctx.window_handle.ok_or_else(|| {
        GlassError::AccessibilityUnavailable(
            "no active window handle in the a11y context (the backend adopted no window)".into(),
        )
    })?;
    automation
        .element_from_handle(Handle::from(handle as isize))
        .map_err(uia_err)
}

/// A tree-walker step's result, with a genuine read failure counted on `budget`.
///
/// UIA reports "there is no such element" as `S_OK` with a NULL out-param, which `windows-rs`
/// cannot build an interface from — so absence arrives as an `Err` too. It is separable because
/// that error's code reaches us as **zero** (`windows_result::Error::code` maps its internal
/// empty-error sentinel back to `HRESULT(0)`), while a real failure carries a negative HRESULT,
/// and `uiautomation::Error::result` is `Some` only for a negative code. A plain `.ok()` collapses
/// the two, which is what let a dropped subtree read as an empty one.
fn step(r: uiautomation::Result<UIElement>, budget: &mut WalkBudget) -> Option<UIElement> {
    r.inspect_err(|e| {
        if e.result().is_some() {
            budget.note_unreadable();
        }
    })
    .ok()
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
    // `canonical_name` knows every documented control type, mapped or not, so the numeric form
    // is reached only by a vendor-defined or future id — still reported, never dropped.
    let raw_role = crate::mapping::canonical_name(ct_id)
        .map(str::to_string)
        .unwrap_or_else(|| format!("UIA:{ct_id}"));
    let name = nonempty(el.get_name().unwrap_or_default());
    let description = normalize_description(&help_text(el, ct_id), name.as_deref());
    let bounds = window_relative_bounds(el, origin);
    let (facts, value) = gather(el, ct_id);
    let states = crate::mapping::map_states(&facts);

    let mut children = Vec::new();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let first_child = step(walker.get_first_child(el), budget);
    // Tests only whether a first child exists, before `is_offscreen` filters it. A node whose
    // children are all offscreen, reached once the budget is spent, still records a truncation
    // though nothing real was declined. Pre-filtering would mean walking the whole
    // `get_first_child`/`get_next_sibling` chain — the unbounded scan `MAX_SIBLINGS` bounds.
    if first_child.is_some() && budget.may_explore_children(depth) {
        // Offscreen children are skipped without entering, so they never count against
        // `MAX_NODES` — a virtualized list of thousands (or a cyclic `get_next_sibling`
        // chain) would otherwise scan this level forever. `MAX_SIBLINGS` bounds the
        // per-level scan regardless of how many are skipped.
        let mut child = first_child;
        let mut siblings = 0usize;
        while let Some(c) = child {
            // Checked before processing each child (not after) so the child that merely
            // completes the tree doesn't get mistaken for one the walk declined to visit.
            if !budget.may_visit_sibling(siblings) {
                break;
            }
            siblings += 1;
            if !c.is_offscreen().unwrap_or(false) {
                children.push(walk(walker, &c, origin, depth + 1, budget)?);
            }
            child = step(walker.get_next_sibling(&c), budget);
        }
    }

    Ok(AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role: crate::mapping::map_role(ct_id, facts.checkable),
        raw_role,
        name,
        description,
        value,
        states,
        bounds,
        children,
    })
}

/// Fetch `el`'s UIA Toggle pattern, gated by control type (Button/CheckBox/MenuItem/
/// SplitButton — the only types that carry it) so this never issues a live cross-process
/// `get_pattern` call for a control that cannot support it. One fetch answers two questions:
/// `StateFacts::checkable` is this pattern's mere *presence* (the control exposes on/off
/// semantics at all, independent of whether the current state is also readable), and
/// `map_role`'s rule that a toggle-capable `Button` — a formatting-bar button, say — is a
/// `ToggleButton` rather than a plain one keys off the same presence. Shared by `gather` and
/// the verify-fingerprint role lookups in `run_set_value`/`run_invoke`, so a node maps to the
/// same role regardless of which path reads it, and only one COM round-trip is spent per node
/// either way — the same reason the value-pattern probe below fetches once for two facts.
///
/// "This control has no Toggle pattern" and "the fetch failed" both yield `None` — a failure
/// must not fail a whole snapshot over one node — but they are not the same event, and only one
/// of them is a bug: a transient failure reports a toggle-capable `Button` as a plain `Button`,
/// and since the same value feeds the `run_set_value`/`run_invoke` fingerprint, it surfaces to
/// the caller as `AxElementChanged` ("the tree drifted") for an element that never moved. So the
/// failure case logs. The two are told apart by the returned code: UIA documents
/// `GetCurrentPattern` as returning success with a NULL interface for an unsupported pattern,
/// which the bindings surface as an error carrying a *non-failure* code (`Error::result()` is
/// `None`); a real COM failure carries a failure `HRESULT`. Neither path costs an extra
/// round-trip.
fn toggle_pattern(el: &UIElement, ct_id: u32) -> Option<UITogglePattern> {
    if !matches!(ct_id, 50000 | 50002 | 50011 | 50031) {
        // Button/CheckBox/MenuItem/SplitButton
        return None;
    }
    match el.get_pattern::<UITogglePattern>() {
        Ok(p) => Some(p),
        Err(e) => {
            if e.result().is_some() {
                // Dev-tool diagnostic (stderr only, same shape as `select_window`'s in the
                // macOS reader): without it, a control whose Toggle fetch broke is
                // indistinguishable after the fact from one that never had the pattern.
                eprintln!(
                    "glass-a11y-windows: Toggle-pattern fetch failed on control type {ct_id} \
                     (HRESULT {:#010x}: {e}); treating the element as not toggle-capable",
                    e.code()
                );
            }
            None
        }
    }
}

/// Gather state facts + the value string in one pass, gating each pattern probe by control type
/// so we don't make a live cross-process `get_pattern` call for a pattern the control can't support
/// (UIA is chatty — each probe is an out-of-process COM round-trip).
fn gather(el: &UIElement, ct_id: u32) -> (crate::mapping::StateFacts, Option<String>) {
    // Fetch the Toggle pattern once: its mere presence is `checkable` (the control exposes
    // on/off semantics at all), independent of whether we can also read its current state.
    let pattern = toggle_pattern(el, ct_id);
    let checkable = pattern.is_some();
    let toggled_on = pattern
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

/// `el`'s UIA `HelpText` — the tooltip, and the secondary label the outline renders as
/// `desc="…"`. Costs one cross-process property read per node.
///
/// A failed read degrades to no description, since one unreadable property must not fail a whole
/// snapshot, but it logs first (the treatment [`toggle_pattern`] already gives its own failures).
/// It matters more here than the `.ok()` on a pattern probe: `CurrentHelpText` answers an *unset*
/// property with an empty string, so every `Err` is a genuine COM failure — a stale element, a hung
/// or disconnected provider, a denied cross-integrity read — never "the app set no help text".
///
/// `FullDescription` is UIA's other secondary label; `uiautomation` 0.25 exposes no accessor for it
/// (only `UIProperty::FullDescription` through `get_property_value`), and no probed app was
/// observed carrying one.
fn help_text(el: &UIElement, ct_id: u32) -> String {
    el.get_help_text().unwrap_or_else(|e| {
        eprintln!(
            "glass-a11y-windows: HelpText read failed on control type {ct_id} \
             (HRESULT {:#010x}: {e}); treating the element as having no description",
            e.code()
        );
        String::new()
    })
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
    let ct_id = el.get_control_type().map_err(uia_err)? as i32 as u32;
    let role = crate::mapping::map_role(ct_id, toggle_pattern(&el, ct_id).is_some());
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
        let after = pat.get_value().ok();
        if read_back_confirms(after.as_deref(), before.as_deref(), text) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // A read that failed, or one showing a value the element reformatted, is not evidence
            // of a projection that accepted the write and kept its value.
            return Err(match after.as_deref() {
                Some(seen) if write_took_no_effect(seen, before.as_deref()) => {
                    GlassError::value_not_applied_because(
                        target.id.0,
                        text,
                        Some(seen),
                        READ_ONLY_PROJECTION,
                    )
                }
                seen => GlassError::value_not_applied(target.id.0, text, seen),
            });
        }
        std::thread::sleep(Duration::from_millis(VERIFY_POLL_MS));
    }
}

/// Actuate `target` via the first UIA action pattern its control publishes. Walks pre-order to
/// `target.id` and verifies the same role+name+bounds fingerprint as `run_set_value` (guards a
/// stale id / tree drift) before touching any pattern.
///
/// Ladder order mirrors how a real client picks a control's actuation verb, most-specific first:
/// Invoke (buttons, menu items — "press this") -> Toggle (checkboxes/switches, which don't
/// implement Invoke) -> SelectionItem (list/tab/tree rows — "select", not "press") ->
/// ExpandCollapse (tree/combo expanders — flip between the two states rather than invoke). The
/// first pattern the control exposes wins; a control that publishes none of the four is
/// `AxActionUnavailable` (the reader itself is fine — this element just offers no actuation verb),
/// which `click_element` (glass-core) treats as a fall-back-to-pointer signal, not a fatal error.
///
/// Only the Toggle rung has a post-state a client can read back, so only it verifies actuation;
/// the other three are fire-and-report, exactly as their patterns define. That rung is also the
/// one exception to "first pattern wins": if its state can't be read it is skipped rather than
/// failed, since it cannot be verified and nothing has been dispatched yet.
///
/// Known limitation, deliberate: `get_pattern` returning `Err` is indistinguishable here between
/// "this control does not implement the pattern" and "the COM call itself failed", so both land
/// on `AxActionUnavailable` and fall back to a pointer click. That is the safe direction —
/// `get_pattern` dispatches no action, so the fallback actuates exactly once. UIA does publish
/// `Is<Pattern>Available` properties that could tell the two apart, but acting on them would turn
/// a disagreement between property and `get_pattern` into a hard, non-falling-back click failure
/// (an error after dispatch never falls back), trading a harmless pointer click for a dead one.
fn run_invoke(ctx: &AxContext, target: &AxTarget) -> Result<()> {
    let automation = UIAutomation::new().map_err(|e| {
        GlassError::AccessibilityUnavailable(format!("UI Automation unavailable: {e}"))
    })?;
    let walker = automation.get_control_view_walker().map_err(uia_err)?;
    let window = find_app_window(&automation, ctx)?;

    // Start at 0 so find_nth's pre-order numbering matches snapshot's walk + assign_ids, same as
    // run_set_value.
    let mut budget = WalkBudget::with_limits(ctx.limits);
    let el = find_nth(&walker, &window, 0, &mut budget, target.id.0)
        .ok_or(GlassError::AxElementChanged(target.id.0))?;

    // Same fingerprint gate as run_set_value: role + name + bounds.
    let ct_id = el.get_control_type().map_err(uia_err)? as i32 as u32;
    let role = crate::mapping::map_role(ct_id, toggle_pattern(&el, ct_id).is_some());
    let name = nonempty(el.get_name().unwrap_or_default());
    let bounds = window_relative_bounds(&el, (ctx.window.x, ctx.window.y));
    if !target.matches(role, name.as_deref())
        || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
    {
        return Err(GlassError::AxElementChanged(target.id.0));
    }

    let fail = |e: uiautomation::Error| GlassError::AxActionFailed(target.id.0, e.to_string());
    if let Ok(p) = el.get_pattern::<UIInvokePattern>() {
        return p.invoke().map_err(fail);
    }
    if let Ok(p) = el.get_pattern::<UITogglePattern>() {
        // Toggle is the one rung with a readable post-state, so don't take the ack as proof:
        // a provider that accepts `Toggle()` without applying it would otherwise report a
        // successful click on a control that never moved. Read before, fire, then poll until
        // the state differs — same cadence as `run_set_value`'s write verify.
        //
        // A pattern whose state can't even be READ can't be verify-toggled, so this rung is
        // unusable — fall through to the rest of the ladder instead of reporting a failure.
        // Nothing has been dispatched at this point, so falling through is safe: the worst
        // outcome is `AxActionUnavailable` and a single pointer click, whereas an error here
        // would propagate (an error after dispatch never falls back) and kill the click.
        if let Ok(before) = p.get_toggle_state() {
            p.toggle().map_err(fail)?;
            let deadline = Instant::now() + Duration::from_millis(SET_VALUE_VERIFY_MS);
            loop {
                // Past the dispatch, a failed read IS a failure: `fail` (AxActionFailed) is
                // right here, because the toggle may have landed and must not be re-actuated.
                if p.get_toggle_state().map_err(fail)? != before {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    // `AxActionFailed`, not `AxActionUnavailable`: the toggle WAS dispatched,
                    // so this must not fall back to a pointer click that could actuate twice.
                    return Err(GlassError::AxActionFailed(
                        target.id.0,
                        "the toggle action was acknowledged but the state did not change".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(VERIFY_POLL_MS));
            }
        }
    }
    if let Ok(p) = el.get_pattern::<UISelectionItemPattern>() {
        return p.select().map_err(fail);
    }
    if let Ok(p) = el.get_pattern::<UIExpandCollapsePattern>() {
        let expanded = p.get_state().map_err(fail)? == ExpandCollapseState::Expanded;
        return if expanded { p.collapse() } else { p.expand() }.map_err(fail);
    }
    Err(GlassError::AxActionUnavailable(target.id.0))
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
    let first_child = step(walker.get_first_child(el), budget);
    // Same gap as `walk`: only tests whether a first child exists, before `is_offscreen` runs.
    // A node whose children are all offscreen, reached once the budget is spent, still records
    // a truncation though nothing real was declined — left as-is for the same reason: it would
    // mean walking the whole sibling chain, exactly the scan `MAX_SIBLINGS` exists to bound.
    if first_child.is_none() || !budget.may_explore_children(depth) {
        return None;
    }
    let mut child = first_child;
    let mut siblings = 0usize;
    while let Some(c) = child {
        // Checked before processing each child (not after) so the child that merely
        // completes the tree doesn't get mistaken for one the walk declined to visit.
        // Same per-level bound as walk(), so find_nth can't spin either.
        if !budget.may_visit_sibling(siblings) {
            break;
        }
        siblings += 1;
        if !c.is_offscreen().unwrap_or(false)
            && let Some(found) = find_nth(walker, &c, depth + 1, budget, target)
        {
            return Some(found);
        }
        child = step(walker.get_next_sibling(&c), budget);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// glass#338: only the reader can hold a read inside the caller's timeout — the worker below
    /// is detached, so nothing outside it can shorten one that has started.
    #[test]
    fn a_read_is_bounded_by_the_caller_when_that_falls_first() {
        let (wait, by_caller) = bounded_wait(AxDeadline::from_millis(50));
        assert!(wait <= Duration::from_millis(50), "{wait:?}");
        assert!(by_caller);
    }

    /// The other direction: without it the test above passes on a reader that waits for nothing.
    #[test]
    fn a_caller_that_names_no_deadline_leaves_the_read_its_own_ceiling() {
        let (wait, by_caller) = bounded_wait(AxDeadline::UNBOUNDED);
        assert!(wait > SNAPSHOT_TIMEOUT - Duration::from_secs(1), "{wait:?}");
        assert!(!by_caller);
    }

    /// The variant decides whether a wait polls on or fails, so the two causes must not collapse:
    /// a bus that went quiet for the whole ceiling is a real fault, and a caller that ran out of
    /// its own time is not.
    #[test]
    fn a_read_the_caller_cut_short_reads_as_not_ready_where_a_quiet_bus_does_not() {
        assert!(matches!(
            never_answered(true),
            GlassError::AccessibilityNotReady(_)
        ));
        assert!(matches!(
            never_answered(false),
            GlassError::AccessibilityUnavailable(_)
        ));
    }

    /// The failure mode this guards: if an absent child ever stopped arriving as a zero code,
    /// every leaf in every snapshot would report an unreadable subtree. Builds the error
    /// directly rather than driving a walker, so it needs no COM.
    #[test]
    fn an_absent_child_is_not_counted_as_unreadable() {
        let mut budget = WalkBudget::new();
        // Zero is what the real chain yields for an absent child — see `step`.
        let absent = uiautomation::Error::new(0, "no such element");
        assert!(step(Err(absent), &mut budget).is_none());
        assert_eq!(
            budget.unreadable(),
            0,
            "S_OK with a null out-param means no such element, not a failed read"
        );
    }

    #[test]
    fn a_failed_child_read_is_counted_as_unreadable() {
        let mut budget = WalkBudget::new();
        // UIA_E_ELEMENTNOTAVAILABLE — a real failure, negative like every HRESULT error.
        let failed = uiautomation::Error::new(-2147220991, "element not available");
        assert!(step(Err(failed), &mut budget).is_none());
        assert_eq!(budget.unreadable(), 1);
    }
}
