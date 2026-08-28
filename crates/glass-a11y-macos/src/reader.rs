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
    Result, WalkBudget, normalize_name, read_back_confirms, write_took_no_effect,
};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFRetained;

use crate::ffi::{self, attr};
use crate::mapping::{self, AxStateFacts};
use crate::select_diagnostic::{CandidateOutcome, candidate_line};
use crate::semantic_deadline::{EffectiveDeadline, SemanticDeadline};

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

/// What a write that took no effect looks like on this backend: the accessibility API accepts a
/// value the toolkit never applies. Twin of the const in `glass-a11y-windows/src/reader.rs`.
const READ_ONLY_PROJECTION: &str = "this element's accessibility value may be a read-only projection that accepts a write \
     without applying it — focus the element and type into it instead";

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
        let deadline = SemanticDeadline::snapshot(ctx.deadline);
        deadline.require()?;
        let (trusted, deadline) = deadline.dispatch_snapshot(ffi::accessibility_is_trusted)?;
        require_accessibility_grant(trusted)?;
        let (window_el, scale) = resolve_window_after_grant(ctx, deadline)?;

        let mut budget = WalkBudget::with_limits(ctx.limits);
        let root = walk(&window_el, &ctx.window, scale, 0, &mut budget, deadline)?;
        deadline.run(|| {
            let mut tree = AxTree::new(root);
            tree.truncated = budget.truncation();
            tree.unreadable = budget.unreadable();
            // Ids/count are assigned by `glass-core` (`AxTree::assign_ids`) so numbering is
            // identical across OS backends.
            Ok(tree)
        })
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let deadline = SemanticDeadline::set_value(ctx.deadline, target.id.0);
        deadline.require()?;
        let (window_el, scale) = resolve_window(ctx, deadline)?;

        // Start at 0 so `find_nth`'s pre-order numbering matches `snapshot`'s `walk` +
        // `AxTree::assign_ids` (root id = 0); the role+name+bounds fingerprint below
        // backstops any residual drift between the snapshot and this re-walk.
        let mut budget = WalkBudget::with_limits(ctx.limits);
        let found = find_nth(window_el, 0, &mut budget, target.id.0, deadline)?;
        deadline.require()?;
        let el = found.ok_or(GlassError::AxElementNotFound(target.id.0))?;

        // Verify role + name + bounds (guards a stale id / tree drift): if drift landed a
        // different same-role+name element on this pre-order id, its bounds sit elsewhere
        // and it is rejected here rather than silently overwritten.
        let ax_role = deadline
            .observe(|| ffi::attribute_string(&el, attr::ROLE))?
            .unwrap_or_default();
        let subrole = read_subrole(&el, &ax_role, deadline)?;
        let role = mapping::map_role(&ax_role, subrole.as_deref());
        // Same rule `walk` derived this element's `name` from, so a fingerprint can never be
        // computed from a differently-read name and reject an element that never moved.
        let name = read_name(&el, deadline)?;
        let bounds = window_relative_rect(&el, scale, &ctx.window, deadline)?;
        deadline.require()?;
        if !target.matches(role, name.as_deref())
            || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
        {
            return Err(GlassError::AxElementChanged(target.id.0));
        }

        if !deadline.observe(|| ffi::is_settable(&el, attr::VALUE))? {
            return Err(GlassError::AxElementNotEditable(target.id.0));
        }

        // Pre-write value: the baseline for the "changed" check. Use the error-aware read (the
        // same call as the post-read below) so a *present but empty* value stays a known `Some("")`
        // baseline instead of folding to `None` — keeping macOS symmetric with the Windows reader
        // (whose `get_value()` returns `Ok("")` for empty). `None` — a failed or absent pre-read —
        // means the baseline is unknown, and `read_back_confirms` then requires an exact match
        // rather than trusting a "differs from before" signal it cannot compute.
        let before = deadline
            .observe(|| ffi::attribute_string_checked(&el, attr::VALUE))?
            .ok()
            .flatten();
        deadline.dispatch(|| ffi::set_string_value(&el, text))?;
        let dispatched = deadline.after_dispatch();

        // Read-back poll: some editables accept the AX write without an `AXError` but never
        // actually change `AXValue` (a misleading success) — require the read-back to show the
        // change before reporting success, never a silent false-success. Both reads are
        // *error-aware*: a failed or absent post-read is inconclusive and never confirms, so we
        // keep polling to the deadline rather than mistaking a failed read for a change. Keep the
        // complete latest result: a later read supersedes a transient failure, while a terminal
        // failure remains the structured cause of the unconfirmed-write verdict.
        let verification =
            dispatched.phase(Instant::now() + Duration::from_millis(SET_VALUE_VERIFY_MS));
        let mut read_back = WriteVerification::new(target.id.0, text, before.as_deref());
        loop {
            let Some(after) = verification.observe(dispatched, || {
                ffi::attribute_string_checked(&el, attr::VALUE)
            })?
            else {
                return Err(read_back.verdict());
            };
            let confirmed = read_back.observe(after);
            if verification.callee_expired(dispatched)? {
                return Err(read_back.verdict());
            }
            match verification.observe(dispatched, || confirmed)? {
                Some(true) => return Ok(()),
                Some(false) => {}
                None => return Err(read_back.verdict()),
            }
            if !verification.sleep(dispatched, Duration::from_millis(SET_VALUE_POLL_MS))? {
                return Err(read_back.verdict());
            }
        }
    }

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let deadline = SemanticDeadline::invoke(ctx.deadline);
        deadline.require()?;
        let (window_el, scale) = resolve_window(ctx, deadline)?;

        // Start at 0, same numbering rationale as `set_value`. Unlike `set_value` (whose
        // miss is `AxElementNotFound` — the id itself is unknown), a miss here is
        // `AxElementChanged`: it means the tree drifted since the id was captured (same
        // classification the Linux/Windows readers' `invoke` use), which is also what the
        // fingerprint mismatch just below reports.
        let mut budget = WalkBudget::with_limits(ctx.limits);
        let found = find_nth(window_el, 0, &mut budget, target.id.0, deadline)?;
        deadline.require()?;
        let el = found.ok_or(GlassError::AxElementChanged(target.id.0))?;

        // Same fingerprint gate as set_value: role + name + bounds.
        let ax_role = deadline
            .observe(|| ffi::attribute_string(&el, attr::ROLE))?
            .unwrap_or_default();
        let subrole = read_subrole(&el, &ax_role, deadline)?;
        let role = mapping::map_role(&ax_role, subrole.as_deref());
        // `name` derived exactly as in `walk` and `set_value` — see there.
        let name = read_name(&el, deadline)?;
        let bounds = window_relative_rect(&el, scale, &ctx.window, deadline)?;
        deadline.require()?;
        if !target.matches(role, name.as_deref())
            || !target.bounds_consistent(bounds, SET_VALUE_BOUNDS_TOL)
        {
            return Err(GlassError::AxElementChanged(target.id.0));
        }

        if !deadline
            .observe(|| ffi::action_names(&el))?
            .iter()
            .any(|a| a == "AXPress")
        {
            return Err(GlassError::AxActionUnavailable(target.id.0));
        }
        // No post-actuation verify here (unlike the Linux/Windows toggle rungs): AXPress is a
        // generic press with no universal post-state to read back — a checkbox's AXValue, a
        // button's nothing, a menu item's opened menu — so there is nothing to confirm against.
        // Accepted parity gap: a control that accepts AXPress without acting reports success.
        // This reader actuates the element it resolved, so it never substitutes another.
        deadline.dispatch(|| {
            ffi::perform_action(&el, "AXPress")
                .map_err(|error| GlassError::AxActionFailed(target.id.0, error))
        })?;
        Ok(None)
    }
}

struct WriteVerification<'a> {
    id: u32,
    requested: &'a str,
    before: Option<&'a str>,
    latest: Option<Result<Option<String>>>,
}

impl<'a> WriteVerification<'a> {
    fn new(id: u32, requested: &'a str, before: Option<&'a str>) -> Self {
        Self {
            id,
            requested,
            before,
            latest: None,
        }
    }

    fn observe(&mut self, result: Result<Option<String>>) -> bool {
        self.latest = Some(result);
        read_back_confirms(
            self.latest
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(Option::as_deref),
            self.before,
            self.requested,
        )
    }

    fn verdict(self) -> GlassError {
        write_verdict(
            self.id,
            self.requested,
            self.before,
            self.latest.unwrap_or(Ok(None)),
        )
    }
}

fn write_verdict(
    id: u32,
    requested: &str,
    before: Option<&str>,
    after: Result<Option<String>>,
) -> GlassError {
    // A read that failed, or one showing a value the element reformatted, is not evidence of a
    // projection that accepted the write and kept its value.
    match after {
        Err(source) => GlassError::write_unconfirmed_because(
            id,
            "the element exposes a writable value but no readable value was available after the write",
            source,
        ),
        Ok(None) => GlassError::AxWriteUnconfirmed(
            id,
            "the element exposes a writable value but no readable value was available after the write"
                .into(),
        ),
        Ok(Some(seen)) if write_took_no_effect(&seen, before) => GlassError::value_not_applied_because(
            id,
            requested,
            Some(&seen),
            READ_ONLY_PROJECTION,
        ),
        Ok(Some(seen)) => GlassError::value_not_applied(id, requested, Some(&seen)),
    }
}

/// Resolve the `AXWindow` + point→pixel `scale` for a semantic operation that has not yet checked
/// the grant. Snapshot performs that first AX call at its dispatch boundary, then enters
/// [`resolve_window_after_grant`]; mutations use this wrapper while they are still pre-dispatch.
fn resolve_window(
    ctx: &AxContext,
    deadline: SemanticDeadline,
) -> Result<(CFRetained<AXUIElement>, f64)> {
    // Grant gate first — fail closed with an actionable error, never a stub tree.
    require_accessibility_grant(deadline.observe(ffi::accessibility_is_trusted)?)?;

    resolve_window_after_grant(ctx, deadline)
}

fn require_accessibility_grant(trusted: bool) -> Result<()> {
    if trusted {
        Ok(())
    } else {
        Err(GlassError::PermissionDenied {
            which: "Accessibility".into(),
            remedy: ACCESSIBILITY_REMEDY.into(),
        })
    }
}

fn resolve_window_after_grant(
    ctx: &AxContext,
    deadline: SemanticDeadline,
) -> Result<(CFRetained<AXUIElement>, f64)> {
    deadline.require()?;
    let &pid = ctx.pids.first().ok_or(GlassError::WindowNotFound)?;
    let app = deadline.observe(|| ffi::app_element(pid as i32))?;

    // The app's `AXWindows` list can be transiently EMPTY right after launch (the window
    // server registers the AX window a beat after the window exists), so a snapshot taken
    // immediately after `start` races it. Poll ONLY the empty-list case: as soon as any
    // window is present, `select_window` decides the match — and if none fits, that is a real
    // geometry mismatch (it logs its diagnostics), which retrying would not fix, so it is
    // returned immediately rather than polled. A failed `AXWindows` read reads as "no windows"
    // and is likewise retried until the budget, then `WindowNotFound`.
    let resolution =
        deadline.phase(Instant::now() + Duration::from_millis(RESOLVE_WINDOW_BUDGET_MS));
    loop {
        let Some(windows) = resolution.observe(deadline, || ffi::app_windows(&app))? else {
            return Err(GlassError::WindowNotFound);
        };
        let windows = windows.unwrap_or_default();
        if !windows.is_empty() {
            return select_window(&windows, &ctx.window, deadline, resolution)?
                .ok_or(GlassError::WindowNotFound);
        }
        if resolution.callee_expired(deadline)? {
            return Err(GlassError::WindowNotFound);
        }
        if !resolution.sleep(deadline, Duration::from_millis(RESOLVE_WINDOW_POLL_MS))? {
            return Err(GlassError::WindowNotFound);
        }
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
/// that case so a `WindowNotFound` is diagnosable without re-instrumenting — each line also
/// carries the candidate's role and the `AXError` behind any failed read ([`candidate_line`]),
/// so a withheld tree (e.g. a locked screen handing back an `AXApplication`) is distinguishable
/// from a genuine geometry mismatch.
fn select_window(
    windows: &[CFRetained<AXUIElement>],
    win: &WindowGeometry,
    deadline: SemanticDeadline,
    resolution: EffectiveDeadline,
) -> Result<Option<(CFRetained<AXUIElement>, f64)>> {
    let mut best: Option<(i64, CFRetained<AXUIElement>, f64)> = None;
    let mut diagnostics: Vec<String> = Vec::new();
    for w in windows {
        if resolution.callee_expired(deadline)? {
            return Ok(None);
        }
        // Read first, so a candidate that fails every subsequent read still names what it is
        // (#263) — see the doc comment above for why that matters.
        let Some(role) = resolution.observe(deadline, || ffi::attribute_string(w, attr::ROLE))?
        else {
            return Ok(None);
        };
        let role = role.as_deref();
        let Some(size) = resolution.observe(deadline, || ffi::ax_size(w))? else {
            return Ok(None);
        };
        let (ax_w, ax_h) = match size {
            Ok(size) => size,
            Err(e) => {
                diagnostics.push(candidate_line(
                    role,
                    &CandidateOutcome::SizeUnreadable(e.to_string()),
                ));
                continue;
            }
        };
        if ax_w <= 0.0 || ax_h <= 0.0 {
            diagnostics.push(candidate_line(
                role,
                &CandidateOutcome::NonPositiveSize { ax_w, ax_h },
            ));
            continue;
        }
        // macOS backing scale is always an integer; snap out the border/content-vs-frame
        // inset noise in the raw width ratio (see doc comment above).
        let scale = (win.width as f64 / ax_w).round().max(1.0);
        if !scale.is_finite() || scale <= 0.0 {
            diagnostics.push(candidate_line(
                role,
                &CandidateOutcome::InvalidScale { ax_w, ax_h, scale },
            ));
            continue;
        }
        let Some(position) = resolution.observe(deadline, || ffi::ax_position(w))? else {
            return Ok(None);
        };
        let (ax_x, ax_y) = match position {
            Ok(pos) => pos,
            Err(e) => {
                diagnostics.push(candidate_line(
                    role,
                    &CandidateOutcome::PositionUnreadable {
                        ax_w,
                        ax_h,
                        scale,
                        error: e.to_string(),
                    },
                ));
                continue;
            }
        };
        // Cast to `i64` before subtracting so `.abs()` can never wrap (`i32::MIN.abs()`
        // panics) — the same no-overflow discipline `axwindow::within_tolerance` follows.
        let dx = ((ax_x * scale).round() as i64 - i64::from(win.x)).abs();
        let dy = ((ax_y * scale).round() as i64 - i64::from(win.y)).abs();
        diagnostics.push(candidate_line(
            role,
            &CandidateOutcome::Measured {
                ax_x,
                ax_y,
                ax_w,
                ax_h,
                scale,
                dx,
                dy,
            },
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
        if resolution.callee_expired(deadline)? {
            return Ok(None);
        }
        // Fail-closed dev-tool diagnostic (stderr only, no new error variant): a
        // `WindowNotFound` with no clue why is much harder to debug than one that shows
        // exactly how close (or not) each candidate came.
        eprintln!(
            "glass-a11y-macos: select_window found no match for ctx.window={win:?}; candidates: [{}]",
            diagnostics.join("; ")
        );
    }
    if resolution.callee_expired(deadline)? {
        return Ok(None);
    }
    Ok(best.map(|(_, w, scale)| (w, scale)))
}

/// One of the label attributes a node's `name`/`description` come from (`AXTitle`,
/// `AXDescription`, `AXHelp`), read through the *error-aware* [`ffi::attribute_string_checked`]
/// and then folded to `None` when empty — exactly what [`ffi::attribute_string`] returns for a
/// present-but-empty value, so `name` keeps the value it had before this reader sourced a
/// description.
///
/// Checked for [`read_subrole`]'s reason: the direct read folds a genuine failure into the same
/// `None` as an absent attribute, and since most nodes legitimately lack `AXHelp`, the dishonest
/// `None`s would be undetectable. A failure still degrades to `None` — one unreadable attribute
/// must not fail a snapshot — but it logs first. `AXTitle` earns it twice over: it decides `name`,
/// half the `AxTarget` fingerprint `set_value` re-walks against, and which attribute is left to
/// describe the node.
fn read_label(
    el: &AXUIElement,
    attr_name: &str,
    deadline: SemanticDeadline,
) -> Result<Option<String>> {
    match deadline.observe(|| ffi::attribute_string_checked(el, attr_name))? {
        Ok(text) => Ok(text.and_then(|text| normalize_name(&text))),
        Err(err) => {
            eprintln!(
                "glass-a11y-macos: {attr_name} read failed: {err}; treating the element as \
                 having no {attr_name}"
            );
            Ok(None)
        }
    }
}

/// `el`'s `name` as [`walk`] records it: the fingerprint `set_value`/`invoke` re-walk against has
/// to come from the same reads, in the same order, that produced the name in the snapshot.
fn read_name(el: &AXUIElement, deadline: SemanticDeadline) -> Result<Option<String>> {
    let title = read_label(el, attr::TITLE, deadline)?;
    match title {
        Some(title) => Ok(Some(title)),
        None => read_label(el, attr::DESCRIPTION, deadline),
    }
}

/// `el`'s `AXSubrole`, but only for the base roles whose subrole actually changes the mapped
/// role ([`mapping::subrole_matters`]) — every other node skips the AX IPC round-trip and gets
/// `None`.
///
/// The one place this read happens, so the three sites that map a role — [`walk`],
/// `set_value`'s fingerprint and `invoke`'s — cannot gate or read it differently and land on
/// different roles for the same node (the Windows reader shares its `toggle_pattern` helper for
/// the same reason: `set_value` re-walks to an id captured from a snapshot, and a fingerprint
/// computed from a differently-read role would reject an element that never moved).
///
/// Reads through the *error-aware* [`ffi::attribute_string_checked`] rather than
/// [`ffi::attribute_string`], which folds a genuine read failure into the same `None` as a
/// legitimately-absent attribute: an `AXOutlineRow` whose subrole read broke would silently degrade
/// to `ListItem` with nothing to show for it. A failure still degrades to `None` — one unreadable
/// attribute must not fail a whole snapshot — but it logs first, the same diagnostic-not-silence
/// treatment `ffi::children` and `ffi::action_names` already get.
///
/// Known limit, and why that log line matters more since the gate widened: a failed read degrades a
/// switch to the plain `Button` or `CheckBox` its base role names — a valid interactive role rather
/// than an obviously-wrong one, marked nowhere in the emitted tree. Clicking it still works (this
/// backend actuates by pointer or native action, neither consulting the role), so the symptom is a
/// switch an agent cannot select by role or verify by state, not one it cannot press.
fn read_subrole(
    el: &AXUIElement,
    ax_role: &str,
    deadline: SemanticDeadline,
) -> Result<Option<String>> {
    if !mapping::subrole_matters(ax_role) {
        deadline.require()?;
        return Ok(None);
    }
    match deadline.observe(|| ffi::attribute_string_checked(el, attr::SUBROLE))? {
        Ok(sub) => Ok(sub),
        Err(err) => {
            eprintln!(
                "glass-a11y-macos: AXSubrole read failed for role={ax_role:?}: {err}; \
                 treating the element as having no subrole"
            );
            Ok(None)
        }
    }
}

fn read_labels(
    el: &AXUIElement,
    deadline: SemanticDeadline,
) -> Result<(Option<String>, Option<String>)> {
    let title = read_label(el, attr::TITLE, deadline)?;
    if title.is_none() {
        let description = read_label(el, attr::DESCRIPTION, deadline)?;
        let help = read_label(el, attr::HELP, deadline)?;
        return deadline.run(|| Ok(mapping::labels(title, || description, || help)));
    }

    let help = read_label(el, attr::HELP, deadline)?;
    let description = if help.is_none() {
        read_label(el, attr::DESCRIPTION, deadline)?
    } else {
        None
    };
    deadline.run(|| Ok(mapping::labels(title, || description, || help)))
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
    deadline: SemanticDeadline,
) -> Result<AxNode> {
    deadline.require()?;
    budget.visit();

    let ax_role = deadline
        .observe(|| ffi::attribute_string(el, attr::ROLE))?
        .unwrap_or_default();
    let subrole = if ax_role == "AXTextField" {
        deadline.observe(|| ffi::attribute_string(el, attr::SUBROLE))?
    } else {
        read_subrole(el, &ax_role, deadline)?
    };
    let role = mapping::map_role(&ax_role, subrole.as_deref());
    // `raw_role` is normally the same AX role string `map_role` matched on — the token, not
    // `AXRoleDescription`'s localized human phrase ("button" / "bouton"), which is useless as a
    // mapping key. A subrole is appended (`"AXRow/AXOutlineRow"`) only when it *decided* the
    // mapped role. Deliberately not appended otherwise: the gate reads a subrole for every
    // button, and decorating them all would rename `AXButton` to `AXButton/AXCloseButton`
    // across every window's controls and split the role histogram the probe prints.
    //
    // The fallback is conditional because that trade only holds while `AXRole` says something.
    // A custom control reports a generic role — `AXUnknown`, or no `AXRole` at all — and puts
    // what distinguishes it in `AXRoleDescription`, which is then the only descriptor there is.
    // If both are absent `raw_role` stays empty: a "role unknown" signal, not a guaranteed
    // field.
    let raw_role = if ax_role.is_empty() || ax_role == "AXUnknown" {
        deadline
            .observe(|| ffi::attribute_string(el, attr::ROLE_DESCRIPTION))?
            .unwrap_or(ax_role)
    } else {
        match &subrole {
            Some(sub)
                if !sub.is_empty()
                    && mapping::map_role(&ax_role, Some(sub))
                        != mapping::map_role(&ax_role, None) =>
            {
                format!("{ax_role}/{sub}")
            }
            _ => ax_role,
        }
    };
    // Which attribute names the node and which is left to describe it is decided in
    // `mapping::labels`, where the rule — and which reads it declines to make — is unit-tested on
    // any host.
    let (name, description) = read_labels(el, deadline)?;
    let value = deadline.observe(|| ffi::attribute_string(el, attr::VALUE))?;
    let bounds = window_relative_rect(el, scale, win, deadline)?;
    let states = mapping::map_states(&gather_states(
        el,
        role,
        subrole.as_deref() == Some("AXSecureTextField"),
        deadline,
    )?);

    let mut children = Vec::new();
    // `ffi::children` returns `Ok(vec![])` for a legitimately-childless (or absent-
    // `AXChildren`) node and only `Err` for a *real* AX read failure. Degrade a real
    // failure to "no children" so one broken node can't fail the whole snapshot — but log
    // it (mirroring `select_window`'s no-match diagnostic) so the dropped subtree is
    // observable, never silent. Counted on `budget` as well — the log serves whoever reads
    // stderr, the count reaches the agent.
    //
    // Resolved before the gate below: a childless node must never be reported truncated
    // for declining to explore a list that was already empty.
    let child_els = deadline
        .observe(|| ffi::children(el))?
        .unwrap_or_else(|err| {
            budget.note_unreadable();
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
    if !child_els.is_empty() && budget.may_explore_children(depth) {
        // `MAX_NODES` only counts nodes actually entered, and `should_skip` siblings are
        // skipped without entering, so an all-skipped level (a virtualized list of thousands)
        // could otherwise iterate without ever tripping it. `MAX_SIBLINGS` bounds the
        // per-level scan regardless of how many are skipped (mirrors the Windows reader).
        for (scanned, child) in child_els.into_iter().enumerate() {
            deadline.require()?;
            if !budget.may_visit_sibling(scanned) {
                break;
            }
            if !should_skip(&child, deadline)? {
                children.push(walk(&child, win, scale, depth + 1, budget, deadline)?);
            }
        }
    }

    deadline.require()?;
    Ok(AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role,
        raw_role,
        name,
        description,
        value,
        states,
        bounds,
        children,
    })
}

/// Whether to prune `el` from the walk: it has no positive-area geometry (zero-size /
/// collapsed / offscreen), so it is neither clickable nor useful in the outline. A named,
/// reusable predicate so [`find_nth`] prunes identically and its pre-order ids line up with
/// this walk's. A node whose size can't be read is *kept* (its `bounds` become `None`)
/// rather than pruned, so an unreadable-geometry container never silently drops its subtree.
fn should_skip(el: &AXUIElement, deadline: SemanticDeadline) -> Result<bool> {
    Ok(matches!(
        deadline.observe(|| ffi::ax_size(el))?,
        Ok((w, h)) if w <= 0.0 || h <= 0.0
    ))
}

/// Pre-order walk mirroring [`walk`]'s traversal — same `should_skip` predicate, same
/// `AXChildren` order, same bounds via [`WalkBudget::may_explore_children`] — to locate the
/// element at
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
    deadline: SemanticDeadline,
) -> Result<Option<CFRetained<AXUIElement>>> {
    deadline.require()?;
    if budget.nodes_walked() == target as usize {
        return Ok(Some(el));
    }
    budget.visit();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    // Counted and logged exactly as `walk` does — the same failure was diagnosable in one
    // traversal and invisible in the other.
    let child_els = deadline
        .observe(|| ffi::children(&el))?
        .unwrap_or_else(|err| {
            budget.note_unreadable();
            eprintln!(
                "glass-a11y-macos: find_nth: AXChildren read failed: {err}; treating as no children"
            );
            Vec::new()
        });
    // Same gap as `walk`: gated on the raw `child_els`, before `should_skip` runs. A node whose
    // children are all skipped, reached once the budget is spent, still records a truncation
    // though nothing real was declined — left as-is for the same reason: pre-filtering means
    // calling `should_skip` over the whole list, the scan `MAX_SIBLINGS` exists to bound.
    if child_els.is_empty() || !budget.may_explore_children(depth) {
        deadline.require()?;
        return Ok(None);
    }
    // Same per-level bound as walk(), so find_nth can't spin either.
    for (scanned, child) in child_els.into_iter().enumerate() {
        deadline.require()?;
        if !budget.may_visit_sibling(scanned) {
            break;
        }
        if !should_skip(&child, deadline)?
            && let Some(found) = find_nth(child, depth + 1, budget, target, deadline)?
        {
            return Ok(Some(found));
        }
    }
    deadline.require()?;
    Ok(None)
}

/// `el`'s window-relative bounds in pixels, or `None` when position/size can't be read or
/// the element has zero area. Shares `glass_core::coords`'s point→pixel conversion with the
/// capture/input path so a11y bounds and click geometry can't drift.
fn window_relative_rect(
    el: &AXUIElement,
    scale: f64,
    win: &WindowGeometry,
    deadline: SemanticDeadline,
) -> Result<Option<AxRect>> {
    let Ok((pos_x, pos_y)) = deadline.observe(|| ffi::ax_position(el))? else {
        return Ok(None);
    };
    let Ok((size_w, size_h)) = deadline.observe(|| ffi::ax_size(el))? else {
        return Ok(None);
    };
    let g = pixel_geometry_from_content_rect(pos_x, pos_y, size_w, size_h, scale);
    if g.width == 0 || g.height == 0 {
        deadline.require()?;
        return Ok(None);
    }
    deadline.require()?;
    Ok(Some(AxRect {
        x: g.x - win.x,
        y: g.y - win.y,
        width: g.width,
        height: g.height,
    }))
}

/// Gather the plain state facts `mapping::map_states` normalizes: `AXEnabled`/`AXFocused`
/// (boolean attributes), `editable`/`focusable` (whether `AXValue`/`AXFocused` are writable),
/// and — for a checkbox/radio/switch — `checkable`/`checked` derived from `AXValue`
/// (`mapping::checkable_checked`; a determinate 0/1 only, per the #170 invariant, so a mixed or
/// unreadable value claims neither). The remaining facts stay at their defaults — macOS doesn't
/// expose them as simple universal attributes, and the reader never over-claims a state it
/// didn't read.
fn gather_states(
    el: &AXUIElement,
    role: AxRole,
    secure: bool,
    deadline: SemanticDeadline,
) -> Result<AxStateFacts> {
    // Only a checkbox/radio/switch carries a checked state, so read the numeric `AXValue` (an
    // extra AX IPC round-trip) only for those roles — every other node skips it. `ToggleButton`
    // is where a switch lands, whichever base role its toolkit gave it.
    let (checkable, checked) = if mapping::role_carries_checked(role) {
        let value = deadline.observe(|| ffi::attribute_i64(el, attr::VALUE))?;
        if value.is_none() {
            // A control of this role with no readable numeric value claims neither checked nor
            // unchecked (the #170 invariant), so `condition:"checked"` silently matches nothing —
            // say why here, since the tree cannot. Reads through the error-blind `attribute_i64`,
            // so this cannot distinguish an absent value from a failed read.
            eprintln!(
                "glass-a11y-macos: {role:?} has no readable AXValue; \
                 it will report neither checked nor unchecked"
            );
        }
        mapping::checkable_checked(role, value)
    } else {
        deadline.require()?;
        (false, false)
    };
    let enabled = deadline
        .observe(|| ffi::attribute_bool(el, attr::ENABLED))?
        .unwrap_or(false);
    let focused = deadline
        .observe(|| ffi::attribute_bool(el, attr::FOCUSED))?
        .unwrap_or(false);
    let focusable = deadline.observe(|| ffi::is_settable(el, attr::FOCUSED))?;
    let editable = deadline.observe(|| ffi::is_settable(el, attr::VALUE))?;
    deadline.require()?;
    Ok(AxStateFacts {
        enabled,
        focused,
        focusable,
        editable,
        secure,
        checkable,
        checked,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{BoundDispatch, BoundKind, Deadline, WalkLimits, Whose};

    fn context(deadline: Deadline) -> AxContext {
        AxContext {
            pids: vec![1],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 200,
                height: 200,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline,
        }
    }

    fn target() -> AxTarget {
        AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("Note".into()),
            bounds: None,
            value: None,
        }
    }

    #[test]
    fn a_final_backend_read_failure_remains_the_unconfirmed_write_cause() {
        let error = write_verdict(
            1,
            "new",
            Some("old"),
            Err(GlassError::Backend("AXValue read failed".into())),
        );

        assert!(
            matches!(error.cause(), GlassError::Backend(message) if message == "AXValue read failed"),
            "{error}"
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched),
            "{error}"
        );
    }

    #[test]
    fn a_final_tool_read_failure_remains_the_unconfirmed_write_cause() {
        let error = write_verdict(
            1,
            "new",
            Some("old"),
            Err(GlassError::ToolFailed {
                call: "AXValue read".into(),
                said: " transport unavailable \n".into(),
            }),
        );

        assert!(
            matches!(error.cause(), GlassError::ToolFailed { .. }),
            "{error}"
        );
        assert_eq!(error.tool_said(), Some("transport unavailable"), "{error}");
    }

    #[test]
    fn a_final_bounded_read_failure_remains_the_unconfirmed_write_cause() {
        let error = write_verdict(
            1,
            "new",
            Some("old"),
            Err(GlassError::caller_deadline_elapsed("AXValue read")),
        );

        assert!(
            matches!(error.cause(), GlassError::Bounded { .. }),
            "{error}"
        );
        assert_eq!(error.bound(), Some(BoundKind::TimedOut), "{error}");
        assert_eq!(error.bound_owner(), Some(Whose::Caller), "{error}");
    }

    #[test]
    fn a_transient_read_failure_can_be_superseded_by_a_successful_read() {
        let mut verification = WriteVerification::new(1, "new", Some("old"));

        assert!(!verification.observe(Err(GlassError::Backend(
            "transient AXValue read failure".into()
        ))));
        assert!(verification.observe(Ok(Some("new".into()))));
    }

    #[test]
    fn readable_none_without_a_source_stays_source_less() {
        let mut verification = WriteVerification::new(1, "new", Some("old"));
        assert!(!verification.observe(Ok(None)));

        let error = verification.verdict();

        assert!(
            matches!(error, GlassError::AxWriteUnconfirmed(1, _)),
            "{error}"
        );
    }

    #[test]
    fn no_completed_read_stays_source_less() {
        let error = WriteVerification::new(1, "new", Some("old")).verdict();

        assert!(
            matches!(error, GlassError::AxWriteUnconfirmed(1, _)),
            "{error}"
        );
    }

    #[test]
    fn snapshot_with_a_spent_deadline_starts_no_ax_read() {
        let mut a11y = MacosA11y::new();
        let error = a11y
            .snapshot(&context(Deadline::from_millis(0)))
            .expect_err("a spent snapshot must stop before checking AX trust");

        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn set_value_with_a_spent_deadline_cannot_dispatch() {
        let mut a11y = MacosA11y::new();
        let error = a11y
            .set_value(&context(Deadline::from_millis(0)), &target(), "new")
            .expect_err("a spent set_value must stop before resolving the target");

        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn invoke_with_a_spent_deadline_cannot_dispatch() {
        let mut a11y = MacosA11y::new();
        let error = a11y
            .invoke(&context(Deadline::from_millis(0)), &target())
            .expect_err("a spent invoke must stop before resolving the target");

        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }
}
