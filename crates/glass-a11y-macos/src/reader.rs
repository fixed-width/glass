#![forbid(unsafe_code)]
//! `MacosA11y`: the `AXUIElement` accessibility reader behind `glass-core`'s
//! [`Accessibility`] seam. Given the launched app's pid and the active window's pixel
//! geometry (from the display backend), it selects the matching `AXWindow`, recovers the
//! point→pixel scale, and walks the element subtree pre-order into a normalized [`AxTree`]
//! in window-relative pixels.
//!
//! **Runs inline on the caller's thread** — unlike the Linux (AT-SPI) and Windows (UIA)
//! readers, AX has no thread-affinity requirement that forces a worker thread, and the
//! on-box test binary already drives this from the process's true main thread. All `unsafe`
//! FFI lives in [`crate::ffi`]; this module is `unsafe`-free.
//!
//! **Fails closed, never stubs.** A missing Accessibility grant is a
//! [`GlassError::PermissionDenied`]; no matching window (including an empty pid set) is a
//! [`GlassError::WindowNotFound`]. It never returns an empty/placeholder tree.

use std::time::{Duration, Instant};

use glass_core::coords::pixel_geometry_from_content_rect;
use glass_core::platform::WindowGeometry;
use glass_core::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxTarget, AxTree, GlassError,
    Result, TruncationLimit, WalkBudget,
};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFRetained;

use crate::ffi::{self, attr};
use crate::mapping::{self, AxStateFacts};

/// Per-axis pixel tolerance when matching an `AXWindow`'s origin against the backend's
/// reported window origin. Same basis as `axwindow.rs`'s geometry-match fallback. Sized for
/// an already-snapped-to-integer `scale` (see [`select_window`]); the raw width ratio can be
/// off by a few points from border/content-vs-frame insets, which is why the scale is
/// snapped before this tolerance is applied rather than folded into a larger tolerance here.
/// Typed `i64` so the pixel-offset comparison in [`select_window`] stays in `i64` end-to-end
/// (no `.abs()` on an `i32` that could wrap — see there).
const POSITION_TOLERANCE_PX: i64 = 8;
/// Slack (pixels) allowed between the backend's reported window height and the height the
/// width-derived `scale` predicts for the `AXWindow`. The scale is taken from *width*
/// because a title bar makes the AX frame height exceed the captured content height; this
/// slack absorbs that difference (generous enough to cover a title bar + toolbar even at 2x
/// Retina) while still rejecting a window whose height is wildly inconsistent with the
/// scale. Position + width already pin the single-window case; this is a secondary guard.
const HEIGHT_CONSISTENCY_SLACK_PX: f64 = 96.0;

/// Per-edge pixel tolerance for `set_value`'s bounds fingerprint (guards a stale id after
/// tree drift landing a same-role+name element elsewhere) — same basis as the Windows
/// reader's `SET_VALUE_BOUNDS_TOL`.
const SET_VALUE_BOUNDS_TOL: i64 = 12;
/// How long `set_value` polls the `AXValue` read-back for the write to take before declaring
/// it a no-op. Mirrors the Windows reader's `SET_VALUE_VERIFY_MS`.
const SET_VALUE_VERIFY_MS: u64 = 800;
/// Interval between read-back poll attempts.
const SET_VALUE_POLL_MS: u64 = 20;

/// How long [`resolve_window`] polls for the app's first `AXWindow` to register before giving
/// up. The window server publishes a freshly-launched window's AX element a beat after the
/// window exists, so a snapshot taken immediately after `start` can find an empty `AXWindows`
/// list and spuriously `WindowNotFound`; this budget absorbs that startup race while still
/// failing fast for an app that genuinely has no window.
const RESOLVE_WINDOW_BUDGET_MS: u64 = 500;
/// Interval between `AXWindows` poll attempts while waiting out the startup race.
const RESOLVE_WINDOW_POLL_MS: u64 = 40;

/// Remedy text for a missing Accessibility grant. Kept in sync with `glass-macos`'s
/// `permissions.rs` wording (this crate can't depend on that private module).
const ACCESSIBILITY_REMEDY: &str =
    "enable glass in System Settings > Privacy & Security > Accessibility";

/// The macOS accessibility reader. Zero-sized; a fresh AX read is performed per `snapshot`.
#[derive(Debug, Default)]
pub struct MacosA11y;

impl MacosA11y {
    pub fn new() -> Self {
        Self
    }
}

impl Accessibility for MacosA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let (window_el, scale) = resolve_window(ctx)?;

        let mut budget = WalkBudget::with_limits(ctx.limits);
        let root = walk(&window_el, &ctx.window, scale, 0, &mut budget);
        let mut tree = AxTree::new(root);
        tree.truncated = budget.truncation();
        // Ids/count are assigned by `glass-core` (`AxTree::assign_ids`) so numbering is
        // identical across OS backends.
        Ok(tree)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let (window_el, scale) = resolve_window(ctx)?;

        // Start at 0 so `find_nth`'s pre-order numbering matches `snapshot`'s `walk` +
        // `AxTree::assign_ids` (root id = 0); the role+name+bounds fingerprint below
        // backstops any residual drift between the snapshot and this re-walk.
        let mut budget = WalkBudget::with_limits(ctx.limits);
        let el = find_nth(window_el, 0, &mut budget, target.id.0)
            .ok_or(GlassError::AxElementNotFound(target.id.0))?;

        // Verify role + name + bounds (guards a stale id / tree drift): if drift landed a
        // different same-role+name element on this pre-order id, its bounds sit elsewhere
        // and it is rejected here rather than silently overwritten.
        let ax_role = ffi::attribute_string(&el, attr::ROLE).unwrap_or_default();
        let role = mapping::map_role(&ax_role);
        let name = ffi::attribute_string(&el, attr::TITLE)
            .or_else(|| ffi::attribute_string(&el, attr::DESCRIPTION));
        let bounds = window_relative_rect(&el, scale, &ctx.window);
        if !target.matches(role, name.as_deref())
            || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
        {
            return Err(GlassError::AxElementChanged(target.id.0));
        }

        if !ffi::is_settable(&el, attr::VALUE) {
            return Err(GlassError::AxElementNotEditable(target.id.0));
        }

        // Pre-write value: the baseline for the "changed" check. Use the error-aware read (the
        // same call as the post-read below) so a *present but empty* value stays a known `Some("")`
        // baseline instead of folding to `None` — keeping macOS symmetric with the Windows reader
        // (whose `get_value()` returns `Ok("")` for empty). `None` — a failed or absent pre-read —
        // means the baseline is unknown, and `read_back_confirms` then requires an exact match
        // rather than trusting a "differs from before" signal it cannot compute.
        let before = ffi::attribute_string_checked(&el, attr::VALUE)
            .ok()
            .flatten();
        ffi::set_string_value(&el, text)?;

        // Read-back poll: some editables accept the AX write without an `AXError` but never
        // actually change `AXValue` (a misleading success) — require the read-back to show the
        // change before reporting success, never a silent false-success. Both reads are
        // *error-aware*: a failed or absent post-read maps to `None`, which is inconclusive and
        // never confirms, so we keep polling to the deadline rather than mistaking a failed read
        // for a change.
        let deadline = Instant::now() + Duration::from_millis(SET_VALUE_VERIFY_MS);
        loop {
            let after = ffi::attribute_string_checked(&el, attr::VALUE)
                .ok()
                .flatten();
            if read_back_confirms(after.as_deref(), before.as_deref(), text) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(GlassError::AxValueNotApplied(target.id.0));
            }
            std::thread::sleep(Duration::from_millis(SET_VALUE_POLL_MS));
        }
    }

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<()> {
        let (window_el, scale) = resolve_window(ctx)?;

        // Start at 0, same numbering rationale as `set_value`. Unlike `set_value` (whose
        // miss is `AxElementNotFound` — the id itself is unknown), a miss here is
        // `AxElementChanged`: it means the tree drifted since the id was captured (same
        // classification the Linux/Windows readers' `invoke` use), which is also what the
        // fingerprint mismatch just below reports.
        let mut budget = WalkBudget::with_limits(ctx.limits);
        let el = find_nth(window_el, 0, &mut budget, target.id.0)
            .ok_or(GlassError::AxElementChanged(target.id.0))?;

        // Same fingerprint gate as set_value: role + name + bounds.
        let ax_role = ffi::attribute_string(&el, attr::ROLE).unwrap_or_default();
        let role = mapping::map_role(&ax_role);
        let name = ffi::attribute_string(&el, attr::TITLE)
            .or_else(|| ffi::attribute_string(&el, attr::DESCRIPTION));
        let bounds = window_relative_rect(&el, scale, &ctx.window);
        if !target.matches(role, name.as_deref())
            || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
        {
            return Err(GlassError::AxElementChanged(target.id.0));
        }

        if !ffi::action_names(&el).iter().any(|a| a == "AXPress") {
            return Err(GlassError::AxActionUnavailable(target.id.0));
        }
        // No post-actuation verify here (unlike the Linux/Windows toggle rungs): AXPress is a
        // generic press with no universal post-state to read back — a checkbox's AXValue, a
        // button's nothing, a menu item's opened menu — so there is nothing to confirm against.
        // Accepted parity gap: a control that accepts AXPress without acting reports success.
        ffi::perform_action(&el, "AXPress").map_err(|e| GlassError::AxActionFailed(target.id.0, e))
    }
}

/// Resolve the `AXWindow` + point→pixel `scale` for `ctx`: the grant gate, pid, app element,
/// and window selection `snapshot` and `set_value` both need — shared so both address the
/// identical window.
fn resolve_window(ctx: &AxContext) -> Result<(CFRetained<AXUIElement>, f64)> {
    // Grant gate first — fail closed with an actionable error, never a stub tree.
    if !ffi::accessibility_is_trusted() {
        return Err(GlassError::PermissionDenied {
            which: "Accessibility".into(),
            remedy: ACCESSIBILITY_REMEDY.into(),
        });
    }

    let &pid = ctx.pids.first().ok_or(GlassError::WindowNotFound)?;
    let app = ffi::app_element(pid as i32);

    // The app's `AXWindows` list can be transiently EMPTY right after launch (the window
    // server registers the AX window a beat after the window exists), so a snapshot taken
    // immediately after `start` races it. Poll ONLY the empty-list case: as soon as any
    // window is present, `select_window` decides the match — and if none fits, that is a real
    // geometry mismatch (it logs its diagnostics), which retrying would not fix, so it is
    // returned immediately rather than polled. A failed `AXWindows` read reads as "no windows"
    // and is likewise retried until the budget, then `WindowNotFound`.
    let deadline = Instant::now() + Duration::from_millis(RESOLVE_WINDOW_BUDGET_MS);
    loop {
        let windows = ffi::app_windows(&app).unwrap_or_default();
        if !windows.is_empty() {
            return select_window(&windows, &ctx.window).ok_or(GlassError::WindowNotFound);
        }
        if Instant::now() >= deadline {
            return Err(GlassError::WindowNotFound);
        }
        std::thread::sleep(Duration::from_millis(RESOLVE_WINDOW_POLL_MS));
    }
}

/// Select the `AXWindow` matching the backend's reported `win` and recover its point→pixel
/// `scale`. The scale is derived from *width* (`win.width / ax_width_pts`) then snapped to
/// the nearest integer, floored at `1.0`: macOS backing scale is always an integer (1x or
/// 2x Retina), so a fractional raw ratio (e.g. `396 / 400 = 0.99`, from a `win` that is the
/// window's *content* rect vs. the `AXWindow`'s *frame* rect) is border/inset noise, not a
/// real scale — snapping it removes that noise before the position gate below runs. The
/// window matches when its `AXPosition` (scaled to pixels) lands within
/// [`POSITION_TOLERANCE_PX`] of `win`'s origin AND its height is consistent with that scale
/// (within [`HEIGHT_CONSISTENCY_SLACK_PX`]). Among candidates, the closest origin wins.
/// `None` when nothing matches (fail closed); logs each candidate's geometry to stderr in
/// that case so a `WindowNotFound` is diagnosable without re-instrumenting.
fn select_window(
    windows: &[CFRetained<AXUIElement>],
    win: &WindowGeometry,
) -> Option<(CFRetained<AXUIElement>, f64)> {
    let mut best: Option<(i64, CFRetained<AXUIElement>, f64)> = None;
    let mut diagnostics: Vec<String> = Vec::new();
    for w in windows {
        let Ok((ax_w, ax_h)) = ffi::ax_size(w) else {
            diagnostics.push("<AXSize unreadable>".into());
            continue;
        };
        if ax_w <= 0.0 || ax_h <= 0.0 {
            diagnostics.push(format!("ax_w={ax_w} ax_h={ax_h} <non-positive size>"));
            continue;
        }
        // macOS backing scale is always an integer; snap out the border/content-vs-frame
        // inset noise in the raw width ratio (see doc comment above).
        let scale = (win.width as f64 / ax_w).round().max(1.0);
        if !scale.is_finite() || scale <= 0.0 {
            diagnostics.push(format!(
                "ax_w={ax_w} ax_h={ax_h} scale={scale} <invalid scale>"
            ));
            continue;
        }
        let Ok((ax_x, ax_y)) = ffi::ax_position(w) else {
            diagnostics.push(format!(
                "ax_w={ax_w} ax_h={ax_h} scale={scale} <AXPosition unreadable>"
            ));
            continue;
        };
        // Cast to `i64` before subtracting so `.abs()` can never wrap (`i32::MIN.abs()`
        // panics) — the same no-overflow discipline `axwindow::within_tolerance` follows.
        let dx = ((ax_x * scale).round() as i64 - i64::from(win.x)).abs();
        let dy = ((ax_y * scale).round() as i64 - i64::from(win.y)).abs();
        diagnostics.push(format!(
            "ax=({ax_x}, {ax_y}, {ax_w}, {ax_h}) scale={scale} dx={dx} dy={dy}"
        ));
        if dx > POSITION_TOLERANCE_PX || dy > POSITION_TOLERANCE_PX {
            continue;
        }
        if (win.height as f64 - ax_h * scale).abs() > HEIGHT_CONSISTENCY_SLACK_PX {
            continue;
        }
        let dist = dx + dy;
        if best
            .as_ref()
            .is_none_or(|(best_dist, _, _)| dist < *best_dist)
        {
            best = Some((dist, w.clone(), scale));
        }
    }
    if best.is_none() {
        // Fail-closed dev-tool diagnostic (stderr only, no new error variant): a
        // `WindowNotFound` with no clue why is much harder to debug than one that shows
        // exactly how close (or not) each candidate came.
        eprintln!(
            "glass-a11y-macos: select_window found no match for ctx.window={win:?}; candidates: [{}]",
            diagnostics.join(", ")
        );
    }
    best.map(|(_, w, scale)| (w, scale))
}

/// Pre-order walk: build this element's [`AxNode`], then recurse into its (non-skipped)
/// children in array order. `budget` tracks the running node total and records which
/// bound (if any) stopped the walk early — shared across the whole walk, and with
/// [`find_nth`], so the two traversals stay in lockstep.
fn walk(
    el: &AXUIElement,
    win: &WindowGeometry,
    scale: f64,
    depth: usize,
    budget: &mut WalkBudget,
) -> AxNode {
    budget.visit();

    let ax_role = ffi::attribute_string(el, attr::ROLE).unwrap_or_default();
    let role = mapping::map_role(&ax_role);
    // `AXRoleDescription` is the human-readable role ("button", "text field"); fall back to the
    // raw AX role string when it's absent. If both are absent (an element exposing neither)
    // `raw_role` is the empty string — a "role unknown" signal, not a guaranteed-populated
    // field.
    let raw_role = ffi::attribute_string(el, attr::ROLE_DESCRIPTION).unwrap_or(ax_role);
    // Name = title, else description — both stable labels (e.g. `setAccessibilityLabel`
    // surfaces as `AXDescription`). Never fold in `AXValue`: it's volatile content, and a
    // node's name must stay stable for the `AxTarget` fingerprint `set_value` relies on.
    let name = ffi::attribute_string(el, attr::TITLE)
        .or_else(|| ffi::attribute_string(el, attr::DESCRIPTION));
    let value = ffi::attribute_string(el, attr::VALUE);
    let bounds = window_relative_rect(el, scale, win);
    let states = mapping::map_states(&gather_states(el, role));

    let mut children = Vec::new();
    // `ffi::children` returns `Ok(vec![])` for a legitimately-childless (or absent-
    // `AXChildren`) node and only `Err` for a *real* AX read failure. Degrade a real
    // failure to "no children" so one broken node can't fail the whole snapshot — but log
    // it (mirroring `select_window`'s no-match diagnostic) so the dropped subtree is
    // observable, never silent. This is a different condition from a bound firing (below),
    // and already reports itself via the log line, so it never touches `budget`.
    //
    // Resolved before the gate below: a childless node must never be reported truncated
    // for declining to explore a list that was already empty.
    let child_els = ffi::children(el).unwrap_or_else(|err| {
        eprintln!(
            "glass-a11y-macos: walk: AXChildren read failed for role={raw_role:?} \
             bounds={bounds:?}: {err}; treating as no children"
        );
        Vec::new()
    });
    // Gated on the raw `child_els`, not filtered by `should_skip` first. A node whose children
    // are all skipped, reached once the node/depth budget is spent, still records a truncation
    // though nothing real was declined. Pre-filtering would mean calling `should_skip` — a live
    // AX round trip — over the whole list, exactly the scan `MAX_SIBLINGS` below exists to bound.
    if !child_els.is_empty() && may_explore_children(budget, depth) {
        // `MAX_NODES` only counts nodes actually entered, and `should_skip` siblings are
        // skipped without entering, so an all-skipped level (a virtualized list of thousands)
        // could otherwise iterate without ever tripping it. `MAX_SIBLINGS` bounds the
        // per-level scan regardless of how many are skipped (mirrors the Windows reader).
        let mut siblings = 0usize;
        for child in child_els {
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
            if !should_skip(&child) {
                children.push(walk(&child, win, scale, depth + 1, budget));
            }
        }
    }

    AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role,
        raw_role,
        name,
        value,
        states,
        bounds,
        children,
    }
}

/// Whether to prune `el` from the walk: it has no positive-area geometry (zero-size /
/// collapsed / offscreen), so it is neither clickable nor useful in the outline. A named,
/// reusable predicate so [`find_nth`] prunes identically and its pre-order ids line up with
/// this walk's. A node whose size can't be read is *kept* (its `bounds` become `None`)
/// rather than pruned, so an unreadable-geometry container never silently drops its subtree.
fn should_skip(el: &AXUIElement) -> bool {
    matches!(ffi::ax_size(el), Ok((w, h)) if w <= 0.0 || h <= 0.0)
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

/// Pre-order walk mirroring [`walk`]'s traversal — same `should_skip` predicate, same
/// `AXChildren` order, same bounds via [`may_explore_children`] — to locate the element at
/// pre-order index `target`. That is the same numbering `glass_core::AxTree::assign_ids`
/// gives the tree `snapshot` returns (root = 0), so a `target.id` captured from a snapshot
/// lands on the same element here. `budget` doubles as the running id (a node's id is
/// `budget.nodes_walked()`'s value on arrival, before [`WalkBudget::visit`]) and the node
/// bound, identically to `walk`. Takes (and, on a mismatch, drops) ownership of each
/// candidate rather than borrowing, since a matched child must outlive the `Vec` of siblings
/// `ffi::children` returns.
fn find_nth(
    el: CFRetained<AXUIElement>,
    depth: usize,
    budget: &mut WalkBudget,
    target: u32,
) -> Option<CFRetained<AXUIElement>> {
    if budget.nodes_walked() == target as usize {
        return Some(el);
    }
    budget.visit();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let child_els = ffi::children(&el).unwrap_or_default();
    // Same gap as `walk`: gated on the raw `child_els`, before `should_skip` runs. A node whose
    // children are all skipped, reached once the budget is spent, still records a truncation
    // though nothing real was declined — left as-is for the same reason: pre-filtering means
    // calling `should_skip` over the whole list, the scan `MAX_SIBLINGS` exists to bound.
    if child_els.is_empty() || !may_explore_children(budget, depth) {
        return None;
    }
    let mut siblings = 0usize;
    for child in child_els {
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
        if !should_skip(&child) {
            if let Some(found) = find_nth(child, depth + 1, budget, target) {
                return Some(found);
            }
        }
    }
    None
}

/// `el`'s window-relative bounds in pixels, or `None` when position/size can't be read or
/// the element has zero area. Shares `glass_core::coords`'s point→pixel conversion with the
/// capture/input path so a11y bounds and click geometry can't drift.
fn window_relative_rect(el: &AXUIElement, scale: f64, win: &WindowGeometry) -> Option<AxRect> {
    let (pos_x, pos_y) = ffi::ax_position(el).ok()?;
    let (size_w, size_h) = ffi::ax_size(el).ok()?;
    let g = pixel_geometry_from_content_rect(pos_x, pos_y, size_w, size_h, scale);
    if g.width == 0 || g.height == 0 {
        return None;
    }
    Some(AxRect {
        x: g.x - win.x,
        y: g.y - win.y,
        width: g.width,
        height: g.height,
    })
}

/// Gather the plain state facts `mapping::map_states` normalizes: `AXEnabled`/`AXFocused`
/// (boolean attributes), `editable`/`focusable` (whether `AXValue`/`AXFocused` are writable),
/// and — for a checkbox/radio/switch — `checkable`/`checked` derived from `AXValue`
/// (`mapping::checkable_checked`; a determinate 0/1 only, per the #170 invariant, so a mixed or
/// unreadable value claims neither). The remaining facts stay at their defaults — macOS doesn't
/// expose them as simple universal attributes, and the reader never over-claims a state it
/// didn't read.
fn gather_states(el: &AXUIElement, role: AxRole) -> AxStateFacts {
    // Only a checkbox/radio/switch carries a checked state, so read the numeric `AXValue` (an
    // extra AX IPC round-trip) only for those roles — every other node skips it. `map_role`
    // maps an `NSSwitch` to `CheckBox`, so switches are covered.
    let (checkable, checked) = match role {
        AxRole::CheckBox | AxRole::RadioButton => {
            mapping::checkable_checked(role, ffi::attribute_i64(el, attr::VALUE))
        }
        _ => (false, false),
    };
    AxStateFacts {
        enabled: ffi::attribute_bool(el, attr::ENABLED).unwrap_or(false),
        focused: ffi::attribute_bool(el, attr::FOCUSED).unwrap_or(false),
        focusable: ffi::is_settable(el, attr::FOCUSED),
        editable: ffi::is_settable(el, attr::VALUE),
        checkable,
        checked,
        ..Default::default()
    }
}

/// Whether a `set_value` write actually took, judged from the value read back. Some
/// read-only-in-practice editables accept the AX write without an `AXError` but never change
/// `AXValue` (a misleading success); a real set changes the value, possibly to a reformatted
/// string. So it took iff the read-back equals the request OR differs from the pre-set
/// value. Mirrors the Windows reader's `set_value_took`.
fn set_value_took(before: &str, after: &str, requested: &str) -> bool {
    after == requested || after != before
}

/// Whether a read-back poll can *confirm* a `set_value` write took. `read_back` is the value
/// read after the write (`None` if that read failed or the attribute was absent); `before` is
/// the pre-write baseline (`None` if the pre-read failed/was absent — baseline unknown).
/// Confirms only when it can prove the write landed: a `None` read-back is inconclusive and
/// never confirms; with a known baseline it delegates to [`set_value_took`]; with an unknown
/// baseline only an exact match with the request confirms — "changed from before" is meaningless
/// without a trustworthy baseline. Mirrors the Windows reader.
fn read_back_confirms(read_back: Option<&str>, before: Option<&str>, requested: &str) -> bool {
    match (read_back, before) {
        (None, _) => false,
        (Some(after), Some(before)) => set_value_took(before, after, requested),
        (Some(after), None) => after == requested,
    }
}

#[cfg(test)]
mod tests {
    use glass_core::{MAX_DEPTH, MAX_NODES};

    use super::{
        may_explore_children, read_back_confirms, set_value_took, TruncationLimit, WalkBudget,
    };

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
        // A read-only editable: value unchanged AND not the requested text.
        assert!(!set_value_took("hello", "hello", "world"));
    }

    #[test]
    fn exact_match_took() {
        assert!(set_value_took("hello", "world", "world"));
    }

    #[test]
    fn reformatted_change_took() {
        // The AX field may normalize the written text — changed from before, so it took.
        assert!(set_value_took("0", "0.0", "0"));
    }

    #[test]
    fn setting_current_value_is_taken() {
        // Edge case: requesting the value it already holds → equals request → taken.
        assert!(set_value_took("world", "world", "world"));
    }

    #[test]
    fn read_back_rejects_a_failed_post_read() {
        // A failed/absent post-write read (None) is inconclusive — never a false success.
        assert!(!read_back_confirms(None, Some("hello"), "world"));
    }

    #[test]
    fn read_back_confirms_change_against_known_baseline() {
        // Known baseline + value changed from it → took (delegates to set_value_took).
        assert!(read_back_confirms(Some("0.0"), Some("0"), "0"));
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
