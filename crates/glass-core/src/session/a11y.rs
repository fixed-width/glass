//! `Glass` accessibility ops: snapshot, marks, click, and set-value.
use super::*;

impl Glass {
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
        let (bounds, active_geo) = {
            let s = self.require_active()?;
            let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
            let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
            let bounds = node.bounds.ok_or(GlassError::AxElementNotClickable(id.0))?;
            (bounds, s.geometry.clone())
        };
        // The element's a11y bounds are reported relative to the active window, but it
        // may actually render in a separate popover window (e.g. an open dropdown's
        // option list) whose own origin they don't reflect. Detect that and route the
        // click into the popover instead of silently missing.
        //
        // This enumeration is a best-effort popover probe, not something an ordinary
        // click depends on: a backend where `list_windows` is heavier or flaky must
        // never turn a normal click into a failure just because the probe failed. An
        // `Err` here degrades to an empty list, which makes `owning_popover` return
        // `None` below and falls straight through to the unchanged `clamped_center`
        // click path.
        let windows = self.list_windows().unwrap_or_default();
        if let Some(popover_id) = owning_popover(bounds, &active_geo, &windows) {
            let popover_geo = windows
                .iter()
                .find(|w| w.id == popover_id)
                .map(|w| w.geometry.clone())
                .ok_or(GlassError::WindowNotFound)?;
            let container = {
                let s = self.require_active()?;
                let tree = s.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
                menu_container_bounds(&tree.root, id, &popover_geo)
            }
            .ok_or(GlassError::AxElementInUnmappedPopover(id.0))?;
            let prev = windows.iter().find(|w| w.active).map(|w| w.id);
            self.select_window(popover_id)?;
            let result = self.pointer_inner(&PointerEvent::Click {
                x: bounds.x - container.x,
                y: bounds.y - container.y,
                button: MouseButton::Left,
                count: 1,
                modifiers: vec![],
            });
            // Best-effort restore: the click's own result (ok or err) still wins.
            if let Some(prev) = prev {
                let _ = self.select_window(prev);
            }
            return result;
        }
        let (x, y) = bounds
            .clamped_center(active_geo.width, active_geo.height)
            .ok_or(GlassError::AxElementNotClickable(id.0))?;
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
}
