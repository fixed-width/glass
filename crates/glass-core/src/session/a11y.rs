//! `Glass` accessibility ops: snapshot, marks, click, and set-value.
use super::*;

/// A checkable element wider than this multiple of its height is treated as "row-shaped". On a
/// backend that frames a switch as its whole row (`Platform::a11y_toggle_control_at_trailing_edge`),
/// `click_element` swipes a row-shaped checkable's trailing control instead of clicking the row
/// center.
const ROW_ASPECT: u32 = 4;

/// Duration of the trailing-toggle swipe (ms) — long enough for idb's HID swipe to register as a
/// pan on a UISwitch; matches the proven ~250ms on-device swipe.
const TOGGLE_SWIPE_MS: u64 = 250;

/// Disclosed as the click method's fallback reason when `set_combo_value` opens a dropdown:
/// that one click is pointer-only by design, not because the native action was unavailable.
const COMBO_OPEN_POINTER_REASON: &str =
    "combo popup opened by pointer so the keyboard commit lands in it";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetValueExecution {
    AlreadyApplied,
    DispatchedAndConfirmed,
}

fn click_method_from_semantic_outcome(outcome: SemanticActionOutcome) -> ClickMethod {
    match outcome.action.method {
        ActionMethod::NativeAction { actuated } => ClickMethod::NativeAction { actuated },
        ActionMethod::Pointer {
            native_fallback: Some(native_fallback),
        } => ClickMethod::Pointer { native_fallback },
        ActionMethod::Pointer {
            native_fallback: None,
        } => unreachable!("legacy auto click always records its native fallback reason"),
        ActionMethod::AccessibilityValue | ActionMethod::Keyboard => {
            unreachable!("click actions cannot return a non-click method")
        }
    }
}

fn glass_error_from_semantic_action(mut error: Box<SemanticActionError>) -> GlassError {
    error
        .source
        .take()
        .expect("legacy ID action failures always retain their original GlassError")
}

/// Poll cadence for `set_value`'s post-toggle verify: how often to re-snapshot while waiting for
/// the swiped switch to read back the wanted state.
const TOGGLE_VERIFY_INTERVAL_MS: u64 = 50;
/// Bound on the post-toggle verify poll — generous enough to absorb a lagging a11y-tree update
/// under load without turning a real actuation failure into an indefinite hang.
const TOGGLE_VERIFY_TIMEOUT_MS: u64 = 2000;

fn may_use_cached_geometry(allow: bool, error: &GlassError) -> bool {
    allow
        && error.bound() == Some(crate::BoundKind::NotStarted)
        && error.bound_owner() == Some(crate::Whose::Caller)
        && error.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched)
}

fn legacy_window_probe_is_best_effort(deadline: Deadline) -> bool {
    deadline == Deadline::UNBOUNDED
}

fn popup_settle_exceeds_remaining(
    remaining: Option<std::time::Duration>,
    settle: std::time::Duration,
) -> bool {
    remaining.is_some_and(|left| left < settle)
}

impl ActiveSession {
    fn accessibility_context(
        &self,
        window: WindowGeometry,
        deadline: Deadline,
    ) -> Result<AxContext> {
        Ok(AxContext {
            pids: self.platform.app_pids_by(deadline)?,
            window,
            window_handle: self.platform.active_window_handle(),
            a11y_bus_addr: self.platform.a11y_bus_addr(),
            limits: self.a11y_limits,
            deadline,
        })
    }
}

impl Glass {
    /// Install caller-selected accessibility walk limits for operations that begin with a fresh
    /// tree read. Internal re-snapshots reuse these limits.
    pub(crate) fn set_a11y_limits(&mut self, max_nodes: Option<usize>) -> Result<()> {
        self.active_mut()?.a11y_limits = WalkLimits::from_max_nodes(max_nodes);
        Ok(())
    }

    /// Snapshot the active window's accessibility tree (normalized, window-
    /// relative, ids assigned by the core). Caches it for `click_element`.
    /// `AxUnsupported` if the backend has no accessibility reader.
    pub fn a11y_snapshot(&mut self, max_nodes: Option<usize>) -> Result<AxTree> {
        self.set_a11y_limits(max_nodes)?;
        // The tool takes no timeout, so the reader keeps its own budget. Inventing one here would
        // cap a plain snapshot at a number no caller asked for.
        self.snapshot_at_current_limits(Deadline::UNBOUNDED)
    }

    /// Subscribe to the backend's change notifications for the active app, if it has any.
    ///
    /// Callers hold the returned signal in a local, never in the session: a poll loop's tick
    /// closure borrows `self` mutably, so a signal stored here could not be waited on from the
    /// pause between ticks.
    ///
    /// `deadline` is passed for the handshake, which spends the caller's budget before the poll
    /// loop starts. No reader bounds its subscription by it yet — the two that have event streams
    /// bound their own registration — so today it only travels.
    pub(crate) fn subscribe_a11y_changes(
        &mut self,
        deadline: Deadline,
    ) -> Option<Box<dyn ChangeSignal>> {
        let s = self.active_mut().ok()?;
        // Reader check first: the accessors below are platform round-trips (`app_pids` shells
        // out to `adb` on Android).
        s.accessibility.as_ref()?;
        // Cached geometry deliberately: a failed window round-trip must degrade to polling,
        // not to an error.
        let ctx = s.accessibility_context(s.geometry.clone(), deadline).ok()?;
        s.accessibility.as_mut()?.subscribe_changes(&ctx)
    }

    /// Re-snapshot the active window reusing the limits the last user snapshot set — it does NOT
    /// reset them. Used by compound operations (the `return:"snapshot"` fold, `wait_for_element`,
    /// and the scroll/combo/toggle loops) so an agent working in a raised-cap tree keeps that
    /// id-space instead of silently reverting to the default cap on the next internal snapshot.
    ///
    /// `deadline` is the enclosing operation's bound, not this walk's — see [`Deadline`]. A
    /// caller with no timeout of its own passes [`Deadline::UNBOUNDED`].
    pub fn a11y_resnapshot(&mut self, deadline: Deadline) -> Result<AxTree> {
        self.snapshot_at_current_limits(deadline)
    }

    /// A final quiet-wait read may use cached geometry only after a proven pre-dispatch geometry
    /// failure.
    pub(crate) fn a11y_resnapshot_for_wait(&mut self, deadline: Deadline) -> Result<AxTree> {
        self.snapshot_at_current_limits_with_wait_fallback(deadline, true)
    }

    /// The snapshot worker: walks the active window's tree bounded by the session's current
    /// `a11y_limits` and caches it. Callers install user-selected limits through
    /// [`Glass::set_a11y_limits`] first (or reuse them).
    fn snapshot_at_current_limits(&mut self, deadline: Deadline) -> Result<AxTree> {
        self.snapshot_at_current_limits_with_wait_fallback(deadline, false)
    }

    fn snapshot_at_current_limits_with_wait_fallback(
        &mut self,
        deadline: Deadline,
        allow_spent_geometry: bool,
    ) -> Result<AxTree> {
        let s = self.active_mut()?;
        // Reader-presence check up front (mirrors set_value_inner) so `AxUnsupported` keeps
        // precedence over — and a reader-less backend skips — the geometry round-trip below.
        if s.accessibility.is_none() {
            return Err(GlassError::AxUnsupported);
        }
        // Re-read: an app can resize itself (open a sidebar / panel) with no glass_window op, and
        // stale geometry maps the tree to the old bounds and clips elements now beyond them.
        // macOS resolves this window via ScreenCaptureKit, so a momentarily off-screen window
        // fails here. Android reports a cached fullscreen window, so a freeform self-resize
        // would not refresh.
        let window = match s.platform.window_by(&WindowOp::Geometry, deadline) {
            Ok(window) => window,
            Err(error) if may_use_cached_geometry(allow_spent_geometry, &error) => {
                s.geometry.clone()
            }
            Err(error) => return Err(error),
        };
        s.geometry = window.clone();
        let ctx = s.accessibility_context(window, deadline)?;
        let acc = s.accessibility.as_mut().ok_or(GlassError::AxUnsupported)?;
        let mut tree = acc.snapshot(&ctx)?;
        tree.assign_ids();
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed(
                "accessibility snapshot",
            ));
        }
        let cached = tree.clone();
        s.pump();
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed(
                "accessibility snapshot",
            ));
        }
        s.last_ax = Some(cached);
        Ok(tree)
    }

    /// Capture the active window and overlay numbered marks on its interactable
    /// accessibility elements. Returns the annotated frame and the marks legend.
    /// Caches the snapshot, so `click_element` resolves a mark's id afterward.
    pub fn a11y_marks(&mut self) -> Result<(Frame, Vec<Mark>)> {
        let frame = self.screenshot(None, None)?;
        let tree = self.a11y_resnapshot(Deadline::UNBOUNDED)?;
        Ok(crate::marks::render(&frame, &tree))
    }

    /// Click the element with id `id` from the most recent `a11y_snapshot`. Tries the
    /// platform's native accessibility action first (works for occluded/off-screen/boundless
    /// elements, and on some backends never moves the pointer); when that's unavailable or
    /// fails, falls back to the normal synthetic-pointer path — the center of the element's
    /// bounds, or a swipe across the trailing control for a row-shaped checkable on a
    /// trailing-toggle backend (see [`AxRect::trailing_toggle_swipe`]). The returned
    /// [`ClickMethod`] says which path actually fired.
    pub fn click_element(&mut self, id: AxNodeId) -> Result<ClickMethod> {
        self.click_element_by(id, Deadline::UNBOUNDED)
    }

    pub fn click_element_by(&mut self, id: AxNodeId, deadline: Deadline) -> Result<ClickMethod> {
        self.click_target_by(
            &ClickTargetParams {
                target: ActionTarget::Id(id),
                mode: ActionMode::Auto,
                timeout_ms: None,
                max_nodes: None,
            },
            deadline,
        )
        .map(click_method_from_semantic_outcome)
        .map_err(glass_error_from_semantic_action)
    }

    /// Record an internal pointer-only click that does not pass through [`Glass::click_target_by`].
    /// Public semantic and ID clicks are audited by that high-level owner. The combo open flow
    /// remains here because it deliberately bypasses native invoke, while a trailing-toggle click
    /// inside `set_value_inner` is already accounted for by the enclosing `SetValue` record.
    fn audited_click(
        &mut self,
        id: AxNodeId,
        click: impl FnOnce(&mut Self, AxNodeId) -> Result<ClickMethod>,
    ) -> Result<ClickMethod> {
        let t = std::time::Instant::now();
        let element = self.element_ref(id);
        let result = click(self, id);
        let dispatch = match &result {
            Ok(_) => DispatchStatus::Dispatched.as_str(),
            Err(error) => match error.bound_dispatch() {
                Some(crate::BoundDispatch::NotDispatched) => DispatchStatus::NotDispatched.as_str(),
                Some(crate::BoundDispatch::MayHaveDispatched) | None => {
                    DispatchStatus::MayHaveDispatched.as_str()
                }
            },
        };
        let confirmation = if result.is_ok() {
            ConfirmationStatus::DispatchConfirmed.as_str()
        } else {
            ConfirmationStatus::Unconfirmed.as_str()
        };
        self.emit_audit(
            &crate::audit::Actuation::ClickElement {
                element,
                mode: ActionMode::Pointer.as_str(),
                method: result.as_ref().ok().map(ClickMethod::label),
                native_fallback: result.as_ref().ok().and_then(ClickMethod::native_fallback),
                actuated_id: result
                    .as_ref()
                    .ok()
                    .and_then(ClickMethod::actuated)
                    .map(|id| id.0),
                dispatch,
                confirmation,
            },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn click_element_inner(&mut self, id: AxNodeId, deadline: Deadline) -> Result<ClickMethod> {
        self.click_target_inner(
            ClickTargetParams {
                target: ActionTarget::Id(id),
                mode: ActionMode::Auto,
                timeout_ms: None,
                max_nodes: None,
            },
            deadline,
        )
        .map(click_method_from_semantic_outcome)
        .map_err(glass_error_from_semantic_action)
    }

    /// The synthetic-pointer half of [`Glass::click_element`], on its own: the center of the
    /// element's bounds — routed into the owning popover window when the element renders in
    /// one, or swiped across the trailing control for a row-shaped checkable on a
    /// trailing-toggle backend. Callable directly by an internal caller that must NOT take
    /// the native action (see [`Glass::set_combo_value`]).
    pub(super) fn click_element_pointer_only(
        &mut self,
        id: AxNodeId,
        plan: Option<&super::semantic_action::PlannedPointerInput>,
        deadline: Deadline,
    ) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("click element"));
        }
        let (bounds, checkable, trailing_toggle_backend, active_geo) = {
            let s = self.require_active()?;
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let bounds = node.bounds.ok_or(GlassError::AxElementNotClickable(id.0))?;
            (
                bounds,
                node.states.checkable,
                s.platform.a11y_toggle_control_at_trailing_edge(),
                s.geometry.clone(),
            )
        };
        // a11y bounds are relative to the active window, but the element may render in a
        // separate popover window (an open dropdown's option list) whose own origin they
        // don't reflect.
        //
        // Untimed clicks retain best-effort geometry; bounded actions propagate query failures.
        let windows = match self.list_windows_by(deadline) {
            Ok(windows) => windows,
            Err(_) if legacy_window_probe_is_best_effort(deadline) => Vec::new(),
            Err(error) => return Err(error),
        };
        if let Some(popover_id) = owning_popover(bounds, &active_geo, &windows) {
            let popover_geo = windows
                .iter()
                .find(|w| w.id == popover_id)
                .map(|w| w.geometry.clone())
                .ok_or(GlassError::WindowNotFound)?;
            let container = {
                let s = self.require_active()?;
                let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
                menu_container_bounds(tree, id, &popover_geo)
            }
            .ok_or(GlassError::AxElementInUnmappedPopover(id.0))?;
            let prev = restorable_window(&windows, &active_geo)?;
            if let Err(primary) = self.select_window_by(popover_id, deadline) {
                if primary.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched) {
                    return Err(primary);
                }
                let restore = self.select_window_by(prev, Deadline::UNBOUNDED);
                return Err(match restore {
                    Ok(_) => primary.after_dispatch(),
                    Err(restore) => GlassError::WindowRestoreFailed {
                        primary: Box::new(primary),
                        restore: Box::new(restore),
                    }
                    .after_dispatch(),
                });
            }
            let primary = match plan {
                Some(super::semantic_action::PlannedPointerInput::TrailingToggle {
                    segment,
                    ..
                }) => self.pointer_inner_by(
                    &PointerEvent::Drag {
                        from_x: segment.from_x - container.x,
                        from_y: segment.from_y - container.y,
                        to_x: segment.to_x - container.x,
                        to_y: segment.to_y - container.y,
                        duration_ms: TOGGLE_SWIPE_MS,
                        button: MouseButton::Left,
                        modifiers: vec![],
                    },
                    deadline,
                ),
                planned => {
                    let point = match planned {
                        Some(super::semantic_action::PlannedPointerInput::Click { point }) => {
                            *point
                        }
                        _ => (bounds.x, bounds.y),
                    };
                    self.pointer_inner_by(
                        &PointerEvent::Click {
                            x: point.0 - container.x,
                            y: point.1 - container.y,
                            button: MouseButton::Left,
                            count: 1,
                            modifiers: vec![],
                        },
                        deadline,
                    )
                }
            };
            // Restore focus even after expiry; temporary selection already makes failure
            // after-dispatch.
            let restore = self.select_window_by(prev, Deadline::UNBOUNDED);
            return match (primary, restore) {
                (Ok(()), Ok(_)) => Ok(()),
                (Err(primary), Ok(_)) => Err(primary.after_dispatch()),
                (Ok(()), Err(restore)) => Err(restore.after_dispatch()),
                (Err(primary), Err(restore)) => Err(GlassError::WindowRestoreFailed {
                    primary: Box::new(primary),
                    restore: Box::new(restore),
                }
                .after_dispatch()),
            };
        }
        // On a backend that frames a switch as its whole row with the control at the trailing
        // edge (iOS/idb), a center tap lands on the inert label — and a `UISwitch` does NOT
        // actuate on a tap even when aimed at the control, it needs a short swipe (see
        // `AxRect::trailing_toggle_swipe`).
        //
        // Gate on the backend capability, NOT geometry alone: a wide labeled checkbox on a
        // desktop backend is row-shaped too, but its indicator is at the LEADING edge.
        let planned_segment = match plan {
            Some(super::semantic_action::PlannedPointerInput::TrailingToggle {
                segment, ..
            }) => Some(*segment),
            _ => None,
        };
        let row_shaped_toggle = checkable
            && trailing_toggle_backend
            && bounds.width > bounds.height.saturating_mul(ROW_ASPECT);
        if planned_segment.is_some() || (plan.is_none() && row_shaped_toggle) {
            let seg = planned_segment
                .or_else(|| bounds.trailing_toggle_swipe(active_geo.width, active_geo.height))
                .ok_or(GlassError::AxElementNotClickable(id.0))?;
            self.pointer_inner_by(
                &PointerEvent::Drag {
                    from_x: seg.from_x,
                    from_y: seg.from_y,
                    to_x: seg.to_x,
                    to_y: seg.to_y,
                    duration_ms: TOGGLE_SWIPE_MS,
                    button: MouseButton::Left,
                    modifiers: vec![],
                },
                deadline,
            )
        } else {
            let (x, y) = match plan {
                Some(super::semantic_action::PlannedPointerInput::Click { point }) => *point,
                _ => bounds
                    .clamped_center(active_geo.width, active_geo.height)
                    .ok_or(GlassError::AxElementNotClickable(id.0))?,
            };
            self.pointer_inner_by(
                &PointerEvent::Click {
                    x,
                    y,
                    button: MouseButton::Left,
                    count: 1,
                    modifiers: vec![],
                },
                deadline,
            )
        }
    }

    /// One native-invoke attempt: the same fingerprint + context assembly as
    /// `set_value_inner` (over freshly re-read window geometry — see below), then the
    /// backend's `invoke`. Passes on the element the backend actuated when it is not `id`.
    pub(super) fn try_native_invoke(
        &mut self,
        id: AxNodeId,
        deadline: Deadline,
    ) -> Result<Option<AxNodeId>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility action",
            ));
        }
        {
            let s = self.active_mut()?;
            // Reader-presence check up front (mirrors `snapshot_at_current_limits`) so
            // `AxUnsupported` keeps precedence over — and a reader-less backend skips — the
            // geometry round-trip below.
            if s.accessibility.is_none() {
                return Err(GlassError::AxUnsupported);
            }
            // Re-read the window geometry: the Windows/macOS `invoke` fingerprints the element
            // by window-RELATIVE bounds derived from `ctx.window`, so a window moved since the
            // snapshot reads as drift and rejects.
            //
            // Keep legacy untimed clicks best-effort. A bounded action propagates the query's
            // structured deadline/error instead of silently continuing with stale geometry.
            match s.platform.window_by(&WindowOp::Geometry, deadline) {
                Ok(window) => s.geometry = window,
                Err(_) if legacy_window_probe_is_best_effort(deadline) => {}
                Err(error) => return Err(error),
            }
        }
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility action",
            ));
        }
        let (target, ctx) = {
            let s = self.require_active()?;
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let target = AxTarget {
                id,
                role: node.role,
                name: node.name.clone(),
                bounds: node.bounds,
                value: node.value.clone(),
            };
            let ctx = s.accessibility_context(s.geometry.clone(), deadline)?;
            (target, ctx)
        };
        let s = self.active_mut()?;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility action",
            ));
        }
        let actuated = s
            .accessibility
            .as_mut()
            .ok_or(GlassError::AxUnsupported)?
            .invoke(&ctx, &target)?;
        s.pump();
        Ok(actuated)
    }

    /// One native-focus attempt with the same fresh geometry, target fingerprint, and deadline
    /// ordering as [`Self::try_native_invoke`].
    pub(super) fn try_native_focus(
        &mut self,
        id: AxNodeId,
        deadline: Deadline,
    ) -> Result<Option<AxNodeId>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility focus",
            ));
        }
        {
            let s = self.active_mut()?;
            if s.accessibility.is_none() {
                return Err(GlassError::AxUnsupported);
            }
            match s.platform.window_by(&WindowOp::Geometry, deadline) {
                Ok(window) => s.geometry = window,
                Err(_) if legacy_window_probe_is_best_effort(deadline) => {}
                Err(error) => return Err(error),
            }
        }
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility focus",
            ));
        }
        let (target, ctx) = {
            let s = self.require_active()?;
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let target = AxTarget {
                id,
                role: node.role,
                name: node.name.clone(),
                bounds: node.bounds,
                value: node.value.clone(),
            };
            let ctx = s.accessibility_context(s.geometry.clone(), deadline)?;
            (target, ctx)
        };
        let s = self.active_mut()?;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility focus",
            ));
        }
        let focused = s
            .accessibility
            .as_mut()
            .ok_or(GlassError::AxUnsupported)?
            .focus(&ctx, &target)?;
        s.pump();
        Ok(focused)
    }

    /// Set the value/text of element `id` (from the latest `a11y_snapshot`) via the
    /// platform a11y API. Errors `NoAxSnapshot`/`AxElementNotFound` (id not in the
    /// cached snapshot), `AxUnsupported` (no reader), or — from the backend —
    /// `AxElementNotEditable`/`AxElementChanged`.
    pub fn set_value(&mut self, id: AxNodeId, text: &str) -> Result<()> {
        self.set_value_by(id, text, Deadline::UNBOUNDED)
    }

    pub fn set_value_by(&mut self, id: AxNodeId, text: &str, deadline: Deadline) -> Result<()> {
        self.set_value_target_by(
            &SetValueTargetParams {
                target: ActionTarget::Id(id),
                timeout_ms: None,
                max_nodes: None,
            },
            text,
            deadline,
        )
        .map(|_| ())
        .map_err(glass_error_from_semantic_action)
    }

    pub(super) fn set_value_inner(
        &mut self,
        id: AxNodeId,
        text: &str,
        deadline: Deadline,
    ) -> Result<SetValueExecution> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("set value"));
        }
        {
            let s = self.active_mut()?;
            // Reader-presence check up front so `AxUnsupported` keeps precedence over — and a
            // reader-less backend skips — the geometry round-trip below.
            if s.accessibility.is_none() {
                return Err(GlassError::AxUnsupported);
            }
            // Re-read the window geometry: the Windows/macOS `set_value` fingerprints the
            // element by window-RELATIVE bounds derived from `ctx.window`, so a window moved
            // since the snapshot reads as drift and rejects.
            //
            // Keep legacy untimed writes best-effort. A bounded action propagates the query's
            // structured deadline/error instead of silently continuing with stale geometry.
            match s.platform.window_by(&WindowOp::Geometry, deadline) {
                Ok(window) => s.geometry = window,
                Err(_) if legacy_window_probe_is_best_effort(deadline) => {}
                Err(error) => return Err(error),
            }
        }
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("set value"));
        }
        let (target, ctx) = {
            let s = self.require_active()?;
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let target = AxTarget {
                id,
                role: node.role,
                name: node.name.clone(),
                bounds: node.bounds,
                value: node.value.clone(),
            };
            let ctx = s.accessibility_context(s.geometry.clone(), deadline)?;
            (target, ctx)
        };
        // A combo has no committing accessibility write: its `Selection` interface moves only
        // the popup's *preview* selection, and the model commits on row activation (Enter/click).
        //
        // Invalidate the cache unless input is proven pre-dispatch; a snapshot replaces it.
        if target.role == AxRole::ComboBox {
            let already_applied = self
                .require_active()?
                .last_ax
                .as_ref()
                .and_then(|tree| tree.find(id))
                .is_some_and(|combo| {
                    !combo.states.expanded
                        && combo
                            .name
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case(text.trim()))
                });
            if already_applied {
                return Ok(SetValueExecution::AlreadyApplied);
            }
            self.set_combo_value(id, &target, text, deadline)?;
            return Ok(SetValueExecution::DispatchedAndConfirmed);
        }
        // iOS's value-set (tap+type) can't drive a checkable: a tap doesn't toggle a UISwitch
        // and there's no text to type, so it takes the trailing-edge swipe instead.
        //
        // Gate on `checkable` alone, NOT row-shape: a checkable that isn't row-shaped must
        // fail-safe through this branch rather than fall through to the delegate below, which
        // would silently tap the inert label and type into nothing.
        //
        // DROP the `&self` borrow before `click_element_inner`, which needs `&mut self`.
        let (trailing, node_state) = {
            let s = self.require_active()?;
            (
                s.platform.a11y_toggle_control_at_trailing_edge(),
                s.last_ax
                    .as_ref()
                    .and_then(|t| t.find(id))
                    .map(|n| n.states),
            )
        };
        if trailing
            && let Some(st) = node_state
            && st.checkable
        {
            // Unrecognized text must NOT fall through to the tap+type delegate, which would
            // silently no-op a UISwitch — and the error has to name "boolean", or a generic
            // "use keystrokes" sends the agent down a futile path.
            let want = parse_bool(text)
                .ok_or_else(|| GlassError::AxValueNotBoolean(id.0, text.to_string()))?;
            if st.checked == want {
                return Ok(SetValueExecution::AlreadyApplied); // truthful no-op, no actuation
            }
            let actuation = self.click_element_inner(id, deadline);
            // Drop the pre-toggle cache after ambiguous actuation to prevent a reversing retry.
            self.invalidate_ax_cache_after_possible_dispatch(actuation)?;
            // iOS has no event stream, so polling uses the nearer caller/verification deadline
            // and never accepts state read after expiry.
            let now = std::time::Instant::now();
            let (budget, whose) = deadline.budget(
                std::time::Duration::from_millis(TOGGLE_VERIFY_TIMEOUT_MS),
                now,
            );
            let verify_deadline = Deadline::at(now + budget);
            let mut seen = None;
            loop {
                if verify_deadline.has_passed() {
                    break;
                }
                let tree = match self.a11y_resnapshot(verify_deadline) {
                    Ok(tree) => tree,
                    Err(error) => {
                        if let Some(active) = self.active.as_mut() {
                            active.last_ax = None;
                        }
                        if whose == crate::deadline::Whose::Callee
                            && error.bound_owner() == Some(crate::Whose::Caller)
                        {
                            break;
                        }
                        return Err(GlassError::after_dispatch(error));
                    }
                };
                if verify_deadline.has_passed() {
                    break;
                }
                seen = find_checkable_near(&tree.root, target.bounds.as_ref())
                    .map(|n| n.states.checked);
                if seen == Some(want) {
                    return Ok(SetValueExecution::DispatchedAndConfirmed);
                }
                let left = verify_deadline.remaining().unwrap_or_default();
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(
                    left.min(std::time::Duration::from_millis(TOGGLE_VERIFY_INTERVAL_MS)),
                );
            }
            // Every verification snapshot refreshed `last_ax`, but none proved the requested
            // post-toggle state. Keep retries fail-safe after either terminal deadline.
            if let Some(active) = self.active.as_mut() {
                active.last_ax = None;
            }
            if whose == crate::deadline::Whose::Caller {
                return Err(GlassError::caller_deadline_elapsed("toggle verification"));
            }
            return {
                // `None` is no checkable near the target's bounds on the last tick — the swipe
                // moved the screen — so it reports as a reading nobody took, not as a state.
                Err(GlassError::value_not_applied_because(
                    id.0,
                    text,
                    seen.map(|on| if on { "on" } else { "off" }),
                    "the swipe across the control's trailing edge did not move it",
                ))
            };
        }
        let s = self.active_mut()?;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("set value"));
        }
        let result = s
            .accessibility
            .as_mut()
            .ok_or(GlassError::AxUnsupported)?
            .set_value(&ctx, &target, text);
        if let Err(e) = result {
            // A failure after dispatch doesn't mean the field is unchanged: Android types before
            // it verifies, so a partial type or the AVD's placeholder-on-clear can still have
            // altered it. Invalidate rather than gate, so a retry with no intervening snapshot
            // isn't rejected as drift.
            if e.set_value_failed_after_writing()
                && let Some(node) = s.last_ax.as_mut().and_then(|t| t.find_mut(id))
            {
                node.value = None;
            }
            return Err(e);
        }
        // `Ok`'s guarantee varies by backend (see `AxTarget::value`), but the requested text
        // still beats the definitely-stale pre-write value. Patch by id; a re-snapshot would
        // cost a whole walk for one field.
        if let Some(node) = s.last_ax.as_mut().and_then(|t| t.find_mut(id)) {
            node.value = (!text.is_empty()).then(|| text.to_string());
        }
        s.pump();
        Ok(SetValueExecution::DispatchedAndConfirmed)
    }

    /// Select an option in a dropdown/combo by label (case-insensitive). Opens the popup when
    /// needed, arrow-navigates from the current selection to the target, and presses Enter to
    /// commit — verifying the button label changed (else `AxValueNotApplied`).
    fn set_combo_value(
        &mut self,
        id: AxNodeId,
        target: &AxTarget,
        text: &str,
        deadline: Deadline,
    ) -> Result<()> {
        let want = text.trim();
        let mut mutation_may_have_dispatched = false;
        let expanded_options = {
            let s = self.require_active()?;
            s.last_ax
                .as_ref()
                .and_then(|tree| tree.find(id))
                .filter(|combo| combo.states.expanded)
                .map(collect_combo_options)
        };
        // A matching closed combo is done; an expanded combo still needs Return to commit.
        if expanded_options.is_none()
            && target
                .name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(want))
        {
            return Ok(());
        }
        let options = match expanded_options {
            Some(options) if !options.is_empty() => options,
            expanded_options => {
                if expanded_options.is_none() {
                    // Use a pointer click because UIA programmatic expand does not move keyboard
                    // focus to the popup.
                    let open = self.audited_click(id, |g, id| {
                        g.click_element_pointer_only(id, None, deadline).map(|()| {
                            ClickMethod::Pointer {
                                native_fallback: COMBO_OPEN_POINTER_REASON.into(),
                            }
                        })
                    });
                    self.invalidate_ax_cache_after_possible_dispatch(open)?;
                    mutation_may_have_dispatched = true;
                }
                self.settle_for_popup(deadline)
                    .map_err(GlassError::after_dispatch)?;
                // Ids don't survive a re-snapshot, so match the open (`expanded`) combo, else the one
                // nearest the target's bounds.
                let tree = self
                    .a11y_resnapshot(deadline)
                    .map_err(GlassError::after_dispatch)?;
                let combo = find_expanded_combo(&tree.root)
                    .or_else(|| find_combo_near(&tree.root, target.bounds.as_ref()))
                    .ok_or(GlassError::AxElementChanged(id.0))?;
                collect_combo_options(combo)
            }
        };
        if options.is_empty() {
            return Err(GlassError::AxElementNotEditable(id.0));
        }
        let target_idx = options
            .iter()
            .position(|(label, _)| label.eq_ignore_ascii_case(want));
        let Some(target_idx) = target_idx else {
            // Unknown option — dismiss the popup so the UI is left neutral, then report.
            if !deadline.has_passed() {
                let escape = self.semantic_key_by(&KeyEvent::Chord("Escape".to_string()), deadline);
                mutation_may_have_dispatched |= match &escape {
                    Ok(()) => true,
                    Err(error) => {
                        error.bound_dispatch() != Some(crate::BoundDispatch::NotDispatched)
                    }
                };
                let _ = self.invalidate_ax_cache_after_possible_dispatch(escape);
            }
            let choices = options
                .iter()
                .map(|(l, _)| l.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let error = GlassError::AxOptionNotFound(id.0, text.to_string(), choices);
            return Err(if mutation_may_have_dispatched {
                error.after_dispatch()
            } else {
                error.before_dispatch()
            });
        };
        // Opening focuses the current selection; step from it to the target, then Enter.
        let current_idx = options.iter().position(|(_, sel)| *sel).unwrap_or(0);
        let delta = target_idx as i32 - current_idx as i32;
        // `is_negative` and `< 0` differ only at delta == 0, where the loop runs zero times and
        // the chord is never sent — not observable either way.
        let chord = if delta.is_negative() { "Up" } else { "Down" };
        for _ in 0..delta.unsigned_abs() {
            let selection = self.semantic_key_by(&KeyEvent::Chord(chord.to_string()), deadline);
            self.invalidate_ax_cache_after_possible_dispatch(selection)
                .map_err(GlassError::after_dispatch)?;
        }
        let commit = self.semantic_key_by(&KeyEvent::Chord("Return".to_string()), deadline);
        self.invalidate_ax_cache_after_possible_dispatch(commit)
            .map_err(GlassError::after_dispatch)?;
        self.settle_for_popup(deadline)
            .map_err(GlassError::after_dispatch)?;
        // Verify the model actually committed — the *target* combo (matched by bounds,
        // now closed so nothing is `expanded`) must read the wanted label.
        let tree = self
            .a11y_resnapshot(deadline)
            .map_err(GlassError::after_dispatch)?;
        let (shows, collapsed) = find_combo_near(&tree.root, target.bounds.as_ref())
            .map(|combo| (combo.name.clone(), !combo.states.expanded))
            .unwrap_or((None, false));
        if collapsed
            && shows
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(want))
        {
            Ok(())
        } else {
            // A combo carries its selection as its name, so that is the read-back; `None` is the
            // combo no longer being where it was. `text` rather than the trimmed `want`, so the
            // caller sees what it asked for, as `AxOptionNotFound` above does.
            Err(GlassError::value_not_applied_because(
                id.0,
                text,
                shows.as_deref(),
                "the option was stepped to and committed, and the combo still shows another",
            ))
        }
    }

    fn invalidate_ax_cache_after_possible_dispatch<T>(&mut self, result: Result<T>) -> Result<T> {
        // Only an explicit pre-dispatch verdict proves the cached pre-mutation tree is still true.
        let may_have_dispatched = match &result {
            Ok(_) => true,
            Err(error) => error.bound_dispatch() != Some(crate::BoundDispatch::NotDispatched),
        };
        if may_have_dispatched && let Some(active) = self.active.as_mut() {
            active.last_ax = None;
        }
        result
    }

    /// Let a just-opened/closed popup realize in the a11y tree before re-reading.
    fn settle_for_popup(&self, deadline: Deadline) -> Result<()> {
        let settle = std::time::Duration::from_millis(250);
        let remaining = deadline.remaining();
        if popup_settle_exceeds_remaining(remaining, settle) {
            std::thread::sleep(remaining.unwrap_or_default());
            return Err(GlassError::caller_deadline_elapsed("combo popup settle"));
        }
        std::thread::sleep(settle);
        Ok(())
    }

    fn semantic_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("semantic key input"));
        }
        self.key_by(event, deadline)
    }
}

/// First node of `role` in pre-order, or `None`.
fn find_role(node: &AxNode, role: AxRole) -> Option<&AxNode> {
    if node.role == role {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_role(c, role))
}

/// Parse a `set_value` boolean for a switch/checkbox. Case-insensitive; `None` for anything else
/// (which falls through to the backend's normal `set_value` path).
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => Some(true),
        "false" | "off" | "0" | "no" => Some(false),
        _ => None,
    }
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
        if node.role == AxRole::ComboBox
            && let Some(b) = &node.bounds
        {
            let (cx, cy) = rect_center(b);
            let d = (cx - tx).pow(2) + (cy - ty).pow(2);
            if best.is_none_or(|(_, bd)| d < bd) {
                *best = Some((node, d));
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

/// The CHECKABLE node nearest `bounds` — disambiguates same-named checkable siblings (e.g. two
/// generic `UISwitch` rows) when re-locating a toggled switch after a re-snapshot, since ids
/// don't survive it either. Matching by name alone risks latching onto a same-named sibling
/// that already happens to sit in the wanted state, turning a no-op sibling into a false `Ok`
/// for the element that was actually supposed to move — bounds are the disambiguator instead,
/// same as `find_combo_near` uses for combos. Unlike `find_combo_near`, there is no
/// single-element fallback when `bounds` is `None`: a toggle verify with no captured bounds
/// must error rather than risk matching the wrong same-named node.
fn find_checkable_near<'a>(
    root: &'a AxNode,
    bounds: Option<&crate::accessibility::AxRect>,
) -> Option<&'a AxNode> {
    let t = bounds?;
    let (tx, ty) = rect_center(t);
    fn walk<'a>(node: &'a AxNode, tx: i64, ty: i64, best: &mut Option<(&'a AxNode, i64)>) {
        if node.states.checkable
            && let Some(b) = &node.bounds
        {
            let (cx, cy) = rect_center(b);
            let d = (cx - tx).pow(2) + (cy - ty).pow(2);
            if best.is_none_or(|(_, bd)| d < bd) {
                *best = Some((node, d));
            }
        }
        for c in &node.children {
            walk(c, tx, ty, best);
        }
    }
    let mut best = None;
    walk(root, tx, ty, &mut best);
    best.map(|(n, _)| n)
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
    if let Some(n) = &node.name
        && !n.is_empty()
    {
        return Some(n.clone());
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
/// minimum) — whatever order the platform's `list_windows` enumerated them in.
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

fn restorable_window(
    windows: &[WindowInfo],
    selected_geometry: &WindowGeometry,
) -> Result<WindowId> {
    fn unique_id<'a>(mut matches: impl Iterator<Item = &'a WindowInfo>) -> Option<WindowId> {
        let id = matches.next()?.id;
        matches.next().is_none().then_some(id)
    }

    unique_id(windows.iter().filter(|window| window.active))
        .or_else(|| {
            unique_id(
                windows
                    .iter()
                    .filter(|window| &window.geometry == selected_geometry),
            )
        })
        .ok_or_else(|| {
            GlassError::Backend(
                "cannot guarantee restoration of the previously selected window; the window list has no unique active or geometry-matching target"
                    .into(),
            )
            .before_dispatch()
        })
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
/// one closer to `root` — since [`AxTree::path_to`] returns root-to-target and `min_by_key`
/// keeps the first minimum.
fn menu_container_bounds(
    tree: &AxTree,
    target: AxNodeId,
    popover: &WindowGeometry,
) -> Option<crate::accessibility::AxRect> {
    let path = tree.path_to(target)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> AxRect {
        AxRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn cached_geometry_requires_the_exact_wait_fallback_proof() {
        let not_started = || GlassError::deadline_not_started("geometry");
        assert!(may_use_cached_geometry(true, &not_started()));
        assert!(!may_use_cached_geometry(false, &not_started()));
        assert!(!may_use_cached_geometry(
            true,
            &GlassError::caller_deadline_elapsed("geometry")
        ));
        assert!(!may_use_cached_geometry(
            true,
            &GlassError::Bounded {
                kind: crate::BoundKind::NotStarted,
                whose: crate::Whose::Callee,
                dispatch: crate::BoundDispatch::NotDispatched,
                message: "callee geometry refusal".into(),
            }
        ));
        assert!(!may_use_cached_geometry(
            true,
            &not_started().after_dispatch()
        ));
    }

    #[test]
    fn only_legacy_unbounded_calls_ignore_window_probe_failures() {
        assert!(legacy_window_probe_is_best_effort(Deadline::UNBOUNDED));
        assert!(!legacy_window_probe_is_best_effort(Deadline::from_millis(
            1_000
        )));
    }

    #[test]
    fn popup_settle_treats_an_exact_tie_as_enough_time() {
        let settle = Duration::from_millis(250);
        assert!(popup_settle_exceeds_remaining(
            Some(Duration::from_millis(249)),
            settle
        ));
        assert!(!popup_settle_exceeds_remaining(Some(settle), settle));
        assert!(!popup_settle_exceeds_remaining(None, settle));
    }

    /// Centre is origin plus half the extent, on each axis independently. Odd extents floor,
    /// which is what keeps the point inside the rect.
    #[test]
    fn rect_center_is_the_midpoint_of_each_axis() {
        assert_eq!(rect_center(&rect(0, 0, 10, 20)), (5, 10));
        assert_eq!(rect_center(&rect(100, 200, 10, 20)), (105, 210));
        // Odd extents floor rather than round up or land on the far edge.
        assert_eq!(rect_center(&rect(0, 0, 5, 7)), (2, 3));
        // Negative origins keep the offset positive, so the axes cannot be swapped unnoticed.
        assert_eq!(rect_center(&rect(-30, -8, 10, 4)), (-25, -6));
        // A zero extent is the origin itself.
        assert_eq!(rect_center(&rect(7, 9, 0, 0)), (7, 9));
    }

    /// Picks the *nearest* combo, not the first in pre-order — the two differ only when the
    /// nearest is later in the tree, which is the case that matters.
    #[test]
    fn find_combo_near_picks_the_closest_not_the_first() {
        let far = ax_node(1, AxRole::ComboBox, Some(rect(0, 0, 10, 10)), vec![]);
        let near = ax_node(2, AxRole::ComboBox, Some(rect(100, 100, 10, 10)), vec![]);
        let root = ax_node(
            0,
            AxRole::Window,
            Some(rect(0, 0, 200, 200)),
            vec![far, near],
        );

        // Target sits on top of the second combo, which is last in pre-order.
        let want_near = rect(100, 100, 10, 10);
        assert_eq!(
            find_combo_near(&root, Some(&want_near)).map(|n| n.id),
            Some(AxNodeId(2))
        );
        // And the mirror, so "always the last" is wrong too.
        let want_far = rect(0, 0, 10, 10);
        assert_eq!(
            find_combo_near(&root, Some(&want_far)).map(|n| n.id),
            Some(AxNodeId(1))
        );
        // Ties keep the first seen, so the comparison is strict.
        let equidistant = rect(50, 50, 10, 10);
        assert_eq!(
            find_combo_near(&root, Some(&equidistant)).map(|n| n.id),
            Some(AxNodeId(1)),
            "a tie must not be taken by the later node"
        );
        // Near the origin a difference of coordinates is indistinguishable from a ratio or a
        // product — put the target far out and give each candidate its whole error on one axis.
        let far_x = ax_node(4, AxRole::ComboBox, Some(rect(-5, 995, 10, 10)), vec![]);
        let near_y = ax_node(5, AxRole::ComboBox, Some(rect(995, 985, 10, 10)), vec![]);
        let root_x = ax_node(
            0,
            AxRole::Window,
            Some(rect(0, 0, 2000, 2000)),
            vec![far_x, near_y],
        );
        let t = rect(995, 995, 10, 10);
        assert_eq!(
            find_combo_near(&root_x, Some(&t)).map(|n| n.id),
            Some(AxNodeId(5)),
            "1000 away on x must beat 10 away on y only by difference, not by ratio"
        );

        // Mirrored, so the same collapse on the other axis is caught too.
        let far_y = ax_node(6, AxRole::ComboBox, Some(rect(995, -5, 10, 10)), vec![]);
        let near_x = ax_node(7, AxRole::ComboBox, Some(rect(985, 995, 10, 10)), vec![]);
        let root_y = ax_node(
            0,
            AxRole::Window,
            Some(rect(0, 0, 2000, 2000)),
            vec![far_y, near_x],
        );
        assert_eq!(
            find_combo_near(&root_y, Some(&t)).map(|n| n.id),
            Some(AxNodeId(7))
        );

        // No target: the first combo, the documented single-combo fallback.
        assert_eq!(
            find_combo_near(&root, None).map(|n| n.id),
            Some(AxNodeId(1))
        );
        // A combo with no bounds is unreachable by proximity but still the fallback.
        let unbounded = ax_node(3, AxRole::ComboBox, None, vec![]);
        let only_unbounded = ax_node(0, AxRole::Window, Some(rect(0, 0, 20, 20)), vec![unbounded]);
        assert_eq!(
            find_combo_near(&only_unbounded, Some(&want_near)).map(|n| n.id),
            Some(AxNodeId(3))
        );
    }

    /// Same proximity rule, but deliberately no fallback: a toggle verify with no captured
    /// bounds must fail rather than risk latching onto a same-named sibling.
    #[test]
    fn find_checkable_near_has_no_fallback_without_bounds() {
        let mut a = ax_node(1, AxRole::CheckBox, Some(rect(0, 0, 10, 10)), vec![]);
        a.states.checkable = true;
        let mut b = ax_node(2, AxRole::CheckBox, Some(rect(100, 100, 10, 10)), vec![]);
        b.states.checkable = true;
        let root = ax_node(0, AxRole::Window, Some(rect(0, 0, 200, 200)), vec![a, b]);

        let near_b = rect(100, 100, 10, 10);
        assert_eq!(
            find_checkable_near(&root, Some(&near_b)).map(|n| n.id),
            Some(AxNodeId(2))
        );
        let near_a = rect(0, 0, 10, 10);
        assert_eq!(
            find_checkable_near(&root, Some(&near_a)).map(|n| n.id),
            Some(AxNodeId(1))
        );
        // Same coordinate-collapse cases as for combos: a difference, not a ratio or product.
        let mut fx = ax_node(4, AxRole::CheckBox, Some(rect(-5, 995, 10, 10)), vec![]);
        fx.states.checkable = true;
        let mut ny = ax_node(5, AxRole::CheckBox, Some(rect(995, 985, 10, 10)), vec![]);
        ny.states.checkable = true;
        let far_root = ax_node(
            0,
            AxRole::Window,
            Some(rect(0, 0, 2000, 2000)),
            vec![fx, ny],
        );
        let t = rect(995, 995, 10, 10);
        assert_eq!(
            find_checkable_near(&far_root, Some(&t)).map(|n| n.id),
            Some(AxNodeId(5))
        );

        // Ties keep the first seen here too, so the comparison is strict.
        let equidistant = rect(50, 50, 10, 10);
        assert_eq!(
            find_checkable_near(&root, Some(&equidistant)).map(|n| n.id),
            Some(AxNodeId(1)),
            "a tie must not be taken by the later node"
        );
        // The documented refusal — unlike find_combo_near, there is no first-match fallback.
        assert_eq!(find_checkable_near(&root, None).map(|n| n.id), None);
        // Not-checkable nodes are invisible to it, whatever their role.
        let plain = ax_node(5, AxRole::CheckBox, Some(rect(0, 0, 10, 10)), vec![]);
        let no_checkables = ax_node(0, AxRole::Window, Some(rect(0, 0, 20, 20)), vec![plain]);
        assert_eq!(find_checkable_near(&no_checkables, Some(&near_a)), None);
    }

    /// Both halves are required: the right role AND actually expanded.
    #[test]
    fn find_expanded_combo_needs_role_and_state() {
        let mut collapsed = ax_node(1, AxRole::ComboBox, None, vec![]);
        collapsed.states.expanded = false;
        let mut expanded = ax_node(2, AxRole::ComboBox, None, vec![]);
        expanded.states.expanded = true;
        let mut expanded_list = ax_node(3, AxRole::List, None, vec![]);
        expanded_list.states.expanded = true;

        let root = ax_node(
            0,
            AxRole::Window,
            None,
            vec![collapsed, expanded_list, expanded],
        );
        assert_eq!(find_expanded_combo(&root).map(|n| n.id), Some(AxNodeId(2)));

        // An expanded node of another role is not a match, nor is a collapsed combo.
        let mut only_list = ax_node(3, AxRole::List, None, vec![]);
        only_list.states.expanded = true;
        let no_combo = ax_node(0, AxRole::Window, None, vec![only_list]);
        assert_eq!(find_expanded_combo(&no_combo), None);
    }

    /// Pre-order, first match wins, and a role that is absent yields None rather than the root.
    #[test]
    fn find_role_returns_the_first_in_pre_order() {
        let deep = ax_node(2, AxRole::Button, None, vec![]);
        let branch = ax_node(1, AxRole::Group, None, vec![deep]);
        let sibling = ax_node(3, AxRole::Button, None, vec![]);
        let root = ax_node(0, AxRole::Window, None, vec![branch, sibling]);

        assert_eq!(
            find_role(&root, AxRole::Button).map(|n| n.id),
            Some(AxNodeId(2))
        );
        assert_eq!(
            find_role(&root, AxRole::Window).map(|n| n.id),
            Some(AxNodeId(0))
        );
        assert_eq!(find_role(&root, AxRole::Slider), None);
    }

    /// Containment is half-open on each axis: the left/top edges are inside, the right/bottom
    /// edges are not. Every bound is asserted from both sides, since one comparison flipped or
    /// one conjunction loosened still leaves the ordinary cases right.
    #[test]
    fn owning_popover_containment_is_half_open_on_every_edge() {
        let active = WindowGeometry {
            x: 0,
            y: 0,
            width: 400,
            height: 400,
        };
        // Popover occupying (100,100) to (200,200) exclusive.
        let pop = WindowGeometry {
            x: 100,
            y: 100,
            width: 100,
            height: 100,
        };
        let windows = vec![
            window_info(1, active.clone(), true),
            window_info(2, pop.clone(), false),
        ];
        // The element's centre is what is tested, so a 2x2 rect centres on its own origin + 1.
        let at = |x: i32, y: i32| AxRect {
            x: x - 1,
            y: y - 1,
            width: 2,
            height: 2,
        };

        assert_eq!(
            owning_popover(at(150, 150), &active, &windows),
            Some(WindowId(2))
        );
        // Inclusive on the near edges.
        assert_eq!(
            owning_popover(at(100, 150), &active, &windows),
            Some(WindowId(2))
        );
        assert_eq!(
            owning_popover(at(150, 100), &active, &windows),
            Some(WindowId(2))
        );
        // Exclusive on the far edges — 200 is the first column outside.
        assert_eq!(owning_popover(at(200, 150), &active, &windows), None);
        assert_eq!(owning_popover(at(150, 200), &active, &windows), None);
        assert_eq!(
            owning_popover(at(199, 199), &active, &windows),
            Some(WindowId(2))
        );
        // Outside on one axis only: every bound must hold, not any of them.
        assert_eq!(owning_popover(at(150, 50), &active, &windows), None);
        assert_eq!(owning_popover(at(50, 150), &active, &windows), None);
        assert_eq!(owning_popover(at(150, 350), &active, &windows), None);
        assert_eq!(owning_popover(at(350, 150), &active, &windows), None);
    }

    /// Ties are broken by area, not by perimeter: a tall thin window and a squat wide one can
    /// share a sum while differing in area.
    #[test]
    fn owning_popover_prefers_the_smaller_area_not_the_smaller_sum() {
        let active = WindowGeometry {
            x: 0,
            y: 0,
            width: 400,
            height: 400,
        };
        // The sums must disagree with the areas, or the test cannot tell the two rules apart.
        let thin = WindowGeometry {
            x: 100,
            y: 100,
            width: 4,
            height: 100,
        };
        let squat = WindowGeometry {
            x: 100,
            y: 100,
            width: 52,
            height: 52,
        };
        // thin: area 400, sum 104. squat: area 2704, sum 104. Equal sums, different areas.
        let windows = vec![
            window_info(1, active.clone(), true),
            window_info(2, squat, false),
            window_info(3, thin, false),
        ];
        let at = AxRect {
            x: 101,
            y: 101,
            width: 2,
            height: 2,
        };
        assert_eq!(owning_popover(at, &active, &windows), Some(WindowId(3)));
    }

    /// The container is the ancestor whose size is closest to the popover's, within 16px on
    /// both axes — so the tolerance is a difference, and the score sums the two axes.
    #[test]
    fn menu_container_bounds_scores_by_summed_axis_difference() {
        let popover = WindowGeometry {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        // Within tolerance on both axes, closer on the sum.
        let close = ax_node(2, AxRole::List, Some(rect(0, 0, 100, 100)), vec![]);
        // Also within tolerance, but further.
        let looser = ax_node(1, AxRole::Group, Some(rect(0, 0, 110, 110)), vec![close]);
        let root = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![looser]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root.clone()), AxNodeId(2), &popover),
            Some(rect(0, 0, 100, 100))
        );

        // Exactly at the tolerance on one axis is still in; one past it is out.
        let edge = ax_node(2, AxRole::List, Some(rect(0, 0, 116, 100)), vec![]);
        let root_edge = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![edge]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root_edge.clone()), AxNodeId(2), &popover),
            Some(rect(0, 0, 116, 100))
        );
        let past = ax_node(2, AxRole::List, Some(rect(0, 0, 117, 100)), vec![]);
        let root_past = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![past]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root_past.clone()), AxNodeId(2), &popover),
            None
        );

        // The same on the other axis, so one tolerance tightened is not covered by the other.
        let edge_h = ax_node(2, AxRole::List, Some(rect(0, 0, 100, 116)), vec![]);
        let root_edge_h = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![edge_h]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root_edge_h.clone()), AxNodeId(2), &popover),
            Some(rect(0, 0, 100, 116))
        );
        let past_h = ax_node(2, AxRole::List, Some(rect(0, 0, 100, 117)), vec![]);
        let root_past_h = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![past_h]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root_past_h.clone()), AxNodeId(2), &popover),
            None
        );

        // The score sums the axes rather than multiplying them, and the two orderings
        // disagree here: (1,10) sums to 11 and multiplies to 10, while (5,5) sums to 10 and
        // multiplies to 25. Candidates differing on only one axis cannot tell them apart.
        let lopsided = ax_node(2, AxRole::List, Some(rect(0, 0, 101, 110)), vec![]);
        let even = ax_node(1, AxRole::Group, Some(rect(0, 0, 105, 105)), vec![lopsided]);
        let root_score = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![even]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root_score.clone()), AxNodeId(2), &popover),
            Some(rect(0, 0, 105, 105)),
            "the lower summed difference must win, not the lower product"
        );

        // The difference is absolute: smaller than the popover counts the same as larger.
        let smaller = ax_node(2, AxRole::List, Some(rect(0, 0, 90, 90)), vec![]);
        let root_small = ax_node(0, AxRole::Window, Some(rect(0, 0, 400, 400)), vec![smaller]);
        assert_eq!(
            menu_container_bounds(&AxTree::new(root_small.clone()), AxNodeId(2), &popover),
            Some(rect(0, 0, 90, 90))
        );
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
            menu_container_bounds(&AxTree::new(root.clone()), AxNodeId(2), &popover),
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
        assert_eq!(
            menu_container_bounds(&AxTree::new(root.clone()), AxNodeId(1), &popover),
            None
        );
    }

    #[test]
    fn menu_container_bounds_prefers_closest_size_over_nearest_ancestor() {
        // The real GTK4 widget tree from the Xvfb spike: layout wrapper `Group`s sit between
        // the option row and the menu `List`, and their bounds *also* fall within the 16px
        // tolerance — so picking the ancestor NEAREST `target` returns a wrapper, not the
        // container.
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
            menu_container_bounds(&AxTree::new(root.clone()), AxNodeId(6), &popover),
            Some(container_bounds),
            "the real container (closest in size to the popover) must win over nearer wrapper groups"
        );
    }

    #[test]
    fn menu_container_bounds_prefers_content_container_over_window_root_sized_ancestor() {
        // Two kinds of ancestor commonly fall within tolerance: an outer node sized like the
        // popover window's own frame (a few px *larger* — decorations/margins) and the inner
        // content container a few px *smaller* (the real GTK4 shape).
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
            menu_container_bounds(&AxTree::new(root.clone()), AxNodeId(2), &popover),
            Some(content_bounds),
            "both root and content are within tolerance, but content is numerically \
             closest to the popover's size and must win over the outer window root"
        );
    }

    #[test]
    fn a11y_snapshot_assigns_ids_and_counts() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        assert_eq!(tree.count, 2);
        assert_eq!(tree.root.id, AxNodeId(0));
        assert_eq!(tree.root.children[0].id, AxNodeId(1));
    }

    #[test]
    fn a11y_resnapshot_rejects_geometry_success_after_the_caller_deadline() {
        let mut g = glass_with_a11y(
            FakePlatform::new(100, 100).with_geometry_delay(Duration::from_millis(20)),
            fake_tree(),
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .a11y_resnapshot(Deadline::from_millis(5))
            .expect_err("the geometry seam must not return late success");

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn a11y_resnapshot_shares_one_absolute_deadline_with_pid_discovery_and_reader() {
        let pid_deadlines = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_pid_deadline_log(pid_deadlines.clone());
        let (mut g, ctx_log) = glass_with_a11y_ctx(platform, fake_tree());
        g.start(&spec()).unwrap();
        let deadline = Deadline::from_millis(500);

        g.a11y_resnapshot(deadline).unwrap();

        assert_eq!(*pid_deadlines.lock().unwrap(), vec![deadline]);
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("the accessibility reader ran")
                .deadline,
            deadline
        );
    }

    #[test]
    fn a11y_subscription_shares_one_absolute_deadline_with_pid_discovery_and_reader() {
        let pid_deadlines = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_pid_deadline_log(pid_deadlines.clone());
        let (mut g, ctx_log) = glass_with_a11y_ctx(platform, fake_tree());
        g.start(&spec()).unwrap();
        let deadline = Deadline::from_millis(500);

        assert!(g.subscribe_a11y_changes(deadline).is_none());

        assert_eq!(*pid_deadlines.lock().unwrap(), vec![deadline]);
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("the accessibility subscriber ran")
                .deadline,
            deadline
        );
    }

    #[test]
    fn native_invoke_shares_one_absolute_deadline_with_pid_discovery_and_reader() {
        let pid_deadlines = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_pid_deadline_log(pid_deadlines.clone());
        let (mut g, _, ctx_log) =
            glass_with_a11y_invoke_ctx(platform, fake_tree(), InvokeBehavior::Succeed);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        pid_deadlines.lock().unwrap().clear();
        let deadline = Deadline::from_millis(500);

        assert_eq!(
            g.click_element_by(AxNodeId(1), deadline).unwrap(),
            ClickMethod::NativeAction { actuated: None }
        );

        assert_eq!(*pid_deadlines.lock().unwrap(), vec![deadline]);
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("the accessibility invoker ran")
                .deadline,
            deadline
        );
    }

    #[test]
    fn set_value_shares_one_absolute_deadline_with_pid_discovery_and_reader() {
        let pid_deadlines = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_pid_deadline_log(pid_deadlines.clone());
        let (mut g, ctx_log) = glass_with_a11y_ctx(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        pid_deadlines.lock().unwrap().clear();
        let deadline = Deadline::from_millis(500);

        g.set_value_by(AxNodeId(1), "renamed", deadline).unwrap();

        assert_eq!(*pid_deadlines.lock().unwrap(), vec![deadline]);
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("the accessibility writer ran")
                .deadline,
            deadline
        );
    }

    #[test]
    fn snapshot_unsupported_without_reader() {
        let mut g = glass_with(FakePlatform::new(40, 30));
        g.start(&spec()).unwrap();
        assert!(matches!(
            g.a11y_snapshot(None).unwrap_err(),
            GlassError::AxUnsupported
        ));
    }

    #[test]
    fn click_element_clicks_center_via_pointer_path() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g.click_element(AxNodeId(1)).unwrap();
        // The Button at (10,10 20x20) → center (20,20), via the normal pointer path.
        assert_eq!(clicks.lock().unwrap().last().copied(), Some((20, 20)));
    }

    #[test]
    fn a11y_snapshot_refreshes_geometry_so_click_element_uses_current_window() {
        // #6: an app resizes itself (opens a sidebar) with no glass_window op, and a stale
        // window clips elements now beyond it. Start 230 wide, platform now reports 458; a
        // Button at x=292 is off a stale 230 window but on-screen in the real 458.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let bounds = AxRect {
            x: 292,
            y: 241,
            width: 48,
            height: 48,
        };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "window".into(),
            name: None,
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 458,
                height: 408,
            }),
            children: vec![AxNode {
                id: AxNodeId(0),
                role: AxRole::Button,
                raw_role: "button".into(),
                name: Some("5".into()),
                description: None,
                value: None,
                states: AxStates::default(),
                bounds: Some(bounds),
                children: vec![],
            }],
        };
        let platform = FakePlatform::new(230, 408)
            .resized_to(WindowGeometry {
                x: 0,
                y: 0,
                width: 458,
                height: 408,
            })
            .with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, AxTree::new(root));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // must refresh s.geometry 230 → 458
        g.click_element(AxNodeId(1)).unwrap(); // the Button at x=292 — on-screen only in 458
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            bounds.clamped_center(458, 408),
            "click_element clamps against the refreshed 458 window, not the stale 230"
        );
    }

    #[test]
    fn click_element_refreshes_geometry_so_the_native_invoke_sees_the_current_window() {
        // The Windows/macOS `invoke` fingerprints by window-RELATIVE bounds from `ctx.window`,
        // so a window moved since the snapshot reads as drift. Script the geometry read to
        // report a moved window after the snapshot's own read.
        let snapshot_geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let moved_geo = WindowGeometry {
            x: 640,
            y: 480,
            width: 100,
            height: 100,
        };
        let platform = FakePlatform::new(100, 100)
            .resized_to(snapshot_geo)
            .resized_to(moved_geo.clone());
        let (mut g, invoke_log, ctx_log) =
            glass_with_a11y_invoke_ctx(platform, fake_tree(), InvokeBehavior::Succeed);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // consumes the first scripted geometry read

        assert_eq!(
            g.click_element(AxNodeId(1)).unwrap(),
            ClickMethod::NativeAction { actuated: None }
        );

        assert_eq!(invoke_log.lock().unwrap().len(), 1, "the native path ran");
        assert_eq!(
            ctx_log.lock().unwrap().as_ref().map(|c| c.window.clone()),
            Some(moved_geo),
            "invoke's ctx carries the refreshed window, not the snapshot's stale one"
        );
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
        g.a11y_snapshot(None).unwrap();
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
    fn click_element_without_bounds_errors() {
        let mut tree = fake_tree();
        tree.root.children.push(AxNode {
            id: AxNodeId(0),
            role: AxRole::Label,
            raw_role: "label".into(),
            name: Some("nobounds".into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        });
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), tree);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        // node #2 is the boundless Label.
        assert!(matches!(
            g.click_element(AxNodeId(2)).unwrap_err(),
            GlassError::AxElementNotClickable(2)
        ));
    }

    #[test]
    fn click_element_prefers_native_invoke_and_synthesizes_no_pointer() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let (mut g, invoke_log) =
            glass_with_a11y_invoke(platform, fake_tree(), InvokeBehavior::Succeed);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        let method = g.click_element(AxNodeId(1)).unwrap();
        assert_eq!(method, ClickMethod::NativeAction { actuated: None });
        assert!(
            clicks.lock().unwrap().is_empty(),
            "no pointer event on the native path"
        );
        let log = invoke_log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].id, AxNodeId(1));
        assert_eq!(log[0].role, AxRole::Button);
        assert_eq!(log[0].name.as_deref(), Some("Save"));
        assert!(
            log[0].bounds.is_some(),
            "fingerprint carries the snapshot bounds"
        );
    }

    #[test]
    fn click_element_carries_the_element_the_backend_actuated_instead() {
        // A backend whose toolkit carries the activation on an ancestor of the label clicks
        // that ancestor. The method has to say so, or the click and its audit record read as
        // if the caller's own element was the one that fired.
        let (mut g, _) = glass_with_a11y_invoke(
            FakePlatform::new(100, 100),
            fake_tree(),
            InvokeBehavior::SucceedOnAnother(9),
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert_eq!(
            g.click_element(AxNodeId(1)).unwrap(),
            ClickMethod::NativeAction {
                actuated: Some(AxNodeId(9))
            }
        );
    }

    #[test]
    fn click_element_falls_back_when_element_has_no_action_and_discloses() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let (mut g, _) = glass_with_a11y_invoke(platform, fake_tree(), InvokeBehavior::NoAction);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        let method = g.click_element(AxNodeId(1)).unwrap();
        assert_eq!(
            method,
            ClickMethod::Pointer {
                native_fallback: "element exposes no activation action".into()
            }
        );
        assert_eq!(clicks.lock().unwrap().last().copied(), Some((20, 20)));
    }

    #[test]
    fn click_element_falls_back_when_backend_has_no_invoke() {
        // Default knob = Unsupported = a backend that never implemented invoke (iOS/Android).
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        let method = g.click_element(AxNodeId(1)).unwrap();
        assert_eq!(
            method,
            ClickMethod::Pointer {
                native_fallback: "backend has no native action path".into()
            }
        );
        assert_eq!(clicks.lock().unwrap().last().copied(), Some((20, 20)));
    }

    #[test]
    fn click_element_action_failure_propagates_and_never_pointer_clicks() {
        // A native action that reported failure may still have been DISPATCHED (the backend
        // fires it on a detached worker and can lose the answer), so a pointer click on top
        // of it would actuate the control twice.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let (mut g, _) = glass_with_a11y_invoke(platform, fake_tree(), InvokeBehavior::Fail);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert!(matches!(
            g.click_element(AxNodeId(1)).unwrap_err(),
            GlassError::AxActionFailed(1, _)
        ));
        assert!(
            clicks.lock().unwrap().is_empty(),
            "no pointer event after a possibly-dispatched native action"
        );
    }

    #[test]
    fn click_element_drift_propagates_and_never_pointer_clicks() {
        // AxElementChanged means the tree drifted; a pointer click at the stale bounds
        // would hit the wrong element, so there must be NO fallback.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let (mut g, _) = glass_with_a11y_invoke(platform, fake_tree(), InvokeBehavior::Drifted);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert!(matches!(
            g.click_element(AxNodeId(1)).unwrap_err(),
            GlassError::AxElementChanged(1)
        ));
        assert!(
            clicks.lock().unwrap().is_empty(),
            "no pointer event after drift"
        );
    }

    #[test]
    fn click_element_boundless_element_clicks_via_invoke() {
        // Today a boundless node is AxElementNotClickable; with a native action it works.
        let mut tree = fake_tree();
        tree.root.children.push(AxNode {
            id: AxNodeId(0),
            role: AxRole::Button,
            raw_role: "button".into(),
            name: Some("hidden".into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        });
        let (mut g, _) =
            glass_with_a11y_invoke(FakePlatform::new(100, 100), tree, InvokeBehavior::Succeed);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert_eq!(
            g.click_element(AxNodeId(2)).unwrap(),
            ClickMethod::NativeAction { actuated: None }
        );
    }

    #[test]
    fn click_element_boundless_element_without_action_stays_not_clickable() {
        let mut tree = fake_tree();
        tree.root.children.push(AxNode {
            id: AxNodeId(0),
            role: AxRole::Button,
            raw_role: "button".into(),
            name: Some("hidden".into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        });
        let (mut g, _) =
            glass_with_a11y_invoke(FakePlatform::new(100, 100), tree, InvokeBehavior::NoAction);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert!(matches!(
            g.click_element(AxNodeId(2)).unwrap_err(),
            GlassError::AxElementNotClickable(2)
        ));
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
        g.a11y_snapshot(None).unwrap();
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
    fn click_element_swipes_the_trailing_control_for_a_row_shaped_checkable() {
        // A checkable node with row-shaped bounds (w > 4h) on a backend that reports the whole
        // cell as the switch's frame: it must swipe the trailing control, while a non-checkable
        // node of the SAME bounds still clicks center.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let bounds = AxRect {
            x: 10,
            y: 10,
            width: 80,
            height: 15,
        }; // 80 > 4 * 15 ⇒ row-shaped
        let leaf = |role: AxRole, name: &str, checkable: bool| AxNode {
            id: AxNodeId(0),
            role,
            raw_role: name.into(),
            name: Some(name.into()),
            description: None,
            value: None,
            states: AxStates {
                checkable,
                ..Default::default()
            },
            bounds: Some(bounds),
            children: vec![],
        };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "window".into(),
            name: None,
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            children: vec![
                leaf(AxRole::CheckBox, "switch", true),
                leaf(AxRole::ListItem, "row", false),
            ],
        };
        // A backend that frames a switch as its whole row (iOS/idb) opts into the trailing-aim.
        let platform = FakePlatform::new(100, 100)
            .with_click_log(clicks.clone())
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y(platform, AxTree::new(root));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        // node #1 = the row-shaped checkable → a swipe across the trailing control, not a click.
        let method = g.click_element(AxNodeId(1)).unwrap();
        assert!(
            matches!(method, ClickMethod::Pointer { .. }),
            "the swipe path is pointer-class (the fake's default invoke is Unsupported)"
        );
        let expected = bounds.trailing_toggle_swipe(100, 100).unwrap();
        {
            let d = drags.lock().unwrap();
            assert_eq!(d.len(), 1, "a swipe was emitted");
            match d[0] {
                PointerEvent::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    duration_ms,
                    ..
                } => {
                    assert_eq!((from_x, from_y), (expected.from_x, expected.from_y));
                    assert_eq!((to_x, to_y), (expected.to_x, expected.to_y));
                    assert_eq!(
                        duration_ms, TOGGLE_SWIPE_MS,
                        "a too-short duration would make idb treat this as a tap, not a swipe"
                    );
                }
                ref e => panic!("expected a Drag, got {e:?}"),
            }
        }
        assert!(
            clicks.lock().unwrap().is_empty(),
            "the row-shaped checkable must swipe, not click"
        );

        // node #2 = the non-checkable row of identical bounds → geometric center click (gate
        // needs checkable, so a plain wide list row is unaffected).
        g.click_element(AxNodeId(2)).unwrap();
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            bounds.clamped_center(100, 100)
        );
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the non-checkable row must not also emit a swipe"
        );
    }

    #[test]
    fn click_element_uses_center_for_a_row_shaped_checkable_on_a_non_trailing_backend() {
        // The trailing-aim is opt-in per backend: a desktop backend frames a labeled checkbox
        // as a wide row too, but its indicator is at the LEADING edge, so a row-shaped
        // checkable here must still click center.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let bounds = AxRect {
            x: 10,
            y: 10,
            width: 80,
            height: 15,
        }; // identical row-shaped bounds to the trailing test
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "window".into(),
            name: None,
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            children: vec![AxNode {
                id: AxNodeId(0),
                role: AxRole::CheckBox,
                raw_role: "checkbox".into(),
                name: Some("labeled".into()),
                description: None,
                value: None,
                states: AxStates {
                    checkable: true,
                    ..Default::default()
                },
                bounds: Some(bounds),
                children: vec![],
            }],
        };
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, AxTree::new(root));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g.click_element(AxNodeId(1)).unwrap();
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            bounds.clamped_center(100, 100),
            "a row-shaped checkable on a non-trailing backend clicks center, not trailing"
        );
    }

    #[test]
    fn click_element_uses_center_for_a_checkable_that_is_not_row_shaped() {
        // Even on a trailing-toggle backend, a checkable whose bounds are NOT row-shaped clicks
        // center. Uses exactly 4:1 (60x15) to pin the strict `>` boundary: 60 is NOT > 4*15=60,
        // so it is treated as tight → center.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let bounds = AxRect {
            x: 10,
            y: 10,
            width: 60,
            height: 15,
        }; // 60 == 4*15 exactly → not row-shaped (strict >)
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "window".into(),
            name: None,
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            children: vec![AxNode {
                id: AxNodeId(0),
                role: AxRole::CheckBox,
                raw_role: "checkbox".into(),
                name: Some("tight".into()),
                description: None,
                value: None,
                states: AxStates {
                    checkable: true,
                    ..Default::default()
                },
                bounds: Some(bounds),
                children: vec![],
            }],
        };
        let platform = FakePlatform::new(100, 100)
            .with_click_log(clicks.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y(platform, AxTree::new(root));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g.click_element(AxNodeId(1)).unwrap();
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            bounds.clamped_center(100, 100),
            "exactly 4:1 is not row-shaped (strict >), so it clicks center"
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
        g.a11y_snapshot(None).unwrap();
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
    fn bounded_click_propagates_a_failing_window_probe_before_pointer_input() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100)
            .with_click_log(clicks.clone())
            .with_failing_list_windows();
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .click_element_by(AxNodeId(1), Deadline::from_millis(1_000))
            .expect_err("bounded popover discovery must not hide its backend failure");

        assert!(
            matches!(error.cause(), GlassError::Backend(message) if message == "list_windows unavailable"),
            "{error}"
        );
        assert!(clicks.lock().unwrap().is_empty());
    }

    #[test]
    fn legacy_native_click_ignores_a_pre_dispatch_geometry_probe_failure() {
        let platform = FakePlatform::new(100, 100).with_failing_geometry();
        let (mut g, invokes) =
            glass_with_a11y_invoke(platform, fake_tree(), InvokeBehavior::Succeed);
        g.start(&spec()).unwrap();
        let mut cached = fake_tree();
        cached.assign_ids();
        g.active.as_mut().unwrap().last_ax = Some(cached);

        let method = g.click_element(AxNodeId(1)).unwrap();

        assert!(matches!(method, ClickMethod::NativeAction { .. }));
        assert_eq!(invokes.lock().unwrap().len(), 1);
    }

    #[test]
    fn native_focus_geometry_failure_is_best_effort_only_for_an_unbounded_legacy_call() {
        for (deadline, should_focus) in [
            (Deadline::UNBOUNDED, true),
            (Deadline::from_millis(1_000), false),
        ] {
            let focus_calls = Arc::new(AtomicUsize::new(0));
            let accessibility = FakeAccessibility::new(fake_tree())
                .with_focus_behavior(InvokeBehavior::Succeed)
                .with_focus_calls(focus_calls.clone());
            let mut g = glass_with_backend(
                FakePlatform::new(100, 100).with_failing_geometry(),
                Box::new(accessibility),
            );
            g.start(&spec()).unwrap();
            let mut cached = fake_tree();
            cached.assign_ids();
            g.active.as_mut().unwrap().last_ax = Some(cached);

            let result = g.try_native_focus(AxNodeId(1), deadline);

            assert_eq!(result.is_ok(), should_focus, "deadline={deadline:?}");
            assert_eq!(
                focus_calls.load(Ordering::Relaxed),
                usize::from(should_focus),
                "deadline={deadline:?}"
            );
            if let Err(error) = result {
                assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted));
                assert_eq!(
                    error.bound_dispatch(),
                    Some(crate::BoundDispatch::NotDispatched)
                );
            }
        }
    }

    /// The click is translated into the popover's container, on both axes. The validated
    /// fixture's container sits at x=0, where subtracting and adding its origin agree, so this
    /// repeats it with the container offset horizontally.
    #[test]
    fn click_element_translates_the_x_axis_into_the_container_too() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
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
            .with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, fake_tree_with_offset_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        g.click_element(globex_id).unwrap();

        // Item at x=70 inside a container at x=40 is 30 in; y is unchanged from the validated
        // fixture at 248 - 194.
        assert_eq!(clicks.lock().unwrap().last().copied(), Some((30, 54)));
    }

    #[test]
    fn planned_popover_inputs_use_the_plan_and_translate_both_axes_into_the_container() {
        let active = window_info(
            1,
            WindowGeometry {
                x: 0,
                y: 0,
                width: 340,
                height: 300,
            },
            true,
        );
        let popover = window_info(
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
        let drags = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300)
            .with_windows(vec![active, popover])
            .with_click_log(clicks.clone())
            .with_drag_log(drags.clone());
        let mut g = glass_with_a11y(platform, fake_tree_with_offset_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let id = tree.root.children[0].children[0].id;

        g.click_element_pointer_only(
            id,
            Some(&super::semantic_action::PlannedPointerInput::Click { point: (83, 267) }),
            Deadline::UNBOUNDED,
        )
        .unwrap();
        g.click_element_pointer_only(
            id,
            Some(
                &super::semantic_action::PlannedPointerInput::TrailingToggle {
                    segment: crate::Segment {
                        from_x: 78,
                        from_y: 254,
                        to_x: 90,
                        to_y: 270,
                    },
                    probe_point: (84, 262),
                },
            ),
            Deadline::UNBOUNDED,
        )
        .unwrap();

        assert_eq!(clicks.lock().unwrap().as_slice(), &[(43, 73)]);
        assert!(matches!(
            drags.lock().unwrap().as_slice(),
            [PointerEvent::Drag {
                from_x: 38,
                from_y: 60,
                to_x: 50,
                to_y: 76,
                ..
            }]
        ));
    }

    #[test]
    fn planned_ordinary_click_uses_the_stored_point_instead_of_recomputing_center() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.click_element_pointer_only(
            AxNodeId(1),
            Some(&super::semantic_action::PlannedPointerInput::Click { point: (13, 17) }),
            Deadline::UNBOUNDED,
        )
        .unwrap();

        assert_eq!(clicks.lock().unwrap().as_slice(), &[(13, 17)]);
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
        let tree = g.a11y_snapshot(None).unwrap();
        // assign_ids in pre-order: root=0, List=1, Globex(ListItem)=2.
        let globex_id = tree.root.children[0].children[0].id;
        assert_eq!(globex_id, AxNodeId(2));

        let method = g.click_element(globex_id).unwrap();

        assert!(
            matches!(method, ClickMethod::Pointer { .. }),
            "the fake's default invoke behavior is Unsupported, so this still routes pointer"
        );
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
    fn popover_success_restores_the_derived_selected_window_when_all_report_inactive() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = popover_platform_with_active(clicks.clone(), select_log.clone(), false);
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        assert!(matches!(
            g.click_element(globex_id).unwrap(),
            ClickMethod::Pointer { .. }
        ));

        assert_eq!(clicks.lock().unwrap().last().copied(), Some((20, 54)));
        assert_eq!(
            *select_log.lock().unwrap(),
            vec![WindowId(2), WindowId(1)],
            "the cached selected geometry identifies the main window even without a global active flag"
        );
        assert_eq!(g.geometry().unwrap().width, 340);
    }

    #[test]
    fn window_geometry_query_consuming_the_click_deadline_reports_started_work() {
        let mut g = glass_with_a11y(
            FakePlatform::new(100, 100).with_geometry_delay(Duration::from_millis(20)),
            fake_tree(),
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .click_element_by(AxNodeId(1), Deadline::from_millis(5))
            .unwrap_err();

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "the geometry query started before consuming the remaining deadline"
        );
    }

    #[test]
    fn popover_selection_timeout_restores_active_window_and_preserves_dispatch() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = popover_platform(clicks.clone(), select_log.clone())
            .with_select_delay(Duration::from_millis(20));
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        let error = g
            .click_element_by(globex_id, Deadline::from_millis(5))
            .unwrap_err();

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "the focus request may have changed OS focus before its confirmation timed out"
        );
        assert!(matches!(
            error.cause(),
            GlassError::Bounded {
                kind: crate::BoundKind::TimedOut,
                dispatch: crate::BoundDispatch::MayHaveDispatched,
                ..
            }
        ));
        assert!(clicks.lock().unwrap().is_empty());
        assert_eq!(
            *select_log.lock().unwrap(),
            vec![WindowId(2), WindowId(1)],
            "a possibly-dispatched focus request still requires restoration"
        );
    }

    #[test]
    fn popover_selection_timeout_restores_derived_window_when_all_report_inactive() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = popover_platform_with_active(clicks.clone(), select_log.clone(), false)
            .with_select_delay(Duration::from_millis(20));
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        let error = g
            .click_element_by(globex_id, Deadline::from_millis(5))
            .unwrap_err();

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert!(clicks.lock().unwrap().is_empty());
        assert_eq!(
            *select_log.lock().unwrap(),
            vec![WindowId(2), WindowId(1)],
            "a possibly-dispatched selection still restores the derived prior target"
        );
    }

    #[test]
    fn popover_pointer_failure_restores_derived_window_when_all_report_inactive() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = popover_platform_with_active(clicks.clone(), select_log.clone(), false)
            .with_failing_pointer();
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        let error = g.click_element(globex_id).unwrap_err();

        assert!(matches!(
            error.cause(),
            GlassError::Backend(message) if message == "scripted pointer failure"
        ));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            *select_log.lock().unwrap(),
            vec![WindowId(2), WindowId(1)],
            "pointer failure must not strand the session on the popover"
        );
        assert_eq!(g.geometry().unwrap().width, 340);
    }

    #[test]
    fn popover_selection_is_refused_when_all_report_inactive_and_geometry_is_ambiguous() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let main_geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: 340,
            height: 300,
        };
        let popover_geometry = WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        };
        let platform = FakePlatform::new(340, 300)
            .with_windows(vec![
                window_info(1, main_geometry.clone(), false),
                window_info(3, main_geometry, false),
                window_info(2, popover_geometry, false),
            ])
            .with_click_log(clicks.clone())
            .with_select_log(select_log.clone());
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        let error = g.click_element(globex_id).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot guarantee restoration of the previously selected window"),
            "{error}"
        );
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched)
        );
        assert!(clicks.lock().unwrap().is_empty());
        assert!(
            select_log.lock().unwrap().is_empty(),
            "no focus request is safe when the prior selected window is ambiguous"
        );
    }

    #[test]
    fn popover_selection_timeout_and_restoration_failure_preserve_both_errors() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = popover_platform(clicks.clone(), select_log.clone())
            .with_select_delay(Duration::from_millis(20))
            .with_failing_select_window(WindowId(1));
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        let error = g
            .click_element_by(globex_id, Deadline::from_millis(5))
            .unwrap_err();

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        let debug = format!("{error:#?}");
        assert!(
            debug.contains("WindowRestoreFailed")
                && debug.contains("primary: Bounded")
                && debug.contains("restore: Backend"),
            "both failures must remain structurally visible: {debug}"
        );
        let display = error.to_string();
        assert!(
            display.contains("deadline")
                && display.contains("scripted select_window failure for 1"),
            "both failure explanations must reach the caller: {display}"
        );
        assert!(clicks.lock().unwrap().is_empty());
        assert_eq!(
            *select_log.lock().unwrap(),
            vec![WindowId(2), WindowId(1)],
            "the prior target restoration was attempted rather than silently skipped"
        );
    }

    #[test]
    fn popover_selection_not_dispatched_does_not_restore_or_claim_focus_mutation() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let platform = popover_platform(clicks.clone(), select_log.clone())
            .rejecting_select_window_before_dispatch(WindowId(2));
        let mut g = glass_with_a11y(platform, fake_tree_with_popover_option());
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        let error = g.click_element(globex_id).unwrap_err();

        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched)
        );
        assert!(matches!(
            error.cause(),
            GlassError::Backend(message) if message == "scripted pre-dispatch rejection for 2"
        ));
        assert!(clicks.lock().unwrap().is_empty());
        assert!(
            select_log.lock().unwrap().is_empty(),
            "neither temporary focus nor restoration should be requested"
        );
    }

    /// The popover-routing platform: an active window plus the dropdown's popover window,
    /// with click + select logs — shared by the popover × native-invoke tests below.
    fn popover_platform(
        clicks: Arc<Mutex<Vec<(i32, i32)>>>,
        select_log: Arc<Mutex<Vec<WindowId>>>,
    ) -> FakePlatform {
        popover_platform_with_active(clicks, select_log, true)
    }

    fn popover_platform_with_active(
        clicks: Arc<Mutex<Vec<(i32, i32)>>>,
        select_log: Arc<Mutex<Vec<WindowId>>>,
        active: bool,
    ) -> FakePlatform {
        let active = window_info(
            1,
            WindowGeometry {
                x: 0,
                y: 0,
                width: 340,
                height: 300,
            },
            active,
        );
        let popover = window_info(
            2,
            WindowGeometry {
                x: -3,
                y: 220,
                width: 326,
                height: 135,
            },
            false,
        );
        FakePlatform::new(340, 300)
            .with_windows(vec![active, popover])
            .with_click_log(clicks)
            .with_select_log(select_log)
    }

    #[test]
    fn click_element_native_invoke_succeeds_for_popover_hosted_element_without_window_select() {
        // The native action addresses the element directly, so the whole popover machinery is
        // skipped: no focus change, no pointer event.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let (mut g, invoke_log) = glass_with_a11y_invoke(
            popover_platform(clicks.clone(), select_log.clone()),
            fake_tree_with_popover_option(),
            InvokeBehavior::Succeed,
        );
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        assert_eq!(
            g.click_element(globex_id).unwrap(),
            ClickMethod::NativeAction { actuated: None }
        );

        assert_eq!(invoke_log.lock().unwrap().len(), 1, "the native path ran");
        assert!(
            clicks.lock().unwrap().is_empty(),
            "no pointer event for a natively-invoked popover element"
        );
        assert!(
            select_log.lock().unwrap().is_empty(),
            "the popover is never raised — the native action needs no window focus"
        );
    }

    #[test]
    fn click_element_native_invoke_drifted_for_popover_hosted_element_is_fatal() {
        // Reject drift before stale popover bounds click whatever moved into that spot.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let select_log = Arc::new(Mutex::new(Vec::new()));
        let (mut g, _) = glass_with_a11y_invoke(
            popover_platform(clicks.clone(), select_log.clone()),
            fake_tree_with_popover_option(),
            InvokeBehavior::Drifted,
        );
        g.start(&spec()).unwrap();
        let tree = g.a11y_snapshot(None).unwrap();
        let globex_id = tree.root.children[0].children[0].id;

        assert!(matches!(
            g.click_element(globex_id).unwrap_err(),
            GlassError::AxElementChanged(id) if id == globex_id.0
        ));

        assert!(
            clicks.lock().unwrap().is_empty(),
            "no pointer event after drift"
        );
        assert!(
            select_log.lock().unwrap().is_empty(),
            "and no window was raised on the way to failing"
        );
    }

    #[test]
    fn click_element_in_popover_without_a_mappable_container_errors() {
        // Same popover-owning geometry, but the target has no List-sized ancestor to
        // recover a container origin from — must error, not silently mis-click.
        //
        // This also stands in for the residual `owning_popover` false positive documented on
        // that function: the size-matching gate turns a geometric misdetection into this
        // catchable error instead of a silent click into the wrong window.
        let globex = AxNode {
            id: AxNodeId(0),
            role: AxRole::ListItem,
            raw_role: "list item".into(),
            name: Some("Globex".into()),
            description: None,
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
            description: None,
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
        let tree = AxTree::new(root);
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
        let snapshot = g.a11y_snapshot(None).unwrap();
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

    /// The combo fixture: a single `ComboBox` named `name`, optionally `expanded` with its
    /// option rows realized underneath (an open GtkDropDown's shape — `ListItem`s carrying
    /// their label, the current one `selected`).
    fn combo(name: &str, options: &[&str]) -> AxTree {
        let bounds = AxRect {
            x: 0,
            y: 188,
            width: 320,
            height: 34,
        };
        let items: Vec<AxNode> = options
            .iter()
            .enumerate()
            .map(|(i, opt)| AxNode {
                states: AxStates {
                    selected: *opt == name,
                    ..Default::default()
                },
                ..named_node(
                    0,
                    AxRole::ListItem,
                    opt,
                    AxRect {
                        x: 20,
                        y: 200 + 30 * i as i32,
                        width: 280,
                        height: 27,
                    },
                )
            })
            .collect();
        let children = if options.is_empty() {
            vec![]
        } else {
            vec![AxNode {
                children: items,
                ..ax_node(
                    0,
                    AxRole::List,
                    Some(AxRect {
                        x: 0,
                        y: 194,
                        width: 320,
                        height: 129,
                    }),
                    vec![],
                )
            }]
        };
        let combo = AxNode {
            name: Some(name.into()),
            states: AxStates {
                expanded: !options.is_empty(),
                ..Default::default()
            },
            children,
            ..ax_node(0, AxRole::ComboBox, Some(bounds), vec![])
        };
        tree_with(340, 300, vec![combo])
    }

    fn expanded_combo_with_selected(name: &str, options: &[&str], selected: &str) -> AxTree {
        let mut tree = combo(name, options);
        for option in &mut tree.root.children[0].children[0].children {
            option.states.selected = option
                .name
                .as_deref()
                .is_some_and(|label| label == selected);
        }
        tree
    }

    enum SnapshotReply {
        Tree(AxTree),
        NotStarted(&'static str),
        TimedOut(&'static str),
        SleepPastDeadline(AxTree),
    }

    struct ScriptedSnapshots {
        replies: VecDeque<SnapshotReply>,
    }

    fn scripted_dispatch_error(
        operation: &'static str,
        dispatch: crate::BoundDispatch,
    ) -> GlassError {
        match dispatch {
            crate::BoundDispatch::NotDispatched => GlassError::deadline_not_started(operation),
            crate::BoundDispatch::MayHaveDispatched => {
                GlassError::caller_deadline_elapsed(operation)
            }
        }
    }

    struct InvokeErrorAccessibility {
        trees: VecDeque<AxTree>,
        invokes: Arc<AtomicUsize>,
        dispatch: crate::BoundDispatch,
    }

    impl Accessibility for InvokeErrorAccessibility {
        fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
            self.trees
                .pop_front()
                .ok_or_else(|| GlassError::Backend("scripted invoke snapshot exhausted".into()))
        }

        fn invoke(&mut self, _ctx: &AxContext, _target: &AxTarget) -> Result<Option<AxNodeId>> {
            self.invokes.fetch_add(1, Ordering::SeqCst);
            Err(scripted_dispatch_error(
                "scripted toggle actuation",
                self.dispatch,
            ))
        }
    }

    struct ScriptedKeyPlatform {
        inner: FakePlatform,
        replies: VecDeque<Result<()>>,
    }

    impl Platform for ScriptedKeyPlatform {
        fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
            self.inner.start_app(spec)
        }

        fn stop_app_by(&mut self, deadline: Deadline) -> Result<()> {
            self.inner.stop_app_by(deadline)
        }

        fn capture_frame_by(
            &mut self,
            region: Option<&Region>,
            deadline: Deadline,
        ) -> Result<Frame> {
            self.inner.capture_frame_by(region, deadline)
        }

        fn capture_window_by(
            &mut self,
            id: WindowId,
            region: Option<&Region>,
            deadline: Deadline,
        ) -> Result<Frame> {
            self.inner.capture_window_by(id, region, deadline)
        }

        fn send_pointer_by(&mut self, event: &PointerEvent, deadline: Deadline) -> Result<()> {
            self.inner.send_pointer_by(event, deadline)
        }

        fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
            match self.replies.pop_front() {
                Some(Err(error))
                    if error.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched) =>
                {
                    Err(error)
                }
                Some(Err(error)) => {
                    self.inner.send_key_by(event, deadline)?;
                    Err(error)
                }
                Some(Ok(())) | None => self.inner.send_key_by(event, deadline),
            }
        }

        fn window_by(&mut self, op: &WindowOp, deadline: Deadline) -> Result<WindowGeometry> {
            self.inner.window_by(op, deadline)
        }

        fn list_windows_by(&mut self, deadline: Deadline) -> Result<Vec<WindowInfo>> {
            self.inner.list_windows_by(deadline)
        }

        fn select_window_by(&mut self, id: WindowId, deadline: Deadline) -> Result<WindowGeometry> {
            self.inner.select_window_by(id, deadline)
        }

        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            self.inner.drain_logs()
        }

        fn app_pid(&self) -> Option<u32> {
            self.inner.app_pid()
        }

        fn a11y_toggle_control_at_trailing_edge(&self) -> bool {
            self.inner.a11y_toggle_control_at_trailing_edge()
        }
    }

    impl Accessibility for ScriptedSnapshots {
        fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
            match self
                .replies
                .pop_front()
                .expect("scripted accessibility snapshot exhausted")
            {
                SnapshotReply::Tree(tree) => Ok(tree),
                SnapshotReply::NotStarted(operation) => {
                    Err(GlassError::deadline_not_started(operation))
                }
                SnapshotReply::TimedOut(operation) => {
                    Err(GlassError::caller_deadline_elapsed(operation))
                }
                SnapshotReply::SleepPastDeadline(tree) => {
                    let left = ctx
                        .deadline
                        .remaining()
                        .expect("sleep-past reply requires a bounded deadline");
                    std::thread::sleep(left.saturating_add(Duration::from_millis(5)));
                    Ok(tree)
                }
            }
        }
    }

    fn glass_with_scripted_snapshots(platform: FakePlatform, replies: Vec<SnapshotReply>) -> Glass {
        glass_with_backend(
            platform,
            Box::new(ScriptedSnapshots {
                replies: replies.into(),
            }),
        )
    }

    fn glass_with_scripted_key_snapshots(
        platform: ScriptedKeyPlatform,
        replies: Vec<SnapshotReply>,
    ) -> Glass {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let mut held = Some(Backend {
            platform: Box::new(platform),
            accessibility: Some(Box::new(ScriptedSnapshots {
                replies: replies.into(),
            })),
        });
        let factory: PlatformFactory = Box::new(move |_backend| {
            held.take()
                .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
        });
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
    }

    fn assert_not_started_was_upgraded_after_dispatch(error: &GlassError) {
        assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted), "{error}");
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller), "{error}");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched),
            "{error}"
        );
    }

    fn assert_option_not_found_dispatch(error: &GlassError, expected: crate::BoundDispatch) {
        assert!(
            matches!(error.cause(), GlassError::AxOptionNotFound(1, _, _)),
            "{error}"
        );
        assert_eq!(error.bound_dispatch(), Some(expected), "{error}");
    }

    #[test]
    fn a11y_resnapshot_rejects_reader_late_success_without_replacing_the_cache() {
        let mut original = fake_tree();
        original.root.name = Some("original".into());
        let mut late = fake_tree();
        late.root.name = Some("late".into());
        let mut g = glass_with_scripted_snapshots(
            FakePlatform::new(100, 100),
            vec![
                SnapshotReply::Tree(original),
                SnapshotReply::SleepPastDeadline(late),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .a11y_resnapshot(Deadline::from_millis(5))
            .expect_err("a reader result returned after the caller deadline is not success");

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(
            g.active
                .as_ref()
                .and_then(|session| session.last_ax.as_ref())
                .and_then(|tree| tree.root.name.as_deref()),
            Some("original"),
            "a late tree must not replace the last on-time snapshot"
        );
    }

    #[test]
    fn a11y_resnapshot_rejects_success_when_core_post_processing_spends_the_deadline() {
        let platform = FakePlatform::new(100, 100).with_drain_logs_delay(Duration::from_millis(20));
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();

        let error = g
            .a11y_resnapshot(Deadline::from_millis(5))
            .expect_err("post-processing that crosses the caller deadline is not success");

        assert_eq!(error.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert!(
            g.active
                .as_ref()
                .and_then(|session| session.last_ax.as_ref())
                .is_none(),
            "a tree whose core post-processing finished late must not be cached"
        );
    }

    #[test]
    fn combo_open_read_failure_requires_a_fresh_snapshot_before_retrying_actuation() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300).with_click_log(clicks.clone());
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::NotStarted("scripted combo open read"),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::Tree(combo("Delta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("the post-open read is scripted to fail");

        assert_not_started_was_upgraded_after_dispatch(&error);
        assert_eq!(clicks.lock().unwrap().len(), 1, "the combo was opened");

        let retry = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("a retry without a fresh snapshot must not reopen the combo");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "the stale pre-open state must not dispatch a second pointer click"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
    }

    #[test]
    fn combo_expanded_without_realized_options_resnapshots_before_refusing() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300)
            .with_click_log(clicks.clone())
            .with_key_log(keys.clone());
        let mut unrealized = combo("Beta", &[]);
        unrealized.root.children[0].states.expanded = true;
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(unrealized),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::Tree(combo("Delta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect("the realized option rows should complete the selection");

        assert!(
            clicks.lock().unwrap().is_empty(),
            "the popup was already open"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[
                KeyEvent::Chord("Down".to_string()),
                KeyEvent::Chord("Down".to_string()),
                KeyEvent::Chord("Return".to_string()),
            ]
        );
    }

    #[test]
    fn combo_selection_key_refusal_retries_same_option_without_closing_open_popup() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = ScriptedKeyPlatform {
            inner: FakePlatform::new(340, 300)
                .with_click_log(clicks.clone())
                .with_key_log(keys.clone()),
            replies: vec![Err(scripted_dispatch_error(
                "scripted combo selection key",
                crate::BoundDispatch::NotDispatched,
            ))]
            .into(),
        };
        let mut g = glass_with_scripted_key_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::Tree(combo("Delta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("the first option key is explicitly refused before dispatch");

        assert_not_started_was_upgraded_after_dispatch(&error);
        assert_eq!(clicks.lock().unwrap().len(), 1, "the combo was opened");
        assert!(
            keys.lock().unwrap().is_empty(),
            "the refused key was not sent"
        );

        g.set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect("the retained expanded snapshot should finish the same selection");
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying must not click the expanded combo and close its popup"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[
                KeyEvent::Chord("Down".to_string()),
                KeyEvent::Chord("Down".to_string()),
                KeyEvent::Chord("Return".to_string()),
            ]
        );
    }

    #[test]
    fn combo_verification_failure_requires_a_fresh_snapshot_before_retrying_actuation() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300)
            .with_click_log(clicks.clone())
            .with_key_log(keys.clone());
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::NotStarted("scripted combo verification read"),
                SnapshotReply::Tree(combo("Delta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("the verification read is scripted to fail");

        assert_not_started_was_upgraded_after_dispatch(&error);
        assert_eq!(clicks.lock().unwrap().len(), 1, "the combo was opened once");
        assert_eq!(
            keys.lock().unwrap().len(),
            3,
            "two option steps and the commit key were sent"
        );

        let retry = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("a retry without a fresh snapshot must not reuse pre-commit state");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "the stale open-popup state must not reopen the combo"
        );
        assert_eq!(
            keys.lock().unwrap().len(),
            3,
            "the stale open-popup state must not send another selection or commit key"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
        g.set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .unwrap();
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert_eq!(keys.lock().unwrap().len(), 3);
    }

    #[test]
    fn combo_fresh_expanded_return_retry_rejects_preview_without_reclicking() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = ScriptedKeyPlatform {
            inner: FakePlatform::new(340, 300)
                .with_click_log(clicks.clone())
                .with_key_log(keys.clone()),
            replies: vec![Err(scripted_dispatch_error(
                "scripted combo key",
                crate::BoundDispatch::MayHaveDispatched,
            ))]
            .into(),
        };
        let mut g = glass_with_scripted_key_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::Tree(expanded_combo_with_selected(
                    "Delta",
                    &["Alpha", "Beta", "Gamma", "Delta"],
                    "Delta",
                )),
                SnapshotReply::Tree(expanded_combo_with_selected(
                    "Delta",
                    &["Alpha", "Beta", "Gamma", "Delta"],
                    "Delta",
                )),
                SnapshotReply::Tree(combo("Delta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("the first selection key reports an ambiguous timeout");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert_eq!(keys.lock().unwrap().len(), 1);

        let retry = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("an ambiguous key dispatch requires a fresh snapshot before retry");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert_eq!(
            keys.lock().unwrap().len(),
            1,
            "the stale open-popup snapshot must not send a second selection key"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("an expanded preview after Return is not a committed value");
        assert!(
            matches!(&error, GlassError::AxValueNotApplied { id: 1, requested, observed, .. }
                if requested == "Delta" && observed.as_deref() == Some("Delta")),
            "{error}"
        );
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying must not click the freshly observed expanded combo"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[
                KeyEvent::Chord("Down".to_string()),
                KeyEvent::Chord("Return".to_string()),
            ]
        );

        g.set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect("the retained post-Return preview should retry the commit");
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying the retained preview must not pointer-click again"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[
                KeyEvent::Chord("Down".to_string()),
                KeyEvent::Chord("Return".to_string()),
                KeyEvent::Chord("Return".to_string()),
            ]
        );
    }

    #[test]
    fn combo_retained_return_retry_rejects_preview_without_reclicking() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = ScriptedKeyPlatform {
            inner: FakePlatform::new(340, 300)
                .with_click_log(clicks.clone())
                .with_key_log(keys.clone()),
            replies: vec![Err(scripted_dispatch_error(
                "scripted combo return",
                crate::BoundDispatch::NotDispatched,
            ))]
            .into(),
        };
        let mut g = glass_with_scripted_key_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::Tree(expanded_combo_with_selected(
                    "Delta",
                    &["Alpha", "Beta", "Gamma", "Delta"],
                    "Delta",
                )),
                SnapshotReply::Tree(expanded_combo_with_selected(
                    "Delta",
                    &["Alpha", "Beta", "Gamma", "Delta"],
                    "Delta",
                )),
                SnapshotReply::Tree(combo("Delta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("Return is explicitly refused before dispatch");
        assert_not_started_was_upgraded_after_dispatch(&error);
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert!(keys.lock().unwrap().is_empty());

        let error = g
            .set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect_err("an expanded preview after Return is not a committed value");
        assert!(
            matches!(&error, GlassError::AxValueNotApplied { id: 1, requested, observed, .. }
                if requested == "Delta" && observed.as_deref() == Some("Delta")),
            "{error}"
        );
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying Return must not close the already-open popup"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[KeyEvent::Chord("Return".to_string())]
        );

        g.set_value_by(AxNodeId(1), "Delta", Deadline::from_millis(2_000))
            .expect("the retained post-Return preview should retry the commit");
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying the retained preview must not pointer-click again"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[
                KeyEvent::Chord("Return".to_string()),
                KeyEvent::Chord("Return".to_string()),
            ]
        );
    }

    #[test]
    fn combo_escape_refusal_retries_unknown_option_without_closing_open_popup() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = ScriptedKeyPlatform {
            inner: FakePlatform::new(340, 300)
                .with_click_log(clicks.clone())
                .with_key_log(keys.clone()),
            replies: vec![Err(scripted_dispatch_error(
                "scripted combo escape",
                crate::BoundDispatch::NotDispatched,
            ))]
            .into(),
        };
        let mut g = glass_with_scripted_key_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::Tree(combo("Beta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let first = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("Omega is not an option and Escape is refused");
        assert_option_not_found_dispatch(&first, crate::BoundDispatch::MayHaveDispatched);
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert!(keys.lock().unwrap().is_empty());

        let retry = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("the same unknown option remains invalid");
        assert_option_not_found_dispatch(&retry, crate::BoundDispatch::MayHaveDispatched);
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying cleanup must not close the popup with a pointer click"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[KeyEvent::Chord("Escape".to_string())]
        );
    }

    #[test]
    fn combo_already_open_escape_refusal_proves_unknown_option_not_dispatched() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = ScriptedKeyPlatform {
            inner: FakePlatform::new(340, 300)
                .with_click_log(clicks.clone())
                .with_key_log(keys.clone()),
            replies: vec![Err(scripted_dispatch_error(
                "scripted combo escape",
                crate::BoundDispatch::NotDispatched,
            ))]
            .into(),
        };
        let mut g = glass_with_scripted_key_snapshots(
            platform,
            vec![SnapshotReply::Tree(combo(
                "Beta",
                &["Alpha", "Beta", "Gamma", "Delta"],
            ))],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("the open combo has no matching option and Escape is refused");

        assert_option_not_found_dispatch(&error, crate::BoundDispatch::NotDispatched);
        assert!(clicks.lock().unwrap().is_empty());
        assert!(keys.lock().unwrap().is_empty());
    }

    #[test]
    fn combo_already_open_ambiguous_escape_failure_marks_unknown_option_after_dispatch() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = ScriptedKeyPlatform {
            inner: FakePlatform::new(340, 300)
                .with_click_log(clicks.clone())
                .with_key_log(keys.clone()),
            replies: vec![Err(GlassError::Backend(
                "scripted combo cleanup transport failure".into(),
            ))]
            .into(),
        };
        let mut g = glass_with_scripted_key_snapshots(
            platform,
            vec![SnapshotReply::Tree(combo(
                "Beta",
                &["Alpha", "Beta", "Gamma", "Delta"],
            ))],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("the missing option remains primary after ambiguous cleanup failure");

        assert_option_not_found_dispatch(&error, crate::BoundDispatch::MayHaveDispatched);
        assert!(clicks.lock().unwrap().is_empty());
        assert_eq!(
            &*keys.lock().unwrap(),
            &[KeyEvent::Chord("Escape".to_string())]
        );
    }

    #[test]
    fn combo_timed_out_open_read_retries_without_closing_open_popup() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300)
            .with_click_log(clicks.clone())
            .with_key_log(keys.clone());
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::TimedOut("scripted combo open read"),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let first = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("the combo open read times out before unknown-option cleanup");
        assert_eq!(first.bound(), Some(crate::BoundKind::TimedOut));
        assert_eq!(first.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            first.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert!(
            keys.lock().unwrap().is_empty(),
            "Escape is skipped after the timed-out read"
        );

        let stale_retry = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("a timed-out read cannot support a retry");
        assert!(matches!(stale_retry, GlassError::NoAxSnapshot));
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert!(keys.lock().unwrap().is_empty());

        g.a11y_resnapshot(Deadline::from_millis(2_000))
            .expect("an on-time snapshot safely recovers the retained popup state");
        let retry = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("the same unknown option remains invalid");
        assert_option_not_found_dispatch(&retry, crate::BoundDispatch::MayHaveDispatched);
        assert_eq!(
            clicks.lock().unwrap().len(),
            1,
            "retrying cleanup must not close the retained expanded popup"
        );
        assert_eq!(
            &*keys.lock().unwrap(),
            &[KeyEvent::Chord("Escape".to_string())]
        );
    }

    #[test]
    fn combo_unknown_option_escape_requires_a_fresh_snapshot_before_retrying() {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300)
            .with_click_log(clicks.clone())
            .with_key_log(keys.clone());
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(combo("Beta", &[])),
                SnapshotReply::Tree(combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"])),
                SnapshotReply::Tree(combo("Beta", &[])),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("Omega is not an option");
        assert_option_not_found_dispatch(&error, crate::BoundDispatch::MayHaveDispatched);
        assert_eq!(clicks.lock().unwrap().len(), 1, "the combo was opened once");
        assert_eq!(
            &*keys.lock().unwrap(),
            &[KeyEvent::Chord("Escape".to_string())],
            "the open popup was dismissed once"
        );

        let retry = g
            .set_value_by(AxNodeId(1), "Omega", Deadline::from_millis(2_000))
            .expect_err("a retry without a fresh snapshot must not reuse the open-popup tree");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(clicks.lock().unwrap().len(), 1);
        assert_eq!(keys.lock().unwrap().len(), 1);

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
    }

    /// The commit walks from the currently-selected option to the wanted one, so both the
    /// direction and the number of steps come from their index difference. Asserted on the
    /// keystrokes: the end state alone is reached by any number of Downs past the target.
    #[test]
    fn a_combo_that_did_not_commit_names_the_selection_it_still_shows() {
        // The selection a combo shows is its name — an edit reaching for `value`, which no combo
        // carries, would report every one of them as unreadable.
        let platform = FakePlatform::new(340, 300).with_key_log(Arc::new(Mutex::new(Vec::new())));
        let (mut g, _) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Beta", &[]),
                combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"]),
                combo("Beta", &[]),
            ],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g.set_value(AxNodeId(1), "Delta").unwrap_err();
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { id: 1, requested, observed, .. }
                if requested == "Delta" && observed.as_deref() == Some("Beta")),
            "{err}"
        );
    }

    #[test]
    fn a_combo_no_longer_where_it_was_reports_no_reading_rather_than_no_value() {
        // Committing can reflow the form out from under the combo; nothing was read, so the verdict
        // must not claim it holds nothing — that reads as a combo with no selection.
        let platform = FakePlatform::new(340, 300).with_key_log(Arc::new(Mutex::new(Vec::new())));
        let (mut g, _) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Beta", &[]),
                combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"]),
                tree_with(340, 300, vec![]),
            ],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g.set_value(AxNodeId(1), "Delta").unwrap_err();
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { observed: None, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("could not be read back"), "{err}");
    }

    #[test]
    fn set_combo_value_steps_from_the_current_selection_to_the_target() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300).with_key_log(keys.clone());
        // Selected is "Beta" (index 1); the target "Delta" is index 3, so two Downs forward.
        let (mut g, _) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Beta", &[]),
                combo("Beta", &["Alpha", "Beta", "Gamma", "Delta"]),
                combo("Delta", &[]),
            ],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g.set_value(AxNodeId(1), "Delta").unwrap();

        let chords: Vec<String> = keys
            .lock()
            .unwrap()
            .iter()
            .filter_map(|k| match k {
                KeyEvent::Chord(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chords,
            vec!["Down", "Down", "Return"],
            "two steps forward from index 1 to index 3, then commit"
        );
    }

    /// The mirror: a target above the current selection walks up, not down.
    #[test]
    fn set_combo_value_walks_up_to_an_earlier_option() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300).with_key_log(keys.clone());
        // Selected is "Delta" (index 3); the target "Alpha" is index 0, so three Ups.
        let (mut g, _) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Delta", &[]),
                combo("Delta", &["Alpha", "Beta", "Gamma", "Delta"]),
                combo("Alpha", &[]),
            ],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g.set_value(AxNodeId(1), "Alpha").unwrap();

        let chords: Vec<String> = keys
            .lock()
            .unwrap()
            .iter()
            .filter_map(|k| match k {
                KeyEvent::Chord(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(chords, vec!["Up", "Up", "Up", "Return"]);
    }

    /// With nothing selected the walk starts from the first option, so the step count is the
    /// target's own index.
    #[test]
    fn set_combo_value_starts_from_the_first_option_when_none_is_selected() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300).with_key_log(keys.clone());
        // `combo` marks an option selected by matching the name, so a name outside the option
        // list leaves every one of them unselected.
        let (mut g, _) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Nothing", &[]),
                combo("Nothing", &["Alpha", "Beta", "Gamma"]),
                combo("Gamma", &[]),
            ],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g.set_value(AxNodeId(1), "Gamma").unwrap();

        let chords: Vec<String> = keys
            .lock()
            .unwrap()
            .iter()
            .filter_map(|k| match k {
                KeyEvent::Chord(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(chords, vec!["Down", "Down", "Return"], "index 0 to index 2");
    }

    #[test]
    fn the_combo_path_waits_for_the_popup_to_realize() {
        // A tree read the instant a popup opens or closes shows the previous state, so both re-reads
        // in the combo path follow a settle. Timed rather than asserted structurally: the settle is
        // a sleep, so removing it leaves every other assertion in this file green.
        let platform = FakePlatform::new(340, 300);
        let (mut g, _invoke_log) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Acme", &[]),
                combo("Acme", &["Acme", "Globex"]),
                combo("Globex", &[]),
            ],
            InvokeBehavior::Succeed,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let started = std::time::Instant::now();
        g.set_value(AxNodeId(1), "Globex").unwrap();
        let elapsed = started.elapsed();

        // Two settles, 250ms each; a generous floor so a loaded machine cannot make this flake.
        assert!(
            elapsed >= std::time::Duration::from_millis(400),
            "combo commit returned in {elapsed:?}, too fast to have settled twice"
        );
    }

    #[test]
    fn set_value_on_a_combo_opens_the_popup_with_a_pointer_click_even_when_invoke_succeeds() {
        // The combo commit loop is keyboard navigation, so the popup must be opened by
        // something that takes keyboard focus — a native expand (UIA's ExpandCollapsePattern)
        // doesn't, and the keystrokes would go to whatever had focus instead.
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(340, 300).with_click_log(clicks.clone());
        let (mut g, invoke_log) = glass_with_a11y_seq_invoke(
            platform,
            vec![
                combo("Acme", &[]),                 // cached: closed, showing Acme
                combo("Acme", &["Acme", "Globex"]), // after the open: expanded + options
                combo("Globex", &[]),               // after the commit: closed, Globex
            ],
            InvokeBehavior::Succeed,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.set_value(AxNodeId(1), "Globex").unwrap();

        assert!(
            invoke_log.lock().unwrap().is_empty(),
            "the popup open must not use the native action — it wouldn't take keyboard focus"
        );
        let clicks = clicks.lock().unwrap();
        assert_eq!(
            clicks.len(),
            1,
            "exactly one pointer click: the popup open, {clicks:?}"
        );
        assert_eq!(clicks[0], (160, 205), "the combo's own clamped center");
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
        g.a11y_snapshot(None).unwrap();
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
    fn set_value_refreshes_geometry_so_the_backend_sees_the_current_window() {
        // The mirror of the `invoke` case: `set_value` fingerprints by window-RELATIVE bounds
        // from `ctx.window` too, so a window moved since the snapshot reads as drift.
        let snapshot_geo = WindowGeometry {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let moved_geo = WindowGeometry {
            x: 640,
            y: 480,
            width: 100,
            height: 100,
        };
        let platform = FakePlatform::new(100, 100)
            .resized_to(snapshot_geo)
            .resized_to(moved_geo.clone());
        // The invoke knob is irrelevant here (set_value never invokes); this builder is just the
        // one that hands back the ctx log.
        let (mut g, _, ctx_log) =
            glass_with_a11y_invoke_ctx(platform, fake_tree(), InvokeBehavior::Unsupported);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // consumes the first scripted geometry read

        g.set_value(AxNodeId(1), "hello").unwrap(); // fake_tree: #1 is Button "Save"

        assert_eq!(
            ctx_log.lock().unwrap().as_ref().map(|c| c.window.clone()),
            Some(moved_geo),
            "set_value's ctx carries the refreshed window, not the snapshot's stale one"
        );
    }

    #[test]
    fn set_value_passes_target_and_text_to_backend() {
        // Build a Glass whose fake records set_value calls, keeping the Arc to inspect.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let log2 = log.clone();
        let mut accessibility = FakeAccessibility::new(fake_tree());
        accessibility.set_log = log2;
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(FakePlatform::new(100, 100)),
            accessibility: Some(Box::new(accessibility)),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // fake_tree: #1 is Button "Save"
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
                value: None,
            }
        );
        assert_eq!(calls[0].1, "hello");
    }

    /// A one-node tree whose root is an editable field holding `value` — the shape the cache
    /// tests need, since `set_value` targets the node the snapshot cached.
    fn editable_field_tree(value: Option<&str>) -> AxTree {
        AxTree::new(AxNode {
            id: AxNodeId(0),
            role: AxRole::TextField,
            raw_role: "text field".into(),
            name: Some("Name".into()),
            description: None,
            value: value.map(Into::into),
            states: AxStates {
                editable: true,
                ..Default::default()
            },
            bounds: Some(rect(0, 0, 50, 20)),
            children: vec![],
        })
    }

    /// A started, snapshotted session over `accessibility` — ready for the `set_value` call the
    /// test is actually about.
    fn glass_ready_for_set_value(accessibility: Box<dyn Accessibility + Send>) -> Glass {
        let mut g = glass_with_backend(FakePlatform::new(100, 100), accessibility);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        g
    }

    /// [`FakeAccessibility`] over `tree`, logging its writes to `set_log`.
    fn logging_a11y(
        tree: AxTree,
        set_log: Arc<Mutex<Vec<(AxTarget, String)>>>,
    ) -> FakeAccessibility {
        let mut accessibility = FakeAccessibility::new(tree);
        accessibility.set_log = set_log;
        accessibility
    }

    fn glass_with_geometry_failure_ready_for_set_value(
        set_log: Arc<Mutex<Vec<(AxTarget, String)>>>,
    ) -> Glass {
        let tree = editable_field_tree(Some("orig"));
        let mut g = glass_with_backend(
            FakePlatform::new(100, 100).with_failing_geometry(),
            Box::new(logging_a11y(tree.clone(), set_log)),
        );
        g.start(&spec()).unwrap();
        let mut cached = tree;
        cached.assign_ids();
        g.active.as_mut().unwrap().last_ax = Some(cached);
        g
    }

    #[test]
    fn legacy_set_value_ignores_a_pre_dispatch_geometry_probe_failure() {
        let set_log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with_geometry_failure_ready_for_set_value(set_log.clone());

        g.set_value(AxNodeId(0), "new").unwrap();

        assert_eq!(set_log.lock().unwrap().len(), 1);
    }

    #[test]
    fn bounded_set_value_propagates_a_pre_dispatch_geometry_probe_failure() {
        let set_log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with_geometry_failure_ready_for_set_value(set_log.clone());

        let error = g
            .set_value_by(AxNodeId(0), "new", Deadline::from_millis(1_000))
            .expect_err("bounded geometry failure must stop before the value backend");

        assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted));
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::NotDispatched)
        );
        assert!(set_log.lock().unwrap().is_empty());
    }

    #[test]
    fn set_value_patches_last_ax_so_a_retry_carries_the_written_value() {
        // Same mock harness as `set_value_passes_target_and_text_to_backend`: asserting on
        // `set_log` proves the cache patch directly. The real drift guard lives in
        // `editable_target` (glass-android), not here, so this doesn't reimplement it.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_ready_for_set_value(Box::new(logging_a11y(
            editable_field_tree(Some("orig")),
            log.clone(),
        )));

        g.set_value(AxNodeId(0), "a").unwrap();
        g.set_value(AxNodeId(0), "b").unwrap(); // no intervening snapshot

        let calls = log.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].0.value,
            Some("a".into()),
            "the second call's target must carry what the first call wrote, not the pre-write \
             snapshot value"
        );
    }

    #[test]
    fn set_value_patches_a_cleared_field_to_no_value_at_all() {
        // Android reports an emptied field as no value rather than `Some("")`, so caching the
        // empty string after a successful clear would make `set_value(id, "")` then
        // `set_value(id, "text")` — ordinary agent sequencing — look like drift on the second.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_ready_for_set_value(Box::new(logging_a11y(
            editable_field_tree(Some("orig")),
            log.clone(),
        )));

        g.set_value(AxNodeId(0), "").unwrap();
        g.set_value(AxNodeId(0), "text").unwrap(); // no intervening snapshot

        assert_eq!(
            log.lock().unwrap()[1].0.value,
            None,
            "a cleared field caches as no value, the shape the reader would report"
        );
    }

    /// An `Accessibility` whose `set_value` fails the first call and succeeds every one after,
    /// logging successful calls like `FakeAccessibility`. `FakeAccessibility::set_fail` is a
    /// fixed knob, not a script, and this needs to vary across one session's two calls — kept
    /// local since only one test needs it.
    struct FlakyOnceAccessibility {
        tree: AxTree,
        failed_once: bool,
        set_log: Arc<Mutex<Vec<(AxTarget, String)>>>,
    }

    impl Accessibility for FlakyOnceAccessibility {
        fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
            Ok(self.tree.clone())
        }
        fn set_value(&mut self, _ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
            if !self.failed_once {
                self.failed_once = true;
                return Err(GlassError::value_not_applied(target.id.0, text, None));
            }
            self.set_log
                .lock()
                .unwrap()
                .push((target.clone(), text.to_string()));
            Ok(())
        }
    }

    #[test]
    fn set_value_invalidates_the_cached_value_after_a_write_that_may_have_landed() {
        // `AxValueNotApplied` is raised after the keystrokes went out — the AVD false-failure
        // `typed_clear_landed` documents (an emptied field reporting its placeholder) is one — so
        // the cached value is no longer a fact and must not gate the retry.
        let set_log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_ready_for_set_value(Box::new(FlakyOnceAccessibility {
            tree: editable_field_tree(Some("orig")),
            failed_once: false,
            set_log: set_log.clone(),
        }));

        assert!(
            g.set_value(AxNodeId(0), "a").is_err(),
            "first write is scripted to fail"
        );
        g.set_value(AxNodeId(0), "a").unwrap(); // retry, no intervening snapshot

        assert_eq!(
            set_log.lock().unwrap()[0].0.value,
            None,
            "the retry's target must not carry the stale pre-failure value"
        );
    }

    /// An `Accessibility` that guards its write on the target's value the way Android's
    /// `editable_target` does — over the same [`AxTarget::value_consistent`], so this scripts the
    /// guard rather than reimplementing it. `held` is what the live element reads.
    struct GuardedAccessibility {
        tree: AxTree,
        held: Option<String>,
        set_log: Arc<Mutex<Vec<(AxTarget, String)>>>,
    }

    impl Accessibility for GuardedAccessibility {
        fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
            Ok(self.tree.clone())
        }
        fn set_value(&mut self, _ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
            if !target.value_consistent(self.held.as_deref()) {
                return Err(GlassError::AxElementChanged(target.id.0));
            }
            self.held = Some(text.to_string());
            self.set_log
                .lock()
                .unwrap()
                .push((target.clone(), text.to_string()));
            Ok(())
        }
    }

    #[test]
    fn a_write_rejected_before_dispatch_keeps_the_fingerprint_that_rejected_it() {
        // The recycled-row case: the snapshot read "Alice", the row now holds "Zara", and the
        // guard rejects. An agent retries without re-snapshotting — blanking the cached value on
        // that rejection would send the retry in with no value to compare, skipping the guard and
        // writing to the wrong row.
        let set_log = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_ready_for_set_value(Box::new(GuardedAccessibility {
            tree: editable_field_tree(Some("Alice")),
            held: Some("Zara".into()),
            set_log: set_log.clone(),
        }));

        for attempt in ["first", "retry"] {
            assert!(
                matches!(
                    g.set_value(AxNodeId(0), "x"),
                    Err(GlassError::AxElementChanged(0))
                ),
                "the {attempt} must be rejected as drift"
            );
        }
        assert!(
            set_log.lock().unwrap().is_empty(),
            "no write may land on the drifted row"
        );
    }

    /// [`GuardedAccessibility`] whose first call fails before dispatching anything — the shape of
    /// Android's `set_value`, which re-snapshots and reaches for adb before it taps.
    struct FirstFailureThenGuarded {
        first_failure: Option<GlassError>,
        guarded: GuardedAccessibility,
    }

    impl Accessibility for FirstFailureThenGuarded {
        fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
            self.guarded.snapshot(ctx)
        }
        fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
            match self.first_failure.take() {
                Some(e) => Err(e),
                None => self.guarded.set_value(ctx, target, text),
            }
        }
    }

    #[test]
    fn a_pre_dispatch_transport_failure_keeps_the_value_that_guards_the_retry() {
        // A not-ready `uiautomator dump` or an adb hiccup fails the guard's own re-snapshot, before
        // anything is typed — as `Backend` or `AccessibilityUnavailable`, the same variants that
        // post-dispatch failures use, so neither can be classified by variant. The captured value
        // is still a fact here; blanking it would let the retry write to the recycled row.
        for failure in [
            GlassError::Backend("uiautomator dump not ready".into()),
            GlassError::AccessibilityUnavailable("adb: device offline".into()),
            GlassError::Bounded {
                kind: crate::BoundKind::NotStarted,
                whose: crate::Whose::Caller,
                dispatch: crate::BoundDispatch::NotDispatched,
                message: "the write was refused before dispatch".into(),
            },
            GlassError::Bounded {
                kind: crate::BoundKind::TimedOut,
                whose: crate::Whose::Caller,
                dispatch: crate::BoundDispatch::MayHaveDispatched,
                message: "the guard snapshot may have gone out".into(),
            },
        ] {
            let set_log = Arc::new(Mutex::new(Vec::new()));
            let mut g = glass_ready_for_set_value(Box::new(FirstFailureThenGuarded {
                first_failure: Some(failure),
                guarded: GuardedAccessibility {
                    tree: editable_field_tree(Some("Alice")),
                    held: Some("Zara".into()), // the row recycled under the snapshot
                    set_log: set_log.clone(),
                },
            }));

            assert!(
                g.set_value(AxNodeId(0), "x").is_err(),
                "first write is scripted to fail before dispatch"
            );
            assert!(
                matches!(
                    g.set_value(AxNodeId(0), "x"),
                    Err(GlassError::AxElementChanged(0))
                ),
                "the retry must still be guarded by the captured value"
            );
            assert!(
                set_log.lock().unwrap().is_empty(),
                "no write may land on the drifted row"
            );
        }
    }

    #[test]
    fn a_post_dispatch_failure_lets_the_retry_through_once_the_field_settles() {
        // The sequence glass#405 is about, end to end: a write that dispatched and could not be
        // confirmed, a field that then settles to the text it sent, and a retry the guard must
        // accept. Only the operation-specific post-write verdicts may clear the cache.
        for failure in [
            GlassError::AxWriteUnconfirmed(0, "the result was lost".into()),
            GlassError::value_not_applied(0, "x", Some("Alice")),
        ] {
            let set_log = Arc::new(Mutex::new(Vec::new()));
            let mut g = glass_ready_for_set_value(Box::new(FirstFailureThenGuarded {
                first_failure: Some(failure),
                guarded: GuardedAccessibility {
                    tree: editable_field_tree(Some("Alice")),
                    // The device applied the write the caller was told it could not confirm.
                    held: Some("x".into()),
                    set_log: set_log.clone(),
                },
            }));

            assert!(
                g.set_value(AxNodeId(0), "x").is_err(),
                "first write is scripted to fail after dispatch"
            );
            g.set_value(AxNodeId(0), "x")
                .expect("the retry must not be refused as drift for a write that landed");
            assert_eq!(
                set_log.lock().unwrap().len(),
                1,
                "exactly the retry reaches the element"
            );
        }
    }

    /// The control for the wait's deadline test: a reader handed an invented deadline abandons a
    /// read it was never told to hurry.
    #[test]
    fn a_snapshot_no_caller_timed_leaves_the_reader_its_own_budget() {
        let (mut g, ctx_log) = glass_with_a11y_ctx(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("snapshot recorded its ctx")
                .deadline,
            Deadline::UNBOUNDED,
        );
    }

    #[test]
    fn wait_safety_snapshot_uses_cached_geometry_for_pre_dispatch_refusal() {
        let mut g = glass_with_a11y(
            FakePlatform::new(100, 100).with_failing_geometry(),
            fake_tree(),
        );
        g.start(&spec()).unwrap();

        let tree = g
            .a11y_resnapshot_for_wait(Deadline::from_millis(1_000))
            .expect("the exact wait fallback may reuse cached geometry");

        assert_eq!(tree.root.role, AxRole::Window);
    }

    #[test]
    fn a11y_snapshot_threads_max_nodes_into_ctx_limits_and_set_value_reuses_them() {
        // Inspects `ctx_log` — the `AxContext.limits` the backend actually received. Real
        // backends still ignore `limits` (they build `WalkBudget::new()`), so this is the only
        // way to observe the plumbing.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let ctx_log = Arc::new(Mutex::new(None));
        let mut accessibility = FakeAccessibility::new(fake_tree());
        accessibility.ctx_log = ctx_log.clone();
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(FakePlatform::new(100, 100)),
            accessibility: Some(Box::new(accessibility)),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();

        g.a11y_snapshot(Some(5000)).unwrap();
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("snapshot recorded its ctx")
                .limits
                .nodes,
            5000,
            "snapshot ctx carries the raised cap"
        );

        g.set_value(AxNodeId(1), "x").unwrap(); // fake_tree: #1 is Button "Save"
        assert_eq!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("set_value recorded its ctx")
                .limits
                .nodes,
            5000,
            "set_value reuses the snapshot's limits"
        );
    }

    #[test]
    fn set_value_propagates_backend_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let mut accessibility = FakeAccessibility::new(fake_tree());
        accessibility.set_fail = true;
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(FakePlatform::new(100, 100)),
            accessibility: Some(Box::new(accessibility)),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert!(matches!(
            g.set_value(AxNodeId(1), "x").unwrap_err(),
            GlassError::AxElementNotEditable(1)
        ));
    }

    /// A switch "Sw" as the readers report one *after* subrole normalization: `ToggleButton`,
    /// row shaped (300x30 at the origin), checkable — the single child of a root Window, so
    /// pre-order numbering gives it id 1. Shared by the trailing-toggle `set_value` tests below.
    ///
    /// Do not re-role it to `CheckBox`: that tests the swipe path against a role no backend
    /// produces.
    fn sw(checked: bool) -> AxTree {
        let switch = AxNode {
            id: AxNodeId(0),
            role: AxRole::ToggleButton,
            raw_role: "switch".into(),
            name: Some("Sw".into()),
            description: None,
            value: None,
            states: AxStates {
                checkable: true,
                checked,
                ..Default::default()
            },
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 300,
                height: 30,
            }),
            children: vec![],
        };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Win".into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 400,
                height: 400,
            }),
            children: vec![switch],
        };
        AxTree::new(root)
    }

    #[test]
    fn set_value_true_swipes_an_unchecked_ios_switch_and_verifies() {
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        // Snapshot #1 (cached read) = unchecked; snapshot #2 (verify re-read) = checked.
        let mut g = glass_with_a11y_seq(platform, vec![sw(false), sw(true)]);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // caches unchecked

        g.set_value(AxNodeId(1), "true").unwrap();

        assert_eq!(drags.lock().unwrap().len(), 1, "a toggle swipe was emitted");
    }

    #[test]
    fn toggle_verification_failure_requires_a_fresh_snapshot_before_retrying_actuation() {
        let drags = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(sw(false)),
                SnapshotReply::Tree(sw(false)),
                SnapshotReply::NotStarted("scripted toggle verification read"),
                SnapshotReply::Tree(sw(true)),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .expect_err("a later toggle verification read is scripted to fail");

        assert_not_started_was_upgraded_after_dispatch(&error);
        assert_eq!(drags.lock().unwrap().len(), 1, "the toggle was actuated");

        let retry = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .expect_err("a retry without a fresh snapshot must not reuse pre-toggle state");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the stale pre-toggle state must not dispatch a second actuation"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
        g.set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .unwrap();
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the fresh post-toggle state makes the retry a truthful no-op"
        );
    }

    #[test]
    fn toggle_actuation_error_that_may_have_dispatched_requires_a_fresh_snapshot() {
        let invokes = Arc::new(AtomicUsize::new(0));
        let platform = FakePlatform::new(400, 400).with_trailing_toggle_backend();
        let mut g = glass_with_backend(
            platform,
            Box::new(InvokeErrorAccessibility {
                trees: vec![sw(false), sw(true)].into(),
                invokes: invokes.clone(),
                dispatch: crate::BoundDispatch::MayHaveDispatched,
            }),
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let error = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .expect_err("the scripted native actuation reports an ambiguous timeout");
        assert_eq!(
            error.bound_dispatch(),
            Some(crate::BoundDispatch::MayHaveDispatched)
        );
        assert_eq!(invokes.load(Ordering::SeqCst), 1);

        let retry = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .expect_err("an ambiguous actuation requires a fresh snapshot before retry");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(
            invokes.load(Ordering::SeqCst),
            1,
            "the stale pre-toggle state must not dispatch a second native action"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
        g.set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .unwrap();
        assert_eq!(invokes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_swipe_path_is_chosen_by_checkable_not_by_role() {
        // What makes the switch normalization safe: a switch now reports `ToggleButton` where iOS
        // said `CheckBox` and macOS said `Button` or `CheckBox` depending on the toolkit, and a
        // path keyed off the role would have stopped actuating switches with no test noticing.
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        for (role, checkable, want_swipe) in [
            (AxRole::ToggleButton, true, true),
            (AxRole::CheckBox, true, true),
            // The fact the path actually keys on: same role, not checkable, no swipe.
            (AxRole::ToggleButton, false, false),
        ] {
            let platform = FakePlatform::new(400, 400)
                .with_drag_log(drags.clone())
                .with_trailing_toggle_backend();
            let mut tree = sw(false);
            tree.root.children[0].role = role;
            tree.root.children[0].states.checkable = checkable;
            let mut g = glass_with_a11y_seq(platform, vec![tree]);
            g.start(&spec()).unwrap();
            g.a11y_snapshot(None).unwrap();
            drags.lock().unwrap().clear();

            g.click_element(AxNodeId(1)).unwrap();

            assert_eq!(
                drags.lock().unwrap().len(),
                usize::from(want_swipe),
                "{role:?} checkable={checkable}"
            );
        }
    }

    #[test]
    fn set_value_toggle_path_uses_native_invoke_when_available() {
        // The trailing-toggle `set_value` branch actuates through `click_element_inner`, so on
        // a backend that HAS a native action the toggle must fire natively — no swipe — and
        // still verify the flip by re-snapshot. (The iOS backend this branch exists for has no
        // invoke today, so only a fake can pin the interaction.)
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        // Snapshot #1 (cached read) = unchecked; snapshot #2 (verify re-read) = checked.
        let (mut g, invoke_log) = glass_with_a11y_seq_invoke(
            platform,
            vec![sw(false), sw(true)],
            InvokeBehavior::Succeed,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.set_value(AxNodeId(1), "true").unwrap();

        assert_eq!(
            invoke_log.lock().unwrap().len(),
            1,
            "the toggle actuated once, natively"
        );
        assert!(
            drags.lock().unwrap().is_empty(),
            "the native action replaces the swipe — actuating both would toggle twice"
        );
    }

    #[test]
    fn set_value_true_is_a_noop_when_already_checked() {
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y(platform, sw(true));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.set_value(AxNodeId(1), "true").unwrap();

        assert!(
            drags.lock().unwrap().is_empty(),
            "already true -> no actuation"
        );
    }

    #[test]
    fn a_toggle_whose_control_left_the_screen_reports_no_reading_rather_than_a_state() {
        // The actuation is a swipe, which can carry the row away; no checkable was read, so naming
        // "on" or "off" would be a state nobody observed.
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(Arc::new(Mutex::new(Vec::new())))
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y_seq(platform, vec![sw(false), tree_with(400, 400, vec![])]);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g.set_value(AxNodeId(1), "true").unwrap_err();
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { observed: None, .. }),
            "{err}"
        );
    }

    #[test]
    fn set_value_errors_when_the_toggle_does_not_apply() {
        let drags = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(sw(false)),
                SnapshotReply::Tree(sw(false)),
                SnapshotReply::TimedOut("scripted toggle verification read"),
                SnapshotReply::Tree(sw(true)),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(10_000))
            .unwrap_err();
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { id: 1, requested, observed, .. }
                if requested == "true" && observed.as_deref() == Some("off")),
            "{err}"
        );
        assert_eq!(drags.lock().unwrap().len(), 1, "the toggle was actuated");

        let retry = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .expect_err("a verification ceiling requires a fresh snapshot before retry");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the stale unchecked poll must not dispatch a second toggle"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
        g.set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .unwrap();
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the fresh checked state makes the retry a truthful no-op"
        );
    }

    #[test]
    fn set_value_false_swipes_a_checked_ios_switch_and_verifies() {
        // The fixed swipe is a TOGGLE gesture (proven on-device: identical swipes alternate
        // off/on/off/on — see `AxRect::trailing_toggle_swipe`), so the same swipe that turns a
        // switch on turns it off; there is no direction logic to exercise separately.
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        // Snapshot #1 (cached read) = checked; snapshot #2 (verify re-read) = unchecked.
        let mut g = glass_with_a11y_seq(platform, vec![sw(true), sw(false)]);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // caches checked

        g.set_value(AxNodeId(1), "false").unwrap();

        assert_eq!(drags.lock().unwrap().len(), 1, "a toggle swipe was emitted");
    }

    #[test]
    fn set_value_false_is_a_noop_when_already_unchecked() {
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y(platform, sw(false));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.set_value(AxNodeId(1), "false").unwrap();

        assert!(
            drags.lock().unwrap().is_empty(),
            "already false -> no actuation"
        );
    }

    /// Two same-named ("Sw") row-shaped switches at DIFFERENT bounds — a `sibling` listed
    /// before the `target` (so a naive first-match-by-name search hits the sibling first,
    /// the exact silent-success risk `find_checkable_near` exists to rule out), and a
    /// `target` at the bounds `set_value` actually addresses. Pre-order id assignment gives
    /// the sibling id 1 and the target id 2. Shared by the disambiguation tests below.
    fn two_switches(sibling_checked: bool, target_checked: bool) -> AxTree {
        let make = |checked: bool, y: i32| AxNode {
            id: AxNodeId(0),
            role: AxRole::CheckBox,
            raw_role: "switch".into(),
            name: Some("Sw".into()),
            description: None,
            value: None,
            states: AxStates {
                checkable: true,
                checked,
                ..Default::default()
            },
            bounds: Some(AxRect {
                x: 0,
                y,
                width: 300,
                height: 30,
            }),
            children: vec![],
        };
        let sibling = make(sibling_checked, 0);
        let target = make(target_checked, 200);
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Win".into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 400,
                height: 400,
            }),
            children: vec![sibling, target],
        };
        AxTree::new(root)
    }

    #[test]
    fn set_value_true_verifies_the_target_by_bounds_when_a_same_named_sibling_is_already_checked() {
        // The sibling "Sw" is already checked (the wanted state) throughout; only the TARGET
        // flips false -> true on the verify re-snapshot. A name-only verify would match the
        // sibling — listed first — and return Ok whether or not the target ever moved.
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y_seq(
            platform,
            vec![two_switches(true, false), two_switches(true, true)],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap(); // caches: sibling already checked, target unchecked

        // Target is id 2 (sibling listed first gets id 1; see `two_switches`).
        g.set_value(AxNodeId(2), "true").unwrap();

        assert_eq!(drags.lock().unwrap().len(), 1, "a toggle swipe was emitted");
    }

    #[test]
    fn set_value_true_errors_when_only_a_same_named_sibling_is_checked() {
        // Same setup, but the swipe "does not take": the target stays unchecked on the verify
        // re-snapshot while the sibling remains (coincidentally) already checked. A name-only
        // verify would match the sibling first and return a false Ok.
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(Arc::new(Mutex::new(Vec::new())))
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y_seq(
            platform,
            vec![two_switches(true, false), two_switches(true, false)],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g.set_value(AxNodeId(2), "true").unwrap_err();
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { id: 2, requested, observed, .. }
                if requested == "true" && observed.as_deref() == Some("off")),
            "{err}"
        );
    }

    #[test]
    fn set_value_on_a_non_checkable_element_ignores_the_trailing_toggle_gate() {
        // The toggle gate must intercept only CHECKABLE elements — `checkable` is the
        // discriminator, not "did the text parse as a bool". Uses a BOOLEAN value ("true")
        // deliberately: with non-bool text both arms fall through to the delegate alike, so a
        // dropped `checkable` guard would be invisible.
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let log2 = log.clone();
        let mut accessibility = FakeAccessibility::new(fake_tree());
        accessibility.set_log = log2;
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(
                FakePlatform::new(100, 100)
                    .with_drag_log(drags.clone())
                    .with_trailing_toggle_backend(),
            ),
            // fake_tree: #1 is a non-checkable Button "Save"
            accessibility: Some(Box::new(accessibility)),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        g.set_value(AxNodeId(1), "true").unwrap();

        let calls = log.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "the delegate accessibility set_value path was taken"
        );
        assert_eq!(calls[0].1, "true");
        assert!(
            drags.lock().unwrap().is_empty(),
            "a non-checkable element must never trigger the toggle swipe"
        );
    }

    #[test]
    fn set_value_on_a_checkable_rejects_non_boolean_text() {
        // A non-boolean value on a checkable+trailing target must ERROR, never fall through to
        // the tap+type delegate, which would tap the inert label, type into nothing, and still
        // report Ok.
        let drags: Arc<Mutex<Vec<PointerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let log2 = log.clone();
        let mut accessibility = FakeAccessibility::new(sw(false));
        accessibility.set_log = log2;
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(
                FakePlatform::new(400, 400)
                    .with_drag_log(drags.clone())
                    .with_trailing_toggle_backend(),
            ),
            // sw(false): #1 is the checkable switch "Sw"
            accessibility: Some(Box::new(accessibility)),
        });
        let factory: PlatformFactory = Box::new(move |_b| {
            held.take()
                .ok_or_else(|| GlassError::Backend("twice".into()))
        });
        let mut g = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g.set_value(AxNodeId(1), "banana").unwrap_err();

        // The error must be the switch-specific "expects a boolean" one, and its message must
        // actually guide the agent (name the accepted values + echo the bad input) — NOT a generic
        // `AxValueNotApplied`, which reads as a text field that would not take the text.
        assert!(
            matches!(err, GlassError::AxValueNotBoolean(1, ref got) if got == "banana"),
            "{err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("true/false") && msg.contains("banana"),
            "error must tell the agent to pass a boolean, not misdirect it: {msg}"
        );
        assert!(
            drags.lock().unwrap().is_empty(),
            "an unparseable value must never trigger the toggle swipe"
        );
        assert!(
            log.lock().unwrap().is_empty(),
            "an unparseable value must never fall through to the tap+type delegate"
        );
    }

    #[test]
    fn parse_bool_accepts_the_documented_spellings() {
        for t in ["true", "on", "1", "yes", "TRUE"] {
            assert_eq!(parse_bool(t), Some(true));
        }
        for f in ["false", "off", "0", "no", "OFF"] {
            assert_eq!(parse_bool(f), Some(false));
        }
        assert_eq!(parse_bool("banana"), None);
    }

    #[test]
    fn click_element_by_passes_deadline_to_native_invoke() {
        let (mut g, _, ctx) = glass_with_a11y_invoke_ctx(
            FakePlatform::new(100, 100),
            fake_tree(),
            InvokeBehavior::Succeed,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        let deadline = Deadline::from_millis(10_000);
        g.click_element_by(AxNodeId(1), deadline).unwrap();
        assert_eq!(ctx.lock().unwrap().as_ref().unwrap().deadline, deadline);
    }

    #[test]
    fn click_element_pointer_fallback_uses_pointer_inner_by() {
        let deadlines = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with_a11y(
            FakePlatform::new(100, 100).with_pointer_deadline_log(deadlines.clone()),
            fake_tree(),
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        let deadline = Deadline::from_millis(10_000);
        g.click_element_by(AxNodeId(1), deadline).unwrap();
        assert_eq!(&*deadlines.lock().unwrap(), &[deadline]);
    }

    #[test]
    fn set_value_by_passes_deadline_to_backend_and_verify_reads() {
        struct ContractBackend {
            tree: AxTree,
            stages: Arc<Mutex<Vec<(&'static str, Deadline)>>>,
        }
        impl Accessibility for ContractBackend {
            fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
                Ok(self.tree.clone())
            }
            fn set_value(
                &mut self,
                ctx: &AxContext,
                _target: &AxTarget,
                _text: &str,
            ) -> Result<()> {
                self.stages.lock().unwrap().push(("dispatch", ctx.deadline));
                // Real backends complete post-write relocation and read-back before returning
                // through this core contract.
                self.stages
                    .lock()
                    .unwrap()
                    .push(("verification_read", ctx.deadline));
                Ok(())
            }
        }
        let stages = Arc::new(Mutex::new(Vec::new()));
        let mut g = glass_with_backend(
            FakePlatform::new(100, 100),
            Box::new(ContractBackend {
                tree: fake_tree(),
                stages: stages.clone(),
            }),
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        let deadline = Deadline::from_millis(10_000);
        g.set_value_by(AxNodeId(1), "updated", deadline).unwrap();
        assert_eq!(
            &*stages.lock().unwrap(),
            &[("dispatch", deadline), ("verification_read", deadline)]
        );
    }

    #[test]
    fn combo_set_value_uses_one_deadline_for_open_wait_select_and_verify() {
        let pointer_deadlines = Arc::new(Mutex::new(Vec::new()));
        let key_deadlines = Arc::new(Mutex::new(Vec::new()));
        let deadline = Deadline::from_millis(10_000);
        let platform = FakePlatform::new(340, 300)
            .with_pointer_deadline_log(pointer_deadlines.clone())
            .with_key_deadline_log(key_deadlines.clone());
        let (mut g, _, ax_deadlines) = glass_with_a11y_seq_deadlines(
            platform,
            vec![
                combo("Acme", &[]),
                combo("Acme", &["Acme", "Globex"]),
                combo("Globex", &[]),
            ],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        ax_deadlines.lock().unwrap().clear();
        g.set_value_by(AxNodeId(1), "Globex", deadline).unwrap();
        assert_eq!(&*pointer_deadlines.lock().unwrap(), &[deadline]);
        assert_eq!(&*key_deadlines.lock().unwrap(), &[deadline, deadline]);
        assert_eq!(&*ax_deadlines.lock().unwrap(), &[deadline, deadline]);

        let short_pointer = Arc::new(Mutex::new(Vec::new()));
        let short_keys = Arc::new(Mutex::new(Vec::new()));
        let short_platform = FakePlatform::new(340, 300)
            .with_pointer_deadline_log(short_pointer.clone())
            .with_key_deadline_log(short_keys.clone());
        let (mut short, _, _) = glass_with_a11y_seq_deadlines(
            short_platform,
            vec![combo("Acme", &[]), combo("Acme", &["Acme", "Globex"])],
            InvokeBehavior::Unsupported,
        );
        short.start(&spec()).unwrap();
        short.a11y_snapshot(None).unwrap();
        let short_deadline = Deadline::from_millis(20);
        let err = short
            .set_value_by(AxNodeId(1), "Globex", short_deadline)
            .unwrap_err();
        assert!(
            matches!(
                err,
                GlassError::Bounded {
                    kind: crate::error::BoundKind::TimedOut,
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(&*short_pointer.lock().unwrap(), &[short_deadline]);
        assert!(short_keys.lock().unwrap().is_empty());
    }

    #[test]
    fn toggle_verify_stops_at_the_nearer_caller_deadline() {
        let drags = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let (mut g, _, ax_deadlines, read_starts) = glass_with_a11y_seq_observed(
            platform,
            vec![sw(false), sw(false)],
            InvokeBehavior::Unsupported,
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        ax_deadlines.lock().unwrap().clear();
        read_starts.lock().unwrap().clear();
        let deadline = Deadline::from_millis(350);
        let err = g.set_value_by(AxNodeId(1), "true", deadline).unwrap_err();
        assert!(
            matches!(
                err,
                GlassError::Bounded {
                    kind: crate::error::BoundKind::TimedOut,
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(drags.lock().unwrap().len(), 1);
        let reads = ax_deadlines.lock().unwrap();
        assert!(!reads.is_empty());
        assert!(reads.iter().all(|seen| *seen == deadline), "{reads:?}");
        let starts = read_starts.lock().unwrap();
        assert!(!starts.is_empty());
        assert!(
            starts
                .iter()
                .all(|(seen, started_live)| *seen == deadline && *started_live),
            "no verification read may start after the effective caller deadline: {starts:?}"
        );
    }

    #[test]
    fn toggle_caller_deadline_requires_a_fresh_snapshot_before_retrying_actuation() {
        let drags = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(400, 400)
            .with_drag_log(drags.clone())
            .with_trailing_toggle_backend();
        let mut g = glass_with_scripted_snapshots(
            platform,
            vec![
                SnapshotReply::Tree(sw(false)),
                SnapshotReply::Tree(sw(false)),
                SnapshotReply::TimedOut("scripted toggle verification read"),
                SnapshotReply::Tree(sw(true)),
            ],
        );
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();

        let err = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(350))
            .unwrap_err();
        assert!(
            matches!(
                err,
                GlassError::Bounded {
                    kind: crate::error::BoundKind::TimedOut,
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(drags.lock().unwrap().len(), 1, "the toggle was actuated");

        let retry = g
            .set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .expect_err("a caller-deadline exit requires a fresh snapshot before retry");
        assert!(matches!(retry, GlassError::NoAxSnapshot), "{retry}");
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the stale unchecked poll must not dispatch a second toggle"
        );

        g.a11y_resnapshot(Deadline::from_millis(2_000)).unwrap();
        g.set_value_by(AxNodeId(1), "true", Deadline::from_millis(2_000))
            .unwrap();
        assert_eq!(
            drags.lock().unwrap().len(),
            1,
            "the fresh checked state makes the retry a truthful no-op"
        );
    }

    #[test]
    fn spent_deadline_dispatches_no_semantic_actuation() {
        let assert_one_failed = |sink: &RecordingSink, action: &str| {
            let expected = vec!["launch:true".to_string(), format!("{action}:false")];
            assert_eq!(&*sink.0.lock().unwrap(), &expected);
        };
        let pointer = Arc::new(Mutex::new(Vec::new()));
        let (mut g, invoke) = glass_with_a11y_invoke(
            FakePlatform::new(100, 100).with_pointer_deadline_log(pointer.clone()),
            fake_tree(),
            InvokeBehavior::Succeed,
        );
        let click_audit = RecordingSink::default();
        g.set_audit_sink(Box::new(click_audit.clone()));
        g.start(&spec()).unwrap();
        g.a11y_snapshot(None).unwrap();
        assert!(
            g.click_element_by(AxNodeId(1), Deadline::from_millis(0))
                .is_err()
        );
        assert!(invoke.lock().unwrap().is_empty());
        assert!(pointer.lock().unwrap().is_empty());
        assert_one_failed(&click_audit, "click_element");

        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut set =
            glass_ready_for_set_value(Box::new(logging_a11y(fake_tree(), writes.clone())));
        let set_audit = RecordingSink::default();
        set.set_audit_sink(Box::new(set_audit.clone()));
        assert!(
            set.set_value_by(AxNodeId(1), "updated", Deadline::from_millis(0))
                .is_err()
        );
        assert!(writes.lock().unwrap().is_empty());
        assert_eq!(set_audit.0.lock().unwrap().as_slice(), &["set_value:false"]);

        let combo_pointer = Arc::new(Mutex::new(Vec::new()));
        let combo_keys = Arc::new(Mutex::new(Vec::new()));
        let mut combo_g = glass_with_a11y(
            FakePlatform::new(340, 300)
                .with_pointer_deadline_log(combo_pointer.clone())
                .with_key_deadline_log(combo_keys.clone()),
            combo("Acme", &[]),
        );
        let combo_audit = RecordingSink::default();
        combo_g.set_audit_sink(Box::new(combo_audit.clone()));
        combo_g.start(&spec()).unwrap();
        combo_g.a11y_snapshot(None).unwrap();
        assert!(
            combo_g
                .set_value_by(AxNodeId(1), "Globex", Deadline::from_millis(0))
                .is_err()
        );
        assert!(combo_pointer.lock().unwrap().is_empty());
        assert!(combo_keys.lock().unwrap().is_empty());
        assert_one_failed(&combo_audit, "set_value");

        let toggle_pointer = Arc::new(Mutex::new(Vec::new()));
        let (mut toggle, toggle_invoke) = glass_with_a11y_invoke(
            FakePlatform::new(400, 400)
                .with_pointer_deadline_log(toggle_pointer.clone())
                .with_trailing_toggle_backend(),
            sw(false),
            InvokeBehavior::Succeed,
        );
        let toggle_audit = RecordingSink::default();
        toggle.set_audit_sink(Box::new(toggle_audit.clone()));
        toggle.start(&spec()).unwrap();
        toggle.a11y_snapshot(None).unwrap();
        assert!(
            toggle
                .set_value_by(AxNodeId(1), "true", Deadline::from_millis(0))
                .is_err()
        );
        assert!(toggle_invoke.lock().unwrap().is_empty());
        assert!(toggle_pointer.lock().unwrap().is_empty());
        assert_one_failed(&toggle_audit, "set_value");

        let (mut delayed_click, invoke_log) = glass_with_a11y_invoke(
            FakePlatform::new(100, 100).with_geometry_delay(Duration::from_millis(20)),
            fake_tree(),
            InvokeBehavior::Succeed,
        );
        let delayed_click_audit = RecordingSink::default();
        delayed_click.set_audit_sink(Box::new(delayed_click_audit.clone()));
        delayed_click.start(&spec()).unwrap();
        delayed_click.a11y_snapshot(None).unwrap();
        assert!(
            delayed_click
                .click_element_by(AxNodeId(1), Deadline::from_millis(5))
                .is_err()
        );
        assert!(invoke_log.lock().unwrap().is_empty());
        assert_one_failed(&delayed_click_audit, "click_element");

        let delayed_writes = Arc::new(Mutex::new(Vec::new()));
        let mut delayed_set = glass_with_backend(
            FakePlatform::new(100, 100).with_geometry_delay(Duration::from_millis(20)),
            Box::new(logging_a11y(fake_tree(), delayed_writes.clone())),
        );
        let delayed_set_audit = RecordingSink::default();
        delayed_set.set_audit_sink(Box::new(delayed_set_audit.clone()));
        delayed_set.start(&spec()).unwrap();
        delayed_set.a11y_snapshot(None).unwrap();
        assert!(
            delayed_set
                .set_value_by(AxNodeId(1), "updated", Deadline::from_millis(5))
                .is_err()
        );
        assert!(delayed_writes.lock().unwrap().is_empty());
        assert_one_failed(&delayed_set_audit, "set_value");
    }
}
