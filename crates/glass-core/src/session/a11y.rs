//! `Glass` accessibility ops: snapshot, marks, click, and set-value.
use super::*;

/// A checkable element wider than this multiple of its height is treated as "row-shaped". On a
/// backend that frames a switch as its whole row (`Platform::a11y_toggle_control_at_trailing_edge`),
/// `click_element` aims a row-shaped checkable's tap at the trailing control instead of the row
/// center.
const ROW_ASPECT: u32 = 4;

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

    /// Click the element with id `id` from the most recent `a11y_snapshot` via the normal
    /// pointer path — the center of its bounds, or the trailing control for a row-shaped
    /// checkable (see [`AxRect::clamped_trailing_point`]).
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
        // A switch whose backend reports the whole row as its frame with the control at the
        // trailing edge (iOS/idb) is mis-tapped at the geometric center — that lands on the inert
        // label. For such a backend, aim a row-shaped checkable's tap at the trailing control.
        // Gated on the backend capability, NOT geometry alone: a wide *labeled* checkbox on a
        // desktop backend is also row-shaped but has its indicator at the LEADING edge, so the
        // trailing-aim must not apply there. The row-shape test uses the raw-bounds aspect as a
        // cheap pre-filter; `clamped_trailing_point` derives its inset from the clamped visible
        // height.
        let (x, y) = if checkable
            && trailing_toggle_backend
            && bounds.width > bounds.height.saturating_mul(ROW_ASPECT)
        {
            bounds.clamped_trailing_point(active_geo.width, active_geo.height)
        } else {
            bounds.clamped_center(active_geo.width, active_geo.height)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::*;

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
    fn click_element_taps_trailing_edge_for_a_row_shaped_checkable() {
        // A checkable node whose bounds are row-shaped (w > 4h) — a backend (iOS/idb) that
        // reports the whole cell as a switch's frame. The click must land near the trailing
        // control (right of center); a non-checkable node of the SAME bounds still clicks center.
        let clicks = Arc::new(Mutex::new(Vec::new()));
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
            .with_trailing_toggle_backend();
        let mut g = glass_with_a11y(platform, AxTree { root, count: 0 });
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();

        // node #1 = the row-shaped checkable → trailing point (right of center).
        g.click_element(AxNodeId(1)).unwrap();
        let trailing = bounds.clamped_trailing_point(100, 100).unwrap();
        assert_eq!(clicks.lock().unwrap().last().copied(), Some(trailing));
        assert!(trailing.0 > bounds.clamped_center(100, 100).unwrap().0);

        // node #2 = the non-checkable row of identical bounds → geometric center (gate needs
        // checkable, so a plain wide list row is unaffected).
        g.click_element(AxNodeId(2)).unwrap();
        assert_eq!(
            clicks.lock().unwrap().last().copied(),
            bounds.clamped_center(100, 100)
        );
    }

    #[test]
    fn click_element_uses_center_for_a_row_shaped_checkable_on_a_non_trailing_backend() {
        // The trailing-aim is opt-in per backend. A desktop backend (default FakePlatform: no
        // `with_trailing_toggle_backend`) frames a labeled checkbox as a wide row too, but its
        // indicator is at the LEADING edge — so a row-shaped checkable here must still click
        // center, never trailing. This is the guard that keeps the iOS fix from misfiring
        // on macOS/Windows/Linux/Android.
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
        let mut g = glass_with_a11y(platform, AxTree { root, count: 0 });
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
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
        let mut g = glass_with_a11y(platform, AxTree { root, count: 0 });
        g.start(&spec()).unwrap();
        g.a11y_snapshot().unwrap();
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
}
