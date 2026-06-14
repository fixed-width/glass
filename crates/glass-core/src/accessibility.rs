//! Platform-agnostic accessibility model + backend seam.
//!
//! Accessibility is a **per-OS** concern, distinct from the per-display-server
//! [`crate::platform::Platform`] seam: AT-SPI serves both X11 and Wayland, and
//! macOS/Windows each expose exactly one accessibility API. Backends (e.g.
//! `glass-a11y-linux`) map their native roles/states into the normalized types
//! here; no OS/AT-SPI/D-Bus types appear in this module.

use std::fmt::Write as _;

use crate::error::Result;
use crate::platform::WindowGeometry;

/// Normalized accessibility role — the union of the AT-SPI / AX / UIA
/// vocabularies. A backend maps its native role in; anything unmapped becomes
/// [`AxRole::Other`] with the native string preserved in [`AxNode::raw_role`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxRole {
    Application,
    Window,
    Dialog,
    Group,
    Button,
    ToggleButton,
    RadioButton,
    CheckBox,
    MenuBar,
    Menu,
    MenuItem,
    Label,
    TextField,
    TextArea,
    ComboBox,
    List,
    ListItem,
    Table,
    Cell,
    Tree,
    TreeItem,
    TabList,
    Tab,
    ScrollBar,
    Slider,
    SpinButton,
    ProgressBar,
    Image,
    Link,
    Separator,
    Toolbar,
    StatusBar,
    Heading,
    Other,
}

impl AxRole {
    /// Whether this role denotes an element a user acts on (clicks / types into) —
    /// the elements worth a Set-of-Mark number. Containers, the window, and static
    /// text return `false`.
    pub fn is_interactable(self) -> bool {
        matches!(
            self,
            AxRole::Button
                | AxRole::ToggleButton
                | AxRole::RadioButton
                | AxRole::CheckBox
                | AxRole::MenuItem
                | AxRole::Tab
                | AxRole::Link
                | AxRole::TextField
                | AxRole::TextArea
                | AxRole::ComboBox
                | AxRole::Slider
                | AxRole::SpinButton
                | AxRole::ListItem
                | AxRole::TreeItem
                | AxRole::Cell
        )
    }

    /// Parse a role from its name (case-insensitive), e.g. `"button"`,
    /// `"ProgressBar"`. `None` for an unknown name.
    pub fn from_name(s: &str) -> Option<AxRole> {
        use AxRole::*;
        Some(match s.to_ascii_lowercase().as_str() {
            "application" => Application,
            "window" => Window,
            "dialog" => Dialog,
            "group" => Group,
            "button" => Button,
            "togglebutton" => ToggleButton,
            "radiobutton" => RadioButton,
            "checkbox" => CheckBox,
            "menubar" => MenuBar,
            "menu" => Menu,
            "menuitem" => MenuItem,
            "label" => Label,
            "textfield" => TextField,
            "textarea" => TextArea,
            "combobox" => ComboBox,
            "list" => List,
            "listitem" => ListItem,
            "table" => Table,
            "cell" => Cell,
            "tree" => Tree,
            "treeitem" => TreeItem,
            "tablist" => TabList,
            "tab" => Tab,
            "scrollbar" => ScrollBar,
            "slider" => Slider,
            "spinbutton" => SpinButton,
            "progressbar" => ProgressBar,
            "image" => Image,
            "link" => Link,
            "separator" => Separator,
            "toolbar" => Toolbar,
            "statusbar" => StatusBar,
            "heading" => Heading,
            "other" => Other,
            _ => return None,
        })
    }
}

/// Normalized state flags — the subset all three OS vocabularies expose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxStates {
    pub focused: bool,
    pub focusable: bool,
    pub enabled: bool,
    pub visible: bool,
    pub selected: bool,
    pub checked: bool,
    pub expanded: bool,
    pub editable: bool,
}

impl AxStates {
    /// Names of the set states, in a stable order, for the text outline.
    pub fn active(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.focused {
            v.push("focused");
        }
        if self.focusable {
            v.push("focusable");
        }
        if self.enabled {
            v.push("enabled");
        }
        if self.visible {
            v.push("visible");
        }
        if self.selected {
            v.push("selected");
        }
        if self.checked {
            v.push("checked");
        }
        if self.expanded {
            v.push("expanded");
        }
        if self.editable {
            v.push("editable");
        }
        v
    }
}

/// Window-relative bounds (0,0 = window top-left). `i32` origin: an element may
/// extend past / be partially off the window. Distinct from the capture
/// [`crate::frame::Region`], which must fit inside the window for cropping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl AxRect {
    /// Center point, clamped into `[0,win_w) × [0,win_h)`. Returns `None` if the
    /// rect or the window has zero area (nothing clickable).
    pub fn clamped_center(&self, win_w: u32, win_h: u32) -> Option<(i32, i32)> {
        if self.width == 0 || self.height == 0 || win_w == 0 || win_h == 0 {
            return None;
        }
        let cx = (self.x + self.width as i32 / 2).clamp(0, win_w as i32 - 1);
        let cy = (self.y + self.height as i32 / 2).clamp(0, win_h as i32 - 1);
        Some((cx, cy))
    }
}

/// A synthetic node id, assigned by `glass-core` (not the backend) in pre-order
/// DFS so numbering is deterministic and identical across OS backends. Stable
/// only within one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxNodeId(pub u32);

/// One accessibility element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxNode {
    /// Assigned by [`AxTree::assign_ids`]; backends may leave it as `AxNodeId(0)`.
    pub id: AxNodeId,
    pub role: AxRole,
    /// The backend's native role string — the escape hatch for unmapped roles.
    pub raw_role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub states: AxStates,
    pub bounds: Option<AxRect>,
    pub children: Vec<AxNode>,
}

/// The active window's accessibility subtree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxTree {
    pub root: AxNode,
    /// Total node count; set by [`AxTree::assign_ids`].
    pub count: usize,
}

impl AxTree {
    /// Number nodes in pre-order DFS (`root = 0`) and set `count`. Backends leave
    /// ids unset; the core assigns them so numbering is identical across OSes.
    pub fn assign_ids(&mut self) {
        fn walk(node: &mut AxNode, next: &mut u32) {
            node.id = AxNodeId(*next);
            *next += 1;
            for child in &mut node.children {
                walk(child, next);
            }
        }
        let mut next = 0;
        walk(&mut self.root, &mut next);
        self.count = next as usize;
    }

    /// Find a node by id (pre-order). Call after [`AxTree::assign_ids`].
    pub fn find(&self, id: AxNodeId) -> Option<&AxNode> {
        fn walk(node: &AxNode, id: AxNodeId) -> Option<&AxNode> {
            if node.id == id {
                return Some(node);
            }
            node.children.iter().find_map(|c| walk(c, id))
        }
        walk(&self.root, id)
    }

    /// Render a compact indented outline, one line per node:
    /// `#<id> <Role> "<name>" (<x>,<y> <w>x<h>) [<states>]` — name/bounds/states
    /// elided when absent. Two spaces of indent per depth level.
    pub fn to_outline(&self) -> String {
        fn walk(node: &AxNode, depth: usize, out: &mut String) {
            let indent = "  ".repeat(depth);
            let _ = write!(out, "{indent}#{} {:?}", node.id.0, node.role);
            if let Some(name) = &node.name {
                let _ = write!(out, " {name:?}");
            }
            if let Some(b) = &node.bounds {
                let _ = write!(out, " ({},{} {}x{})", b.x, b.y, b.width, b.height);
            }
            let states = node.states.active();
            if !states.is_empty() {
                let _ = write!(out, " [{}]", states.join(","));
            }
            out.push('\n');
            for child in &node.children {
                walk(child, depth + 1, out);
            }
        }
        let mut out = String::new();
        walk(&self.root, 0, &mut out);
        out
    }
}

/// Context the display backend supplies so the a11y reader can locate the right
/// app/window and validate coordinates. `window` is in screen coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxContext {
    /// The launched app's process ids (root + descendants the backend can enumerate). The
    /// reader accepts a window whose owning pid is in this set; an **empty** set means "no
    /// pid hint — correlate by geometry/title instead". Multi-element only when the display
    /// backend has a process-tree view (Windows' Job set); 1-element on X11/Wayland.
    pub pids: Vec<u32>,
    pub window: WindowGeometry,
    /// Raw native handle of glass's active (adopted) window — a Windows `HWND` as `i64`. `Some`
    /// whenever the backend tracks one; the Windows reader binds UI Automation directly to it (no
    /// desktop re-discovery), so a11y reads the *exact* window glass is driving. `None` on backends
    /// that address accessibility another way (Linux uses `a11y_bus_addr`); those ignore this field.
    pub window_handle: Option<i64>,
    /// Address of the private a11y bus glass spawned for this launch, if any. `Some` only when the
    /// caller passed `a11y: true` and the bus started. When `None`, the Linux reader returns
    /// `AccessibilityUnavailable` (instructing the caller to relaunch with `a11y:true`) — it does
    /// NOT fall back to any host/ambient bus. Non-Linux backends ignore this field.
    pub a11y_bus_addr: Option<String>,
}

/// A fingerprint identifying the element a value-set targets: its synthetic id
/// (pre-order index), the role/name the caller saw in the snapshot, and the
/// element's window-relative bounds when known. The backend re-walks to the id
/// and verifies role+name (and bounds, when present) so a stale id — or tree
/// drift that lands a *different* same-role+name element on the id — errors
/// rather than overwriting the wrong element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxTarget {
    pub id: AxNodeId,
    pub role: AxRole,
    pub name: Option<String>,
    /// The element's window-relative bounds at snapshot time, when known. An
    /// extra fingerprint: re-walking to a pre-order id can land on a different
    /// same-role+name element if the tree drifted, and that element sits
    /// elsewhere — see [`Self::bounds_consistent`].
    pub bounds: Option<AxRect>,
}

impl AxTarget {
    /// Whether a reached node's role + name match this target.
    pub fn matches(&self, role: AxRole, name: Option<&str>) -> bool {
        self.role == role && self.name.as_deref() == name
    }

    /// Whether a reached element's bounds `got` are consistent with the bounds
    /// captured for this target, within `tol` px on every edge. `true` when no
    /// bounds were captured (nothing to verify — role+name still gate). A
    /// genuinely different element that drift moved onto this id sits elsewhere
    /// and is rejected; sub-pixel / DWM-border jitter is tolerated.
    pub fn bounds_consistent(&self, got: Option<AxRect>, tol: i64) -> bool {
        match (self.bounds, got) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(a), Some(b)) => {
                (i64::from(a.x) - i64::from(b.x)).abs() <= tol
                    && (i64::from(a.y) - i64::from(b.y)).abs() <= tol
                    && (i64::from(a.width) - i64::from(b.width)).abs() <= tol
                    && (i64::from(a.height) - i64::from(b.height)).abs() <= tol
            }
        }
    }
}

/// The OS accessibility seam — one impl per OS. Object-safe; the session stores
/// it boxed as `Send` (the `Send` bound lives at the storage site, not on the
/// trait). Distinct from `Platform`: accessibility varies per-OS, not per-
/// display-server.
pub trait Accessibility {
    /// Snapshot the active window's accessibility subtree, normalized and in
    /// window-relative coordinates. Node ids are assigned by the caller
    /// afterward via [`AxTree::assign_ids`]; the backend need not set them.
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree>;

    /// Set the editable element identified by `target` to `text`. The backend
    /// re-walks pre-order to `target.id`, verifies role+name, then sets via the
    /// native editable interface. Default: unsupported.
    fn set_value(&mut self, _ctx: &AxContext, _target: &AxTarget, _text: &str) -> Result<()> {
        Err(crate::error::GlassError::AxUnsupported)
    }
}

/// A precise wait condition over an accessibility element. State variants assert
/// the matched node carries (or lacks) one of the [`AxStates`] flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementCondition {
    /// A node matching the selector exists.
    Appears,
    /// No node matches the selector.
    Disappears,
    Enabled,
    Disabled,
    Checked,
    Unchecked,
    Selected,
    Unselected,
    Expanded,
    Collapsed,
    Focused,
    Visible,
    Hidden,
}

impl ElementCondition {
    /// Parse from the condition name (case-insensitive). `None` for unknown.
    pub fn from_name(s: &str) -> Option<ElementCondition> {
        use ElementCondition::*;
        Some(match s.to_ascii_lowercase().as_str() {
            "appears" => Appears,
            "disappears" => Disappears,
            "enabled" => Enabled,
            "disabled" => Disabled,
            "checked" => Checked,
            "unchecked" => Unchecked,
            "selected" => Selected,
            "unselected" => Unselected,
            "expanded" => Expanded,
            "collapsed" => Collapsed,
            "focused" => Focused,
            "visible" => Visible,
            "hidden" => Hidden,
            _ => return None,
        })
    }

    /// The state predicate a matched node must satisfy. `Appears` accepts any
    /// node; `Disappears` is handled separately (existence, not state).
    fn state_pred(self) -> fn(&AxStates) -> bool {
        use ElementCondition::*;
        match self {
            Appears | Disappears => |_| true,
            Enabled => |s| s.enabled,
            Disabled => |s| !s.enabled,
            Checked => |s| s.checked,
            Unchecked => |s| !s.checked,
            Selected => |s| s.selected,
            Unselected => |s| !s.selected,
            Expanded => |s| s.expanded,
            Collapsed => |s| !s.expanded,
            Focused => |s| s.focused,
            Visible => |s| s.visible,
            Hidden => |s| !s.visible,
        }
    }
}

/// Result of evaluating an [`ElementCondition`] against a tree.
#[derive(Debug)]
pub enum ElementMatch<'a> {
    /// Condition satisfied. Carries the matched node for positive conditions;
    /// `None` for `Disappears` (there is no node to return).
    Satisfied(Option<&'a AxNode>),
    /// Not satisfied yet.
    Pending,
}

/// Owned snapshot of a matched element (decoupled from the borrowed tree), for
/// returning across the poll loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementInfo {
    pub id: AxNodeId,
    pub role: AxRole,
    pub name: Option<String>,
    pub value: Option<String>,
    pub bounds: Option<AxRect>,
    pub states: AxStates,
}

impl ElementInfo {
    /// Snapshot an [`AxNode`] into an owned [`ElementInfo`], decoupled from the tree's lifetime.
    pub fn from_node(n: &AxNode) -> ElementInfo {
        ElementInfo {
            id: n.id,
            role: n.role,
            name: n.name.clone(),
            value: n.value.clone(),
            bounds: n.bounds,
            states: n.states,
        }
    }
}

/// Find the first node (pre-order DFS) satisfying `pred`.
fn find_preorder<'a>(node: &'a AxNode, pred: &dyn Fn(&AxNode) -> bool) -> Option<&'a AxNode> {
    if pred(node) {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_preorder(c, pred))
}

/// Evaluate a precise element condition against `tree`. The selector is the
/// conjunction of: `name` substring of the node's name, `role` equality, and
/// `value_contains` substring of the node's value (each optional). For positive
/// conditions, returns the first node matching selector + state; for
/// `Disappears`, satisfied iff no node matches the selector.
///
/// Note: a `name` or `value_contains` filter only matches nodes whose `name`/`value`
/// field is `Some` — a node with `name: None` never matches a name query. Pass
/// `name: None` to skip the name filter entirely.
pub fn element_match<'a>(
    tree: &'a AxTree,
    name: Option<&str>,
    role: Option<AxRole>,
    value_contains: Option<&str>,
    condition: ElementCondition,
) -> ElementMatch<'a> {
    let selector_match = |n: &AxNode| -> bool {
        name.is_none_or(|q| n.name.as_deref().is_some_and(|nm| nm.contains(q)))
            && role.is_none_or(|r| n.role == r)
            && value_contains.is_none_or(|v| n.value.as_deref().is_some_and(|val| val.contains(v)))
    };
    if condition == ElementCondition::Disappears {
        return if find_preorder(&tree.root, &selector_match).is_none() {
            ElementMatch::Satisfied(None)
        } else {
            ElementMatch::Pending
        };
    }
    let pred = condition.state_pred();
    match find_preorder(&tree.root, &|n| selector_match(n) && pred(&n.states)) {
        Some(n) => ElementMatch::Satisfied(Some(n)),
        None => ElementMatch::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_is_object_safe() {
        fn _accepts(_a: &mut dyn Accessibility) {}
    }

    #[test]
    fn ax_target_matches_on_role_and_name() {
        let t = AxTarget { id: AxNodeId(3), role: AxRole::TextField, name: Some("Email".into()), bounds: None };
        assert!(t.matches(AxRole::TextField, Some("Email")));
        assert!(!t.matches(AxRole::Button, Some("Email")), "role must match");
        assert!(!t.matches(AxRole::TextField, Some("Name")), "name must match");
        assert!(!t.matches(AxRole::TextField, None), "missing name must not match a named target");

        let t_unnamed = AxTarget { id: AxNodeId(5), role: AxRole::TextField, name: None, bounds: None };
        assert!(t_unnamed.matches(AxRole::TextField, None), "unnamed target matches unnamed live node");
        assert!(!t_unnamed.matches(AxRole::TextField, Some("X")), "unnamed target must not match a named live node");
    }

    #[test]
    fn ax_target_bounds_consistent_rejects_a_moved_element() {
        let r = AxRect { x: 100, y: 50, width: 80, height: 20 };
        let t = AxTarget { id: AxNodeId(3), role: AxRole::TextField, name: None, bounds: Some(r) };
        // Exact and within-tolerance bounds pass.
        assert!(t.bounds_consistent(Some(r), 8));
        assert!(
            t.bounds_consistent(Some(AxRect { x: 104, y: 53, width: 80, height: 20 }), 8),
            "minor jitter within tolerance is accepted"
        );
        // A different element that drift landed on this id sits elsewhere → rejected.
        assert!(!t.bounds_consistent(Some(AxRect { x: 300, y: 400, width: 120, height: 30 }), 8));
        // Expected a positioned element but the reached one has none → reject.
        assert!(!t.bounds_consistent(None, 8));
        // No fingerprint captured → nothing to verify, accept (role+name still gates).
        let t_nofp = AxTarget { id: AxNodeId(3), role: AxRole::TextField, name: None, bounds: None };
        assert!(t_nofp.bounds_consistent(Some(r), 8));
        assert!(t_nofp.bounds_consistent(None, 8));
    }

    #[test]
    fn clamped_center_is_in_bounds() {
        let r = AxRect { x: 10, y: 20, width: 40, height: 10 };
        assert_eq!(r.clamped_center(100, 100), Some((30, 25)));
    }

    #[test]
    fn clamped_center_clamps_to_window() {
        let r = AxRect { x: 90, y: 90, width: 40, height: 40 };
        // center would be (110,110); clamps to (63,47) for a 64x48 window.
        assert_eq!(r.clamped_center(64, 48), Some((63, 47)));
    }

    #[test]
    fn clamped_center_rejects_zero_area() {
        assert_eq!(AxRect { x: 0, y: 0, width: 0, height: 5 }.clamped_center(10, 10), None);
        assert_eq!(AxRect { x: 0, y: 0, width: 5, height: 5 }.clamped_center(0, 10), None);
    }

    #[test]
    fn active_states_listed_in_order() {
        let s = AxStates { focusable: true, enabled: true, checked: true, ..Default::default() };
        assert_eq!(s.active(), vec!["focusable", "enabled", "checked"]);
    }

    /// A leaf node with the given role + name, no bounds, ids unset.
    fn leaf(role: AxRole, name: &str) -> AxNode {
        AxNode {
            id: AxNodeId(0),
            role,
            raw_role: format!("{role:?}").to_lowercase(),
            name: Some(name.into()),
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        }
    }

    fn sample_tree() -> AxTree {
        let mut button = leaf(AxRole::Button, "Save");
        button.bounds = Some(AxRect { x: 12, y: 40, width: 80, height: 24 });
        button.states = AxStates { focusable: true, enabled: true, ..Default::default() };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Settings".into()),
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect { x: 0, y: 0, width: 640, height: 480 }),
            children: vec![button, leaf(AxRole::Label, "Ready")],
        };
        AxTree { root, count: 0 }
    }

    #[test]
    fn assign_ids_numbers_preorder_and_counts() {
        let mut t = sample_tree();
        t.assign_ids();
        assert_eq!(t.count, 3);
        assert_eq!(t.root.id, AxNodeId(0));
        assert_eq!(t.root.children[0].id, AxNodeId(1));
        assert_eq!(t.root.children[1].id, AxNodeId(2));
    }

    #[test]
    fn find_returns_node_by_id() {
        let mut t = sample_tree();
        t.assign_ids();
        assert_eq!(t.find(AxNodeId(1)).unwrap().name.as_deref(), Some("Save"));
        assert!(t.find(AxNodeId(99)).is_none());
    }

    #[test]
    fn outline_is_compact_indented_text() {
        let mut t = sample_tree();
        t.assign_ids();
        let out = t.to_outline();
        assert_eq!(
            out,
            "#0 Window \"Settings\" (0,0 640x480)\n  \
             #1 Button \"Save\" (12,40 80x24) [focusable,enabled]\n  \
             #2 Label \"Ready\"\n"
        );
    }

    #[test]
    fn interactable_roles_are_classified() {
        for r in [
            AxRole::Button,
            AxRole::ToggleButton,
            AxRole::RadioButton,
            AxRole::CheckBox,
            AxRole::MenuItem,
            AxRole::Tab,
            AxRole::Link,
            AxRole::TextField,
            AxRole::TextArea,
            AxRole::ComboBox,
            AxRole::Slider,
            AxRole::SpinButton,
            AxRole::ListItem,
            AxRole::TreeItem,
            AxRole::Cell,
        ] {
            assert!(r.is_interactable(), "{r:?} should be interactable");
        }
        for r in [AxRole::Window, AxRole::Group, AxRole::Label, AxRole::Image, AxRole::Other] {
            assert!(!r.is_interactable(), "{r:?} should not be interactable");
        }
    }

    #[test]
    fn role_from_name_is_case_insensitive() {
        assert_eq!(AxRole::from_name("button"), Some(AxRole::Button));
        assert_eq!(AxRole::from_name("ProgressBar"), Some(AxRole::ProgressBar));
        assert_eq!(AxRole::from_name("CHECKBOX"), Some(AxRole::CheckBox));
        assert_eq!(AxRole::from_name("nonsense"), None);
    }

    #[test]
    fn condition_from_name_maps_known_and_rejects_unknown() {
        assert_eq!(ElementCondition::from_name("appears"), Some(ElementCondition::Appears));
        assert_eq!(ElementCondition::from_name("disappears"), Some(ElementCondition::Disappears));
        assert_eq!(ElementCondition::from_name("enabled"), Some(ElementCondition::Enabled));
        assert_eq!(ElementCondition::from_name("hidden"), Some(ElementCondition::Hidden));
        assert_eq!(ElementCondition::from_name("wat"), None);
        // case-insensitive
        assert_eq!(ElementCondition::from_name("Enabled"), Some(ElementCondition::Enabled));
        assert_eq!(ElementCondition::from_name("DISAPPEARS"), Some(ElementCondition::Disappears));
    }

    #[test]
    fn element_match_appears_finds_first_by_name_substring() {
        let mut t = sample_tree();
        t.assign_ids();
        match element_match(&t, Some("Sav"), None, None, ElementCondition::Appears) {
            ElementMatch::Satisfied(Some(n)) => assert_eq!(n.id, AxNodeId(1)),
            other => panic!("expected Satisfied(Save), got {other:?}"),
        }
    }

    #[test]
    fn element_match_role_filters() {
        let mut t = sample_tree();
        t.assign_ids();
        // A Label also exists; require role=Button so only "Save" qualifies.
        match element_match(&t, None, Some(AxRole::Button), None, ElementCondition::Appears) {
            ElementMatch::Satisfied(Some(n)) => assert_eq!(n.name.as_deref(), Some("Save")),
            other => panic!("expected the Button, got {other:?}"),
        }
    }

    #[test]
    fn element_match_state_condition_requires_the_state() {
        let mut t = sample_tree();
        t.assign_ids();
        // Save is enabled -> Enabled satisfied; it is not checked -> Checked pending.
        assert!(matches!(
            element_match(&t, Some("Save"), None, None, ElementCondition::Enabled),
            ElementMatch::Satisfied(Some(_))
        ));
        assert!(matches!(
            element_match(&t, Some("Save"), None, None, ElementCondition::Checked),
            ElementMatch::Pending
        ));
        // Negative form: Save is enabled, so Disabled is pending.
        assert!(matches!(
            element_match(&t, Some("Save"), None, None, ElementCondition::Disabled),
            ElementMatch::Pending
        ));
    }

    #[test]
    fn element_match_disappears_is_satisfied_when_absent() {
        let mut t = sample_tree();
        t.assign_ids();
        assert!(matches!(
            element_match(&t, Some("Ghost"), None, None, ElementCondition::Disappears),
            ElementMatch::Satisfied(None)
        ));
        assert!(matches!(
            element_match(&t, Some("Save"), None, None, ElementCondition::Disappears),
            ElementMatch::Pending
        ));
    }

    #[test]
    fn element_match_value_contains_filters() {
        let mut t = sample_tree();
        t.assign_ids();
        // Give the Label a value and match on it.
        t.root.children[1].value = Some("Loading 50%".into());
        match element_match(&t, None, Some(AxRole::Label), Some("50%"), ElementCondition::Appears) {
            ElementMatch::Satisfied(Some(n)) => assert_eq!(n.name.as_deref(), Some("Ready")),
            other => panic!("expected the Label by value, got {other:?}"),
        }
        assert!(matches!(
            element_match(&t, None, Some(AxRole::Label), Some("99%"), ElementCondition::Appears),
            ElementMatch::Pending
        ));
    }
}
