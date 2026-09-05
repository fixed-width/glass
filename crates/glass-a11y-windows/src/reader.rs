//! `WindowsA11y`: the UI Automation `Accessibility` reader. Runs each bounded call on a detached,
//! COM-isolated thread (`glass_core::A11yThread`), finds the app's top-level window by PID
//! (geometry fallback), and walks the bounded Control view into an `AxTree`. Never returns a stub:
//! failures are `AccessibilityUnavailable`.

use std::time::{Duration, Instant};

use glass_core::{
    A11yMutationDispatch, A11yThread, Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTarget,
    AxTree, ChangeSignal, Deadline, GlassError, PointerHit, Result, WalkBudget, Whose,
    normalize_description, normalize_name, read_back_confirms, write_took_no_effect,
};
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UIRangeValuePattern, UISelectionItemPattern,
    UITogglePattern, UIValuePattern,
};
use uiautomation::types::{ExpandCollapseState, Handle, Rect, ToggleState};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};

/// Every bounded call runs on a fresh detached thread: UIA is COM and thread-affine, so it must
/// not run on the caller's. The cap is what stops a hung provider blocking the calling tool for
/// longer than it.
static UIA: A11yThread = A11yThread::new("UIA", Duration::from_secs(10));
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
        let ctx = ctx.clone();
        UIA.snapshot(ctx.deadline, move || run_snapshot(&ctx))
    }

    fn subscribe_changes(&mut self, ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        // Unlike `snapshot` above and `set_value`/`invoke` below there is no timeout wrapper: the
        // subscription's own thread is the long-lived one, and `subscribe` already bounds how
        // long it will wait for the registrations to land.
        crate::events::subscribe(ctx)
    }

    fn state_coverage(&self) -> glass_core::AxStateCoverage {
        crate::mapping::STATE_COVERAGE
    }

    fn focus(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let ctx = ctx.clone();
        let target = target.clone();
        focus_with_thread(&UIA, ctx, move |ctx, dispatch| {
            run_focus(&ctx, &target, dispatch)
        })?;
        Ok(None)
    }

    fn pointer_target_at(
        &mut self,
        ctx: &AxContext,
        target: &AxTarget,
        point: (i32, i32),
    ) -> Result<PointerHit> {
        let ctx = ctx.clone();
        let target = target.clone();
        UIA.snapshot(ctx.deadline, move || {
            run_pointer_target_at(&ctx, &target, point)
        })
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let ctx = ctx.clone();
        let target = target.clone();
        let text = text.to_string();
        set_value_with_thread(&UIA, ctx, target.id.0, move |ctx, dispatch| {
            run_set_value(&ctx, &target, &text, dispatch)
        })
    }

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let ctx = ctx.clone();
        let target = target.clone();
        // This reader actuates the element it resolved, so it never substitutes another.
        invoke_with_thread(&UIA, ctx, move |ctx, dispatch| {
            run_invoke(&ctx, &target, dispatch)
        })
        .map(|()| None)
    }
}

fn set_value_with_thread(
    thread: &A11yThread,
    ctx: AxContext,
    target: u32,
    job: impl FnOnce(AxContext, &A11yMutationDispatch) -> Result<()> + Send + 'static,
) -> Result<()> {
    thread.set_value(target, ctx.deadline, move |dispatch| job(ctx, dispatch))
}

fn invoke_with_thread(
    thread: &A11yThread,
    ctx: AxContext,
    job: impl FnOnce(AxContext, &A11yMutationDispatch) -> Result<()> + Send + 'static,
) -> Result<()> {
    thread.invoke(ctx.deadline, move |dispatch| job(ctx, dispatch))
}

fn focus_with_thread(
    thread: &A11yThread,
    ctx: AxContext,
    job: impl FnOnce(AxContext, &A11yMutationDispatch) -> Result<()> + Send + 'static,
) -> Result<()> {
    thread.focus(ctx.deadline, move |dispatch| job(ctx, dispatch))
}

fn uia_err(e: impl std::fmt::Display) -> GlassError {
    GlassError::AccessibilityUnavailable(format!("UI Automation error: {e}"))
}

fn required_uia<T>(result: uiautomation::Result<T>) -> Result<T> {
    result.map_err(uia_err)
}

fn optional_uia<T>(result: uiautomation::Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == 0 => Ok(None),
        Err(error) => Err(uia_err(error)),
    }
}

#[cfg(test)]
fn optional_pattern<T>(result: uiautomation::Result<T>) -> Result<Option<T>> {
    optional_uia(result)
}

fn required_com<T>(
    deadline: Deadline,
    operation: &str,
    call: impl FnOnce() -> uiautomation::Result<T>,
) -> Result<T> {
    ensure_com_deadline(deadline, operation)?;
    required_uia(call())
}

fn optional_com<T>(
    deadline: Deadline,
    operation: &str,
    call: impl FnOnce() -> uiautomation::Result<T>,
) -> Result<Option<T>> {
    ensure_com_deadline(deadline, operation)?;
    optional_uia(call())
}

fn confirmation_com<T>(
    deadline: Deadline,
    operation: &str,
    call: impl FnOnce() -> uiautomation::Result<T>,
) -> Result<T> {
    required_com(deadline, operation, call).map_err(GlassError::after_dispatch)
}

fn ensure_com_deadline(deadline: Deadline, operation: &str) -> Result<()> {
    ensure_com_deadline_at(deadline, Instant::now(), operation)
}

fn ensure_com_deadline_at(deadline: Deadline, now: Instant, operation: &str) -> Result<()> {
    if deadline
        .remaining_at(now)
        .is_some_and(|remaining| remaining.is_zero())
    {
        Err(GlassError::deadline_not_started(operation))
    } else {
        Ok(())
    }
}

fn confirmation_end_at(deadline: Deadline, ceiling: Duration, now: Instant) -> Instant {
    deadline.resolve(now + ceiling).0
}

fn may_request_parent(depth: usize, depth_limit: usize) -> bool {
    depth < depth_limit
}

fn run_snapshot(ctx: &AxContext) -> Result<AxTree> {
    // UIAutomation::new() initializes COM (MTA) on this thread.
    let automation = required_com(ctx.deadline, "initialize UI Automation", UIAutomation::new)?;
    let walker = required_com(ctx.deadline, "get UIA control-view walker", || {
        automation.get_control_view_walker()
    })?;
    let window = find_app_window(&automation, ctx)?;

    let origin = (ctx.window.x, ctx.window.y);
    let mut budget = WalkBudget::with_limits(ctx.limits);
    let root_node = walk(
        &walker,
        &window,
        origin,
        (ctx.window.width, ctx.window.height),
        0,
        &mut budget,
        ctx.deadline,
    )?;
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
    required_com(ctx.deadline, "get UIA element from window handle", || {
        automation.element_from_handle(Handle::from(handle as isize))
    })
}

/// A tree-walker step's result, with a genuine read failure counted on `budget`.
///
/// UIA reports "there is no such element" as `S_OK` with a NULL out-param, which `windows-rs`
/// cannot build an interface from — so absence arrives as an `Err` too. It is separable because
/// that error's code reaches us as **zero** (`windows_result::Error::code` maps its internal
/// empty-error sentinel back to `HRESULT(0)`), while a real failure carries a negative HRESULT,
/// and `uiautomation::Error::result` is `Some` only for a negative code. A plain `.ok()` collapses
/// the two, which is what let a dropped subtree read as an empty one.
fn step(
    deadline: Deadline,
    operation: &str,
    budget: &mut WalkBudget,
    call: impl FnOnce() -> uiautomation::Result<UIElement>,
) -> Result<Option<UIElement>> {
    ensure_com_deadline(deadline, operation)?;
    Ok(call()
        .inspect_err(|e| {
            if e.result().is_some() {
                budget.note_unreadable();
            }
        })
        .ok())
}

/// Recursively build a normalized node, bounded by [`WalkBudget`] (node count, nesting depth,
/// and per-level sibling scan) so a pathological tree can't burn the reader's whole ceiling
/// with no tree to show for it.
fn walk(
    walker: &UITreeWalker,
    el: &UIElement,
    origin: (i32, i32),
    window_size: (u32, u32),
    depth: usize,
    budget: &mut WalkBudget,
    deadline: Deadline,
) -> Result<AxNode> {
    budget.visit();
    let ct_id =
        required_com(deadline, "read UIA control type", || el.get_control_type())? as i32 as u32;
    // `canonical_name` knows every documented control type, mapped or not, so the numeric form
    // is reached only by a vendor-defined or future id — still reported, never dropped.
    let raw_role = crate::mapping::canonical_name(ct_id)
        .map(str::to_string)
        .unwrap_or_else(|| format!("UIA:{ct_id}"));
    let framework = framework_id(el, ct_id, deadline)?;
    let name = nonempty(required_com(deadline, "read UIA name", || el.get_name())?);
    let description = normalize_description(&help_text(el, ct_id, deadline)?, name.as_deref());
    let bounds = window_relative_bounds(el, origin, deadline)?;
    let (mut facts, value) = gather(el, ct_id, deadline)?;
    facts.offscreen |= bounds_are_offscreen(bounds, window_size);
    let states = crate::mapping::map_states(&facts);

    let mut children = Vec::new();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let first_child = step(deadline, "read first UIA child", budget, || {
        walker.get_first_child(el)
    })?;
    // Tests only whether a first child exists before applying the traversal budget. Offscreen
    // elements remain in the normalized tree with `visible = false`; semantic actionability must
    // be able to prove and refuse them rather than making them indistinguishable from absence.
    if first_child.is_some() && budget.may_explore_children(depth) {
        // A virtualized list of thousands (or a cyclic `get_next_sibling`
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
            children.push(walk(
                walker,
                &c,
                origin,
                window_size,
                depth + 1,
                budget,
                deadline,
            )?);
            child = step(deadline, "read next UIA sibling", budget, || {
                walker.get_next_sibling(&c)
            })?;
        }
    }

    Ok(AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role: crate::mapping::map_role_with_framework(ct_id, facts.checkable, framework.as_deref()),
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
/// An unsupported pattern is the binding's explicit non-failure sentinel (`Error::code() == 0`),
/// which [`optional_pattern`] maps to `None`. Every real HRESULT failure propagates so a transient
/// provider error cannot silently change the node's role or state.
fn toggle_pattern(
    el: &UIElement,
    ct_id: u32,
    deadline: Deadline,
) -> Result<Option<UITogglePattern>> {
    if !matches!(ct_id, 50000 | 50002 | 50011 | 50031) {
        // Button/CheckBox/MenuItem/SplitButton
        return Ok(None);
    }
    optional_com(deadline, "get UIA Toggle pattern", || {
        el.get_pattern::<UITogglePattern>()
    })
}

/// Gather state facts + the value string in one pass, gating each pattern probe by control type
/// so we don't make a live cross-process `get_pattern` call for a pattern the control can't support
/// (UIA is chatty — each probe is an out-of-process COM round-trip).
fn gather(
    el: &UIElement,
    ct_id: u32,
    deadline: Deadline,
) -> Result<(crate::mapping::StateFacts, Option<String>)> {
    // Fetch the Toggle pattern once: its mere presence is `checkable` (the control exposes
    // on/off semantics at all), independent of whether we can also read its current state.
    let pattern = toggle_pattern(el, ct_id, deadline)?;
    let checkable = pattern.is_some();
    let toggled_on = match pattern.as_ref() {
        Some(pattern) => {
            required_com(deadline, "read UIA Toggle state", || {
                pattern.get_toggle_state()
            })? == ToggleState::On
        }
        None => false,
    };
    let selected = if matches!(ct_id, 50007 | 50019 | 50024 | 50029) {
        match optional_com(deadline, "get UIA SelectionItem pattern", || {
            el.get_pattern::<UISelectionItemPattern>()
        })? {
            Some(pattern) => required_com(deadline, "read UIA selection state", || {
                pattern.is_selected()
            })?,
            None => false,
        }
    } else {
        false
    };
    let expanded = if matches!(ct_id, 50003 | 50009 | 50011 | 50023 | 50024 | 50026 | 50033) {
        match optional_com(deadline, "get UIA ExpandCollapse pattern", || {
            el.get_pattern::<UIExpandCollapsePattern>()
        })? {
            Some(pattern) => {
                required_com(deadline, "read UIA expanded state", || pattern.get_state())?
                    == ExpandCollapseState::Expanded
            }
            None => false,
        }
    } else {
        false
    };
    // Value pattern: one fetch for both the value string and read-only (Edit/ComboBox/Document)
    let (value_text, readonly) = if matches!(ct_id, 50003 | 50004 | 50030) {
        match optional_com(deadline, "get UIA Value pattern", || {
            el.get_pattern::<UIValuePattern>()
        })? {
            Some(pattern) => (
                Some(required_com(deadline, "read UIA value", || {
                    pattern.get_value()
                })?),
                Some(required_com(deadline, "read UIA read-only state", || {
                    pattern.is_readonly()
                })?),
            ),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    // RangeValue pattern: a Slider/Spinner/ProgressBar exposes its numeric position here, never
    // via ValuePattern, so read it (gated by control type — `get_pattern` is a COM round-trip) so
    // `value_contains`/`wait_for_element` can match the number.
    let value = if value_text.is_some() {
        value_text
    } else if matches!(ct_id, 50012 | 50015 | 50016) {
        match optional_com(deadline, "get UIA RangeValue pattern", || {
            el.get_pattern::<UIRangeValuePattern>()
        })? {
            Some(pattern) => Some(crate::mapping::format_range_value(required_com(
                deadline,
                "read UIA range value",
                || pattern.get_value(),
            )?)),
            None => None,
        }
    } else {
        None
    };
    // Editable iff a writable ValuePattern is present — for ANY value-bearing
    // control (Edit/ComboBox/Document), not just Edit; otherwise a writable
    // ComboBox/Document reports editable=false while set_value would succeed on
    // it. `readonly` is only `Some` for those three types (gated above), so the
    // match keeps non-value controls non-editable.
    let editable =
        matches!(ct_id, 50003 | 50004 | 50030) && readonly.map(|ro| !ro).unwrap_or(false);
    let facts = crate::mapping::StateFacts {
        enabled: required_com(deadline, "read UIA enabled state", || el.is_enabled())?,
        offscreen: required_com(deadline, "read UIA offscreen state", || el.is_offscreen())?,
        focused: required_com(deadline, "read UIA focused state", || {
            el.has_keyboard_focus()
        })?,
        focusable: required_com(deadline, "read UIA focusable state", || {
            el.is_keyboard_focusable()
        })?,
        selected,
        toggled_on,
        expanded,
        editable,
        secure: required_com(deadline, "read UIA password state", || el.is_password())?,
        checkable,
    };
    Ok((facts, value))
}

/// `el`'s UIA `HelpText` — the tooltip, and the secondary label the outline renders as
/// `desc="…"`. Costs one cross-process property read per node.
///
/// A failed read degrades to no description, since one unreadable property must not fail a whole
/// snapshot, but it logs first. The caller deadline is still checked immediately before the COM
/// read, so a detached worker cannot start this optional read after its budget expires.
/// It matters more here than the `.ok()` on a pattern probe: `CurrentHelpText` answers an *unset*
/// property with an empty string, so every `Err` is a genuine COM failure — a stale element, a hung
/// or disconnected provider, a denied cross-integrity read — never "the app set no help text".
///
/// `FullDescription` is UIA's other secondary label; `uiautomation` 0.25 exposes no accessor for it
/// (only `UIProperty::FullDescription` through `get_property_value`), and no probed app was
/// observed carrying one.
fn help_text(el: &UIElement, ct_id: u32, deadline: Deadline) -> Result<String> {
    ensure_com_deadline(deadline, "read UIA help text")?;
    Ok(el.get_help_text().unwrap_or_else(|e| {
        eprintln!(
            "glass-a11y-windows: HelpText read failed on control type {ct_id} \
             (HRESULT {:#010x}: {e}); treating the element as having no description",
            e.code()
        );
        String::new()
    }))
}

/// UIA `BoundingRectangle` (screen) → window-relative `AxRect`, or `None` for zero-area.
fn window_relative_bounds(
    el: &UIElement,
    origin: (i32, i32),
    deadline: Deadline,
) -> Result<Option<AxRect>> {
    let r: Rect = required_com(deadline, "read UIA bounding rectangle", || {
        el.get_bounding_rectangle()
    })?;
    let (w, h) = (r.get_width(), r.get_height());
    if w <= 0 || h <= 0 {
        return Ok(None);
    }
    Ok(Some(AxRect {
        x: r.get_left() - origin.0,
        y: r.get_top() - origin.1,
        width: w as u32,
        height: h as u32,
    }))
}

fn bounds_are_offscreen(bounds: Option<AxRect>, window_size: (u32, u32)) -> bool {
    let Some(bounds) = bounds else {
        return false;
    };
    let left = i64::from(bounds.x);
    let top = i64::from(bounds.y);
    let right = left + i64::from(bounds.width);
    let bottom = top + i64::from(bounds.height);
    right <= 0 || bottom <= 0 || left >= i64::from(window_size.0) || top >= i64::from(window_size.1)
}

fn nonempty(s: String) -> Option<String> {
    normalize_name(&s)
}

/// The element's UIA `FrameworkId`, which `map_role_with_framework` decides a `Document` on.
/// Gated by control type like `toggle_pattern`: only 50030 is decided by it, so no other node
/// spends a cross-process property read. Read per node, never per app — a browser's window
/// element reports `Win32` while the page inside it carries the engine's id. An empty id yields
/// `None`; a failed read propagates so it cannot silently change the mapped role.
fn framework_id(el: &UIElement, ct_id: u32, deadline: Deadline) -> Result<Option<String>> {
    if ct_id != 50030 {
        return Ok(None);
    }
    Ok(nonempty(required_com(
        deadline,
        "read UIA framework id",
        || el.get_framework_id(),
    )?))
}

fn find_target(
    walker: &UITreeWalker,
    window: &UIElement,
    ctx: &AxContext,
    target: &AxTarget,
) -> Result<UIElement> {
    let mut budget = WalkBudget::with_limits(ctx.limits);
    find_nth(walker, window, 0, &mut budget, target.id.0, ctx.deadline)?
        .ok_or(GlassError::AxElementChanged(target.id.0))
}

fn verify_target_fingerprint(el: &UIElement, ctx: &AxContext, target: &AxTarget) -> Result<u32> {
    let ct_id = required_com(ctx.deadline, "read target UIA control type", || {
        el.get_control_type()
    })? as i32 as u32;
    let role = crate::mapping::map_role_with_framework(
        ct_id,
        toggle_pattern(el, ct_id, ctx.deadline)?.is_some(),
        framework_id(el, ct_id, ctx.deadline)?.as_deref(),
    );
    let name = nonempty(required_com(ctx.deadline, "read target UIA name", || {
        el.get_name()
    })?);
    let bounds = window_relative_bounds(el, (ctx.window.x, ctx.window.y), ctx.deadline)?;
    if !target.matches(role, name.as_deref())
        || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
    {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    Ok(ct_id)
}

fn run_focus(ctx: &AxContext, target: &AxTarget, dispatch: &A11yMutationDispatch) -> Result<()> {
    let automation = required_com(
        ctx.deadline,
        "initialize UI Automation for focus",
        UIAutomation::new,
    )?;
    let root = find_app_window(&automation, ctx)?;
    let walker = required_com(
        ctx.deadline,
        "get UIA control-view walker for focus",
        || automation.get_control_view_walker(),
    )?;
    let element = find_target(&walker, &root, ctx, target)?;
    let ct_id = verify_target_fingerprint(&element, ctx, target)?;
    let (facts, _) = gather(&element, ct_id, ctx.deadline)?;
    if !facts.focusable && !facts.editable {
        return Err(GlassError::AxActionUnavailable(target.id.0));
    }
    dispatch.dispatch(|| element.set_focus().map_err(uia_err))
}

fn runtime_id_path(
    automation: &UIAutomation,
    walker: &UITreeWalker,
    root: &UIElement,
    element: &UIElement,
    depth_limit: usize,
    deadline: Deadline,
) -> Result<Vec<Vec<i32>>> {
    let mut path = Vec::new();
    let mut current = element.clone();
    let mut depth = 0usize;
    loop {
        path.push(required_com(deadline, "read UIA runtime id", || {
            current.get_runtime_id()
        })?);
        if required_com(deadline, "compare UIA ancestry element with root", || {
            automation.compare_elements(&current, root)
        })? {
            return Ok(path);
        }
        if !may_request_parent(depth, depth_limit) {
            return Err(GlassError::AccessibilityUnavailable(format!(
                "UI Automation hit ancestry exceeded the configured depth limit ({depth_limit})"
            )));
        }
        let Some(parent) =
            optional_com(deadline, "read UIA parent", || walker.get_parent(&current))?
        else {
            return Ok(path);
        };
        current = parent;
        depth += 1;
    }
}

fn run_pointer_target_at(
    ctx: &AxContext,
    target: &AxTarget,
    point: (i32, i32),
) -> Result<PointerHit> {
    let automation = required_com(
        ctx.deadline,
        "initialize UI Automation for pointer hit probe",
        UIAutomation::new,
    )?;
    let root = find_app_window(&automation, ctx)?;
    let walker = required_com(
        ctx.deadline,
        "get UIA control-view walker for pointer hit probe",
        || automation.get_control_view_walker(),
    )?;
    let element = find_target(&walker, &root, ctx, target)?;
    verify_target_fingerprint(&element, ctx, target)?;

    let screen_x = ctx
        .window
        .x
        .checked_add(point.0)
        .ok_or_else(|| GlassError::Backend("UIA hit-test x coordinate overflow".into()))?;
    let screen_y = ctx
        .window
        .y
        .checked_add(point.1)
        .ok_or_else(|| GlassError::Backend("UIA hit-test y coordinate overflow".into()))?;
    let screen = uiautomation::types::Point::new(screen_x, screen_y);
    let Some(hit) = optional_com(ctx.deadline, "get UIA element from point", || {
        automation.element_from_point(screen)
    })?
    else {
        return Ok(PointerHit::Inconclusive);
    };
    if required_com(ctx.deadline, "compare UIA hit with target", || {
        automation.compare_elements(&hit, &element)
    })? {
        return Ok(PointerHit::Target);
    }

    let hit_ct_id = required_com(ctx.deadline, "read hit UIA control type", || {
        hit.get_control_type()
    })? as i32 as u32;
    let hit_role = crate::mapping::map_role_with_framework(
        hit_ct_id,
        toggle_pattern(&hit, hit_ct_id, ctx.deadline)?.is_some(),
        framework_id(&hit, hit_ct_id, ctx.deadline)?.as_deref(),
    );
    let target_ids = runtime_id_path(
        &automation,
        &walker,
        &root,
        &element,
        ctx.limits.depth,
        ctx.deadline,
    )?;
    let hit_ids = runtime_id_path(
        &automation,
        &walker,
        &root,
        &hit,
        ctx.limits.depth,
        ctx.deadline,
    )?;
    let target_path = target_ids.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let hit_path = hit_ids.iter().map(Vec::as_slice).collect::<Vec<_>>();
    Ok(crate::mapping::classify_uia_hit_path(
        &target_path,
        Some(&hit_path),
        hit_role.is_interactable(),
    ))
}

fn run_set_value(
    ctx: &AxContext,
    target: &AxTarget,
    text: &str,
    dispatch: &A11yMutationDispatch,
) -> Result<()> {
    let automation = required_com(
        ctx.deadline,
        "initialize UI Automation for set-value",
        UIAutomation::new,
    )?;
    let walker = required_com(
        ctx.deadline,
        "get UIA control-view walker for set-value",
        || automation.get_control_view_walker(),
    )?;
    let window = find_app_window(&automation, ctx)?;

    // Start at 0 so find_nth's pre-order numbering matches snapshot's walk +
    // assign_ids (root id = 0); the role+name verify backstops any drift.
    let el = find_target(&walker, &window, ctx, target)?;

    // Verify role + name + bounds (guards a stale id / tree drift). role+name
    // alone isn't unique (many controls share a role and an empty name), so if
    // drift lands a different same-role+name element on this pre-order id, the
    // bounds fingerprint — the element sits elsewhere — rejects it. A target
    // without captured bounds falls back to role+name only.
    verify_target_fingerprint(&el, ctx, target)?;
    let pat = optional_com(ctx.deadline, "get target UIA Value pattern", || {
        el.get_pattern::<UIValuePattern>()
    })?
    .ok_or(GlassError::AxElementNotEditable(target.id.0))?;
    // Pre-write value: the baseline for the "changed" check. A failed pre-read propagates before
    // dispatch rather than weakening confirmation to an unknown baseline.
    let before = required_com(ctx.deadline, "read UIA value before set-value", || {
        pat.get_value()
    })?;
    dispatch.dispatch(|| pat.set_value(text).map_err(uia_err))?;
    // Verify the write took, error-aware. egui/accesskit read-only editables accept SetValue
    // without error but never apply it (false success). Poll the value back — a real numeric set
    // lands a frame later. A failed post-read propagates after dispatch, preserving that the write
    // may already have landed rather than masquerading as successful confirmation.
    let now = Instant::now();
    let (confirmation_end, confirmation_owner) = ctx.deadline.resolve(confirmation_end_at(
        Deadline::UNBOUNDED,
        Duration::from_millis(SET_VALUE_VERIFY_MS),
        now,
    ));
    let mut after = before.clone();
    loop {
        let now = Instant::now();
        if now >= confirmation_end {
            if confirmation_owner == Whose::Caller {
                return Err(
                    GlassError::deadline_not_started("UIA set-value confirmation").after_dispatch(),
                );
            }
            return Err(if write_took_no_effect(&after, Some(&before)) {
                GlassError::value_not_applied_because(
                    target.id.0,
                    text,
                    Some(&after),
                    READ_ONLY_PROJECTION,
                )
            } else {
                GlassError::value_not_applied(target.id.0, text, Some(&after))
            });
        }
        after = confirmation_com(
            Deadline::at(confirmation_end),
            "read UIA value after set-value",
            || pat.get_value(),
        )?;
        if read_back_confirms(Some(&after), Some(&before), text) {
            return Ok(());
        }
        std::thread::sleep(
            Duration::from_millis(VERIFY_POLL_MS)
                .min(confirmation_end.saturating_duration_since(Instant::now())),
        );
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
/// the other three are fire-and-report, exactly as their patterns define. A failed Toggle state
/// read is a real provider error and propagates before dispatch.
///
/// UIA explicitly identifies unsupported patterns with `UIA_E_NOTSUPPORTED`; only that result is
/// treated as absence. Every other pattern lookup failure propagates, so a provider or COM failure
/// cannot silently become pointer fallback.
fn run_invoke(ctx: &AxContext, target: &AxTarget, dispatch: &A11yMutationDispatch) -> Result<()> {
    let automation = required_com(
        ctx.deadline,
        "initialize UI Automation for invoke",
        UIAutomation::new,
    )?;
    let walker = required_com(
        ctx.deadline,
        "get UIA control-view walker for invoke",
        || automation.get_control_view_walker(),
    )?;
    let window = find_app_window(&automation, ctx)?;

    // Start at 0 so find_nth's pre-order numbering matches snapshot's walk + assign_ids, same as
    // run_set_value.
    let el = find_target(&walker, &window, ctx, target)?;

    // Same fingerprint gate as run_set_value: role + name + bounds.
    verify_target_fingerprint(&el, ctx, target)?;

    let fail = |e: uiautomation::Error| GlassError::AxActionFailed(target.id.0, e.to_string());
    if let Some(p) = optional_com(ctx.deadline, "get target UIA Invoke pattern", || {
        el.get_pattern::<UIInvokePattern>()
    })? {
        return dispatch.dispatch(|| p.invoke().map_err(fail));
    }
    if let Some(p) = optional_com(ctx.deadline, "get target UIA Toggle pattern", || {
        el.get_pattern::<UITogglePattern>()
    })? {
        // Toggle is the one rung with a readable post-state, so don't take the ack as proof:
        // a provider that accepts `Toggle()` without applying it would otherwise report a
        // successful click on a control that never moved. Read before, fire, then poll until
        // the state differs, using the same cadence as `run_set_value`'s write verification.
        let before = required_com(ctx.deadline, "read UIA Toggle state before invoke", || {
            p.get_toggle_state()
        })?;
        dispatch.dispatch(|| p.toggle().map_err(fail))?;
        let now = Instant::now();
        let (confirmation_end, confirmation_owner) = ctx.deadline.resolve(confirmation_end_at(
            Deadline::UNBOUNDED,
            Duration::from_millis(SET_VALUE_VERIFY_MS),
            now,
        ));
        loop {
            // Past the dispatch, a failed read IS a failure: `fail` (AxActionFailed) is
            // right here, because the toggle may have landed and must not be re-actuated.
            let now = Instant::now();
            if now >= confirmation_end {
                return Err(if confirmation_owner == Whose::Caller {
                    GlassError::deadline_not_started("UIA Toggle confirmation").after_dispatch()
                } else {
                    GlassError::AxActionFailed(
                        target.id.0,
                        "the toggle action was acknowledged but the state did not change".into(),
                    )
                });
            }
            if confirmation_com(
                Deadline::at(confirmation_end),
                "read UIA Toggle state after invoke",
                || p.get_toggle_state(),
            )? != before
            {
                return Ok(());
            }
            std::thread::sleep(
                Duration::from_millis(VERIFY_POLL_MS)
                    .min(confirmation_end.saturating_duration_since(Instant::now())),
            );
        }
    }
    if let Some(p) = optional_com(ctx.deadline, "get target UIA SelectionItem pattern", || {
        el.get_pattern::<UISelectionItemPattern>()
    })? {
        return dispatch.dispatch(|| p.select().map_err(fail));
    }
    if let Some(p) = optional_com(
        ctx.deadline,
        "get target UIA ExpandCollapse pattern",
        || el.get_pattern::<UIExpandCollapsePattern>(),
    )? {
        let expanded = required_com(ctx.deadline, "read UIA ExpandCollapse state", || {
            p.get_state()
        })? == ExpandCollapseState::Expanded;
        return dispatch
            .dispatch(|| if expanded { p.collapse() } else { p.expand() }.map_err(fail));
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
    deadline: Deadline,
) -> Result<Option<UIElement>> {
    if budget.nodes_walked() == target as usize {
        return Ok(Some(el.clone()));
    }
    budget.visit();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let first_child = optional_com(
        deadline,
        "read first UIA child during target rewalk",
        || walker.get_first_child(el),
    )?;
    // Same child and budget order as `walk`; offscreen nodes participate in ids so visibility can
    // be proved and refused instead of disappearing from semantic resolution.
    if first_child.is_none() || !budget.may_explore_children(depth) {
        return Ok(None);
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
        if let Some(found) = find_nth(walker, &c, depth + 1, budget, target, deadline)? {
            return Ok(Some(found));
        }
        child = optional_com(
            deadline,
            "read next UIA sibling during target rewalk",
            || walker.get_next_sibling(&c),
        )?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uia_property_hresult_is_propagated_instead_of_becoming_false() {
        let error = required_uia::<bool>(Err(uiautomation::Error::new(
            0x8000_4005_u32 as i32,
            "synthetic property failure",
        )))
        .expect_err("a failed required property read must remain an error");

        assert!(
            matches!(error, GlassError::AccessibilityUnavailable(ref message)
                if message.contains("synthetic property failure")),
            "{error}"
        );
    }

    #[test]
    fn uia_pattern_hresult_is_propagated_instead_of_becoming_absent() {
        let error = optional_pattern::<()>(Err(uiautomation::Error::new(
            0x8000_4005_u32 as i32,
            "synthetic pattern failure",
        )))
        .expect_err("a real pattern HRESULT must remain an error");

        assert!(
            matches!(error, GlassError::AccessibilityUnavailable(ref message)
                if message.contains("synthetic pattern failure")),
            "{error}"
        );
    }

    #[test]
    fn only_explicit_unsupported_pattern_result_becomes_absent() {
        let pattern =
            optional_pattern::<()>(Err(uiautomation::Error::new(0, "unsupported pattern"))).expect(
                "the dependency's explicit unsupported-pattern sentinel is not a COM failure",
            );

        assert_eq!(pattern, None);
    }

    #[test]
    fn com_deadline_is_checked_at_the_exact_absolute_instant() {
        let now = Instant::now();
        let error = ensure_com_deadline_at(Deadline::at(now), now, "UIA property read")
            .expect_err("a COM call at the caller deadline must not start");
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
        assert!(
            ensure_com_deadline_at(
                Deadline::at(now + Duration::from_millis(1)),
                now,
                "UIA property read"
            )
            .is_ok()
        );
    }

    #[test]
    fn confirmation_deadline_preserves_the_already_dispatched_mutation() {
        let mut calls = 0;
        let error = confirmation_com(Deadline::at(Instant::now()), "UIA confirmation", || {
            calls += 1;
            Ok(())
        })
        .expect_err("expired confirmation must not issue another COM call");
        assert_eq!(calls, 0);
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert!(!error.invoke_fallback_eligible());
    }

    #[test]
    fn confirmation_uses_the_nearer_caller_deadline_or_operation_ceiling() {
        let now = Instant::now();
        let ceiling = Duration::from_millis(800);
        assert_eq!(
            confirmation_end_at(Deadline::at(now + Duration::from_millis(20)), ceiling, now),
            now + Duration::from_millis(20)
        );
        assert_eq!(
            confirmation_end_at(Deadline::at(now + Duration::from_secs(5)), ceiling, now),
            now + ceiling
        );
        assert_eq!(
            confirmation_end_at(Deadline::UNBOUNDED, ceiling, now),
            now + ceiling
        );
    }

    #[test]
    fn ancestry_limit_stops_before_requesting_one_more_parent() {
        assert!(!may_request_parent(0, 0));
        assert!(may_request_parent(0, 1));
        assert!(!may_request_parent(1, 1));
    }

    #[test]
    fn bounds_outside_the_window_are_offscreen_even_when_the_provider_omits_the_property() {
        assert!(bounds_are_offscreen(
            Some(AxRect {
                x: 20,
                y: 650,
                width: 160,
                height: 32,
            }),
            (700, 500),
        ));
        assert!(!bounds_are_offscreen(
            Some(AxRect {
                x: 20,
                y: 20,
                width: 160,
                height: 32,
            }),
            (700, 500),
        ));
    }

    #[test]
    fn windows_set_value_forwards_ax_context_deadline_to_a11y_thread() {
        let ctx = AxContext {
            pids: vec![],
            window: glass_core::WindowGeometry::default(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: glass_core::WalkLimits::DEFAULT,
            deadline: glass_core::Deadline::from_millis(20),
        };
        let thread = A11yThread::new("test UIA", Duration::from_secs(1));

        let error = set_value_with_thread(&thread, ctx, 7, |_, _| {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        })
        .unwrap_err();

        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(error.bound(), Some(glass_core::BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
        assert!(!error.invoke_fallback_eligible());
        assert!(!error.set_value_failed_after_writing());
    }

    #[test]
    fn windows_set_value_timeout_after_native_dispatch_is_unconfirmed() {
        let ctx = AxContext {
            pids: vec![],
            window: glass_core::WindowGeometry::default(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: glass_core::WalkLimits::DEFAULT,
            deadline: glass_core::Deadline::from_millis(20),
        };
        let thread = A11yThread::new("test UIA", Duration::from_secs(1));

        let error = set_value_with_thread(&thread, ctx, 7, |_, dispatch| {
            dispatch.dispatch(|| {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            })
        })
        .unwrap_err();

        assert!(
            matches!(error, GlassError::AxWriteUnconfirmedCaused { id: 7, .. }),
            "{error}"
        );
        assert!(error.set_value_failed_after_writing(), "{error}");
    }

    #[test]
    fn windows_invoke_timeout_before_native_dispatch_cancels_the_late_action() {
        let ctx = AxContext {
            pids: vec![],
            window: glass_core::WindowGeometry::default(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: glass_core::WalkLimits::DEFAULT,
            deadline: glass_core::Deadline::from_millis(20),
        };
        let thread = A11yThread::new("test UIA", Duration::from_secs(1));
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_invoked = std::sync::Arc::clone(&invoked);

        let error = invoke_with_thread(&thread, ctx, move |_, dispatch| {
            std::thread::sleep(Duration::from_millis(60));
            dispatch.dispatch(|| {
                worker_invoked.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        })
        .expect_err("the caller stops during target resolution");

        assert_eq!(
            error.bound_owner(),
            Some(glass_core::Whose::Caller),
            "{error}"
        );
        assert_eq!(
            error.bound(),
            Some(glass_core::BoundKind::TimedOut),
            "{error}"
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched),
            "{error}"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "the Windows wrapper allowed a detached invoke to dispatch after timeout"
        );
    }

    #[test]
    fn windows_invoke_timeout_after_native_dispatch_remains_may_have_dispatched() {
        let ctx = AxContext {
            pids: vec![],
            window: glass_core::WindowGeometry::default(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: glass_core::WalkLimits::DEFAULT,
            deadline: glass_core::Deadline::from_millis(20),
        };
        let thread = A11yThread::new("test UIA", Duration::from_secs(1));

        let error = invoke_with_thread(&thread, ctx, |_, dispatch| {
            dispatch.dispatch(|| {
                std::thread::sleep(Duration::from_millis(60));
                Ok(())
            })
        })
        .expect_err("the native action outlives the caller");

        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched),
            "{error}"
        );
    }

    /// The failure mode this guards: if an absent child ever stopped arriving as a zero code,
    /// every leaf in every snapshot would report an unreadable subtree. Builds the error
    /// directly rather than driving a walker, so it needs no COM.
    #[test]
    fn an_absent_child_is_not_counted_as_unreadable() {
        let mut budget = WalkBudget::new();
        // Zero is what the real chain yields for an absent child — see `step`.
        let absent = uiautomation::Error::new(0, "no such element");
        assert!(
            step(Deadline::UNBOUNDED, "test child", &mut budget, || Err(
                absent
            ))
            .expect("an absent child is not an error")
            .is_none()
        );
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
        assert!(
            step(Deadline::UNBOUNDED, "test child", &mut budget, || Err(
                failed
            ))
            .expect("a failed child read is accounted for in the walk budget")
            .is_none()
        );
        assert_eq!(budget.unreadable(), 1);
    }
}
