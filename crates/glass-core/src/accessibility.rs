//! Platform-agnostic accessibility model + backend seam.
//!
//! Accessibility is a **per-OS** concern, distinct from the per-display-server
//! [`crate::platform::Platform`] seam: AT-SPI serves both X11 and Wayland, and
//! macOS/Windows each expose exactly one accessibility API. Backends (e.g.
//! `glass-a11y-linux`) map their native roles/states into the normalized types
//! here; no OS/AT-SPI/D-Bus types appear in this module.

use std::fmt::Write as _;

use crate::error::Result;
use crate::platform::{Segment, WindowGeometry};

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
    /// The element exposes a real toggle state (`checked` is only meaningful when this is true).
    pub checkable: bool,
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
        if self.checkable {
            v.push(if self.checked { "checked" } else { "unchecked" });
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
    /// The element's visible intersection with `[0,win_w) × [0,win_h)`, as
    /// `(left, top, right, bottom)`. `None` when the rect or window has zero area, or the element
    /// has no visible overlap with the window (fully clipped off-screen). Every actuation point
    /// below derives from this one clip so their intersection semantics can't drift: a
    /// partially-clipped element is still acted on within its own visible portion, and a
    /// fully-clipped one returns `None` — surfaced as a not-clickable error rather than a silent
    /// click on the window edge that never lands on the element (the "no silent fallbacks"
    /// invariant).
    fn visible_intersection(&self, win_w: u32, win_h: u32) -> Option<(i32, i32, i32, i32)> {
        if self.width == 0 || self.height == 0 || win_w == 0 || win_h == 0 {
            return None;
        }
        let left = self.x.max(0);
        let top = self.y.max(0);
        let right = (self.x + self.width as i32).min(win_w as i32);
        let bottom = (self.y + self.height as i32).min(win_h as i32);
        (right > left && bottom > top).then_some((left, top, right, bottom))
    }

    /// The click point for this element: the center of its visible intersection with the window,
    /// always inside `[0,win_w) × [0,win_h)`. `None` when there is nothing to click (see
    /// [`Self::visible_intersection`]).
    pub fn clamped_center(&self, win_w: u32, win_h: u32) -> Option<(i32, i32)> {
        let (left, top, right, bottom) = self.visible_intersection(win_w, win_h)?;
        Some(((left + right) / 2, (top + bottom) / 2))
    }

    /// Actuation point for a **row-shaped checkable** element. A backend (iOS/idb) can report
    /// a table-cell switch's frame as the whole row, whose control sits at the trailing edge;
    /// the geometric [`Self::clamped_center`] then lands on the label and a tap no-ops. This
    /// aims near the trailing control: `x = right_edge - inset`, floored at the horizontal
    /// center so it never crosses back past the middle; `y` = vertical center. The inset is the
    /// visible height rather than a fixed pixel amount, so it scales with the control at any
    /// device scale (a switch's width ≈ its row height). Shares [`Self::visible_intersection`]
    /// with `clamped_center`, so the clip / zero-area / fully-offscreen `None` are identical.
    pub fn clamped_trailing_point(&self, win_w: u32, win_h: u32) -> Option<(i32, i32)> {
        let (left, top, right, bottom) = self.visible_intersection(win_w, win_h)?;
        let center_x = (left + right) / 2;
        let inset = bottom - top; // visible height; the control is ~this far from the edge
        let x = (right - inset).max(center_x);
        Some((x, (top + bottom) / 2))
    }

    /// Endpoints of a short horizontal swipe centered on the trailing control of a row-shaped
    /// element — the gesture that toggles a control (e.g. an iOS `UISwitch`) which does NOT actuate
    /// on a tap. Anchored at the same trailing point as [`Self::clamped_trailing_point`]; the span is
    /// ~1.5×the control height (`inset`), matching the proven idb swipe. `None` for an off-screen rect,
    /// exactly like [`Self::clamped_center`]. For a genuinely row-shaped input — the shape the
    /// caller gates on (see `ROW_ASPECT` in `session::a11y`) — the segment lies entirely in the
    /// trailing (right) region, clear of the left-edge back-swipe zone; that is an emergent
    /// property of row-shaped bounds, not a guarantee this method makes for arbitrary input.
    ///
    /// Always left-to-right, never direction-aware — deliberately: on-device testing showed three
    /// IDENTICAL left-to-right swipes alternate a `UISwitch` unchecked -> checked -> unchecked ->
    /// checked. A short swipe here registers as a TOGGLE gesture, not a directional drag-to-value,
    /// so there is no "swipe right to turn on" physics to encode — direction is irrelevant to the
    /// outcome. Do not "fix" this into direction-dependent logic.
    pub fn trailing_toggle_swipe(&self, win_w: u32, win_h: u32) -> Option<Segment> {
        let (left, top, right, bottom) = self.visible_intersection(win_w, win_h)?;
        let (anchor_x, anchor_y) = self.clamped_trailing_point(win_w, win_h)?;
        let inset = bottom - top; // control ~this far from the trailing edge and ~this tall
        let half = (inset * 3 / 4).max(1); // span 1.5*inset; matches the proven ~951->1077 px swipe on inset 84; floor of 1 keeps a thin control's swipe non-zero-length
        let from_x = (anchor_x - half).max(left);
        let to_x = (anchor_x + half).min(right);
        Some(Segment {
            from_x,
            from_y: anchor_y,
            to_x,
            to_y: anchor_y,
        })
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

/// Bounds on how much of an app's accessibility tree a backend walks. Shared by every OS
/// backend so a tree's size limits never depend on which platform produced it.
///
/// `MAX_NODES` bounds the whole tree; `MAX_DEPTH` bounds nesting; `MAX_SIBLINGS` bounds the
/// per-level scan, because `MAX_NODES` only counts nodes actually *kept* — a level with a
/// pathological number of skipped siblings would otherwise iterate without ever tripping it.
pub const MAX_NODES: usize = 1500;
/// See [`MAX_NODES`].
pub const MAX_DEPTH: usize = 30;
/// See [`MAX_NODES`].
pub const MAX_SIBLINGS: usize = 4096;

/// Which bound stopped a walk early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationLimit {
    Nodes,
    Depth,
    Siblings,
}

impl TruncationLimit {
    /// The limit's numeric value, for the disclosure notice.
    pub fn value(self) -> usize {
        match self {
            TruncationLimit::Nodes => MAX_NODES,
            TruncationLimit::Depth => MAX_DEPTH,
            TruncationLimit::Siblings => MAX_SIBLINGS,
        }
    }

    /// Human-readable unit for the disclosure notice.
    fn label(self) -> &'static str {
        match self {
            TruncationLimit::Nodes => "nodes",
            TruncationLimit::Depth => "levels deep",
            TruncationLimit::Siblings => "siblings per level",
        }
    }
}

/// Record of a walk that stopped early. Its presence on an [`AxTree`] means elements are
/// missing from the tree — and therefore cannot be addressed by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncation {
    pub limit: TruncationLimit,
    pub nodes_walked: usize,
}

impl Truncation {
    /// The disclosure appended to every rendered outline. Says plainly that elements are
    /// missing and names the pixel fallback — the same shape as [`AxTree::empty_guidance`],
    /// because a truncated tree fails the agent the same way a treeless one does.
    pub fn notice(&self) -> String {
        format!(
            "… tree truncated at {} {} ({} nodes walked). Some elements are NOT shown and \
             cannot be addressed by id. Narrow the UI, or drive by pixels: glass_screenshot, \
             then glass_click at x,y.",
            self.limit.value(),
            self.limit.label(),
            self.nodes_walked,
        )
    }
}

/// Bookkeeping for a bounded pre-order walk. Every backend threads one of these through its
/// traversal so the caps and the truncation record are computed one way rather than five.
#[derive(Debug, Default)]
pub struct WalkBudget {
    count: usize,
    truncated: Option<Truncation>,
}

impl WalkBudget {
    pub fn new() -> WalkBudget {
        WalkBudget::default()
    }

    /// Count a visited node. Call exactly once on entry to each node, before its children.
    pub fn visit(&mut self) {
        self.count += 1;
    }

    pub fn nodes_walked(&self) -> usize {
        self.count
    }

    /// Whether the node budget is spent.
    pub fn nodes_exhausted(&self) -> bool {
        self.count >= MAX_NODES
    }

    /// Whether `depth` has reached the nesting bound (so children must not be walked).
    pub fn depth_exhausted(&self, depth: usize) -> bool {
        depth >= MAX_DEPTH
    }

    /// Record that a bound stopped the walk. Only the FIRST hit is kept: it is the cause,
    /// while any later hit is a consequence of having continued.
    pub fn hit(&mut self, limit: TruncationLimit) {
        let nodes_walked = self.count;
        self.truncated.get_or_insert(Truncation {
            limit,
            nodes_walked,
        });
    }

    /// The recorded truncation, or `None` when the walk completed.
    pub fn truncation(&self) -> Option<Truncation> {
        self.truncated
    }
}

/// The active window's accessibility subtree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxTree {
    pub root: AxNode,
    /// Total node count; set by [`AxTree::assign_ids`].
    pub count: usize,
    /// `Some` when the backend stopped walking early — see [`Truncation`]. `None` means the
    /// tree is complete.
    pub truncated: Option<Truncation>,
}

impl AxTree {
    /// A complete (non-truncated) tree. Callers still run [`AxTree::assign_ids`]. A backend
    /// that stopped early sets [`AxTree::truncated`] afterward.
    pub fn new(root: AxNode) -> AxTree {
        AxTree {
            root,
            count: 0,
            truncated: None,
        }
    }

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
        // Truncation is a fact about the tree, not a rendering style, so it is disclosed
        // wherever the tree is rendered. A capped tree that rendered as complete would read
        // as "the element does not exist" — the silent fallback this field exists to prevent.
        if let Some(t) = self.truncated {
            let _ = writeln!(out, "{}", t.notice());
        }
        out
    }

    /// Guidance to surface when a snapshot exposes nothing to address — only the window
    /// root, with no child elements. That means the app isn't publishing a usable
    /// accessibility tree, which (outside the Linux no-bus path, which errors before a
    /// tree is ever built) otherwise returns a bare root-only outline with no next step.
    /// Backend-agnostic: the same thin-tree outcome on Windows/macOS/Android now steers
    /// the agent to the pixel loop the way the Linux reader's no-tree error already does.
    pub fn empty_guidance(&self) -> Option<&'static str> {
        self.root.children.is_empty().then_some(
            "no accessibility elements exposed — the app may not publish an a11y tree \
             (some toolkits need it enabled, e.g. relaunch with a11y:true; canvas/game apps \
             never will). Drive it by pixels instead: glass_screenshot, then glass_click at x,y.",
        )
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
            Checked => |s| s.checkable && s.checked,
            Unchecked => |s| s.checkable && !s.checked,
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
    // Role filter: exact match, OR — when an interactable role is requested *and* a name or
    // value disambiguator is also present — an actable node the backend left generically
    // classified. Toolkits like Jetpack Compose surface a real button as a clickable
    // `Group`/`Other` (the role is lost), so an exact filter would miss it; matching by
    // name + actability finds it anyway. The disambiguator is required: without it, a
    // role-only query would match the first focusable container in the tree — a confident
    // wrong match reported as success rather than an honest miss.
    let has_disambiguator = name.is_some() || value_contains.is_some();
    let role_match = |n: &AxNode, r: AxRole| {
        n.role == r
            || (r.is_interactable()
                && has_disambiguator
                && n.states.focusable
                && matches!(n.role, AxRole::Group | AxRole::Other))
    };
    let selector_match = |n: &AxNode| -> bool {
        name.is_none_or(|q| n.name.as_deref().is_some_and(|nm| nm.contains(q)))
            && role.is_none_or(|r| role_match(n, r))
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
        let t = AxTarget {
            id: AxNodeId(3),
            role: AxRole::TextField,
            name: Some("Email".into()),
            bounds: None,
        };
        assert!(t.matches(AxRole::TextField, Some("Email")));
        assert!(!t.matches(AxRole::Button, Some("Email")), "role must match");
        assert!(
            !t.matches(AxRole::TextField, Some("Name")),
            "name must match"
        );
        assert!(
            !t.matches(AxRole::TextField, None),
            "missing name must not match a named target"
        );

        let t_unnamed = AxTarget {
            id: AxNodeId(5),
            role: AxRole::TextField,
            name: None,
            bounds: None,
        };
        assert!(
            t_unnamed.matches(AxRole::TextField, None),
            "unnamed target matches unnamed live node"
        );
        assert!(
            !t_unnamed.matches(AxRole::TextField, Some("X")),
            "unnamed target must not match a named live node"
        );
    }

    #[test]
    fn ax_target_bounds_consistent_rejects_a_moved_element() {
        let r = AxRect {
            x: 100,
            y: 50,
            width: 80,
            height: 20,
        };
        let t = AxTarget {
            id: AxNodeId(3),
            role: AxRole::TextField,
            name: None,
            bounds: Some(r),
        };
        // Exact and within-tolerance bounds pass.
        assert!(t.bounds_consistent(Some(r), 8));
        assert!(
            t.bounds_consistent(
                Some(AxRect {
                    x: 104,
                    y: 53,
                    width: 80,
                    height: 20
                }),
                8
            ),
            "minor jitter within tolerance is accepted"
        );
        // A different element that drift landed on this id sits elsewhere → rejected.
        assert!(!t.bounds_consistent(
            Some(AxRect {
                x: 300,
                y: 400,
                width: 120,
                height: 30
            }),
            8
        ));
        // Expected a positioned element but the reached one has none → reject.
        assert!(!t.bounds_consistent(None, 8));
        // No fingerprint captured → nothing to verify, accept (role+name still gates).
        let t_nofp = AxTarget {
            id: AxNodeId(3),
            role: AxRole::TextField,
            name: None,
            bounds: None,
        };
        assert!(t_nofp.bounds_consistent(Some(r), 8));
        assert!(t_nofp.bounds_consistent(None, 8));
    }

    #[test]
    fn clamped_center_is_in_bounds() {
        let r = AxRect {
            x: 10,
            y: 20,
            width: 40,
            height: 10,
        };
        assert_eq!(r.clamped_center(100, 100), Some((30, 25)));
    }

    #[test]
    fn clamped_center_rejects_fully_offscreen() {
        // Element entirely past the window's right/bottom edge → no visible portion → None
        // (a not-clickable error, not a silent click on the window corner that misses it).
        let r = AxRect {
            x: 90,
            y: 90,
            width: 40,
            height: 40,
        };
        assert_eq!(r.clamped_center(64, 48), None);
    }

    #[test]
    fn clamped_center_uses_visible_portion_when_partially_clipped() {
        // Element spans x[60,100] in an 80-wide window → visible x[60,80], center x=70; y
        // fully inside. The click lands on the visible part of the element, not the edge.
        let r = AxRect {
            x: 60,
            y: 10,
            width: 40,
            height: 20,
        };
        assert_eq!(r.clamped_center(80, 100), Some((70, 20)));
    }

    #[test]
    fn clamped_center_uses_visible_portion_when_clipped_top_left() {
        // Element hangs off the top-left (a negative origin is valid — see `AxRect.x/y`):
        // spans x[-10,30] in an 80-wide window → visible x[0,30], center 15; y[-4,16] → visible
        // y[0,16], center 8. Exercises the `.max(0)` clip on the left/top edges.
        let r = AxRect {
            x: -10,
            y: -4,
            width: 40,
            height: 20,
        };
        assert_eq!(r.clamped_center(80, 100), Some((15, 8)));
    }

    #[test]
    fn clamped_center_rejects_zero_area() {
        assert_eq!(
            AxRect {
                x: 0,
                y: 0,
                width: 0,
                height: 5
            }
            .clamped_center(10, 10),
            None
        );
        assert_eq!(
            AxRect {
                x: 0,
                y: 0,
                width: 5,
                height: 5
            }
            .clamped_center(0, 10),
            None
        );
    }

    #[test]
    fn clamped_trailing_point_targets_the_trailing_control() {
        // A row-shaped element (idb's whole-cell switch frame): the trailing point sits
        // right of center, near the right edge — not the geometric center.
        let r = AxRect {
            x: 0,
            y: 0,
            width: 300,
            height: 30,
        };
        let (x, y) = r.clamped_trailing_point(400, 400).expect("has a point");
        let (cx, _) = r.clamped_center(400, 400).unwrap();
        assert!(x > cx, "trailing point is right of center ({x} !> {cx})");
        assert!(
            (270..300).contains(&x),
            "near the right edge, inset ~= height"
        );
        assert_eq!(y, 15, "vertical center");
    }

    #[test]
    fn clamped_trailing_point_never_crosses_left_of_center() {
        // A near-square element: right - height would fall left of center, so it floors at center.
        let r = AxRect {
            x: 0,
            y: 0,
            width: 30,
            height: 30,
        };
        let (x, _) = r.clamped_trailing_point(400, 400).unwrap();
        assert_eq!(x, r.clamped_center(400, 400).unwrap().0);
    }

    #[test]
    fn clamped_trailing_point_rejects_offscreen_like_clamped_center() {
        let r = AxRect {
            x: 500,
            y: 500,
            width: 40,
            height: 20,
        };
        assert_eq!(r.clamped_trailing_point(400, 400), None);
    }

    #[test]
    fn trailing_toggle_swipe_crosses_the_trailing_control() {
        // A row-shaped switch (idb's whole-cell frame): 990 wide, 84 tall, at (108,439),
        // window 1206x2622 — the rc3 KeyboardVisceral geometry.
        let r = AxRect {
            x: 108,
            y: 439,
            width: 990,
            height: 84,
        };
        let seg = r.trailing_toggle_swipe(1206, 2622).expect("has a segment");
        // Anchor == clamped_trailing_point.x; swipe is centered on it, span = 1.5*inset(84) = 126.
        let (anchor_x, anchor_y) = r.clamped_trailing_point(1206, 2622).unwrap();
        assert_eq!(seg.from_y, anchor_y);
        assert_eq!(
            seg.to_y, anchor_y,
            "horizontal swipe stays at the control's vertical center"
        );
        assert!(seg.from_x < seg.to_x, "real left-to-right movement");
        assert_eq!(seg.from_x, anchor_x - 63);
        assert_eq!(seg.to_x, anchor_x + 63);
        // Entirely in the right half — structurally clear of the left-edge back-swipe zone.
        assert!(seg.from_x > (r.x + r.x + r.width as i32) / 2);
    }

    #[test]
    fn trailing_toggle_swipe_clamps_into_visible_bounds() {
        // A tall/narrow control (height > width/2): the anchor falls back to center_x and the
        // half-span (1.5*inset) overshoots BOTH edges, so both clamps must fire.
        // rect 20x40 in a 400x400 window: inset=40, center_x=10, anchor_x=10, half=30 →
        // unclamped (-20, 40) → clamped to (0, 20). Deleting either clamp breaks these asserts.
        let r = AxRect {
            x: 0,
            y: 0,
            width: 20,
            height: 40,
        };
        let seg = r.trailing_toggle_swipe(400, 400).unwrap();
        assert_eq!(seg.from_x, 0, "from clamps to the left edge");
        assert_eq!(seg.to_x, 20, "to clamps to the right edge");
        assert!(seg.from_x < seg.to_x, "still a real left-to-right movement");
    }

    #[test]
    fn trailing_toggle_swipe_keeps_a_nonzero_span_for_a_thin_control() {
        // A 1px-tall control: inset*3/4 == 0 without the .max(1) guard → a zero-length "tap".
        let r = AxRect {
            x: 0,
            y: 0,
            width: 100,
            height: 1,
        };
        let seg = r.trailing_toggle_swipe(400, 400).unwrap();
        assert!(
            seg.from_x < seg.to_x,
            "even a 1px-tall control yields a real swipe, not a zero-length tap"
        );
    }

    #[test]
    fn trailing_toggle_swipe_rejects_offscreen_like_clamped_center() {
        let r = AxRect {
            x: 500,
            y: 500,
            width: 40,
            height: 20,
        };
        assert_eq!(r.trailing_toggle_swipe(400, 400), None);
    }

    #[test]
    fn active_states_listed_in_order() {
        let s = AxStates {
            focusable: true,
            enabled: true,
            checked: true,
            checkable: true,
            ..Default::default()
        };
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

    #[test]
    fn empty_guidance_flags_a_treeless_snapshot() {
        // Only the window root, no children → nothing to address → steer to pixels.
        let empty = AxTree::new(leaf(AxRole::Window, "App"));
        let hint = empty
            .empty_guidance()
            .expect("a root-only tree must yield guidance");
        assert!(
            hint.contains("glass_screenshot"),
            "guidance names the pixel path: {hint}"
        );
        // A tree with real elements has something to address — no hint.
        assert!(sample_tree().empty_guidance().is_none());
    }

    fn sample_tree() -> AxTree {
        let mut button = leaf(AxRole::Button, "Save");
        button.bounds = Some(AxRect {
            x: 12,
            y: 40,
            width: 80,
            height: 24,
        });
        button.states = AxStates {
            focusable: true,
            enabled: true,
            ..Default::default()
        };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Settings".into()),
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            }),
            children: vec![button, leaf(AxRole::Label, "Ready")],
        };
        AxTree::new(root)
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
        for r in [
            AxRole::Window,
            AxRole::Group,
            AxRole::Label,
            AxRole::Image,
            AxRole::Other,
        ] {
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
        assert_eq!(
            ElementCondition::from_name("appears"),
            Some(ElementCondition::Appears)
        );
        assert_eq!(
            ElementCondition::from_name("disappears"),
            Some(ElementCondition::Disappears)
        );
        assert_eq!(
            ElementCondition::from_name("enabled"),
            Some(ElementCondition::Enabled)
        );
        assert_eq!(
            ElementCondition::from_name("hidden"),
            Some(ElementCondition::Hidden)
        );
        assert_eq!(ElementCondition::from_name("wat"), None);
        // case-insensitive
        assert_eq!(
            ElementCondition::from_name("Enabled"),
            Some(ElementCondition::Enabled)
        );
        assert_eq!(
            ElementCondition::from_name("DISAPPEARS"),
            Some(ElementCondition::Disappears)
        );
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
        match element_match(
            &t,
            None,
            Some(AxRole::Button),
            None,
            ElementCondition::Appears,
        ) {
            ElementMatch::Satisfied(Some(n)) => assert_eq!(n.name.as_deref(), Some("Save")),
            other => panic!("expected the Button, got {other:?}"),
        }
    }

    #[test]
    fn role_button_matches_unclassified_actable_node() {
        // A Compose button often surfaces as a clickable (focusable) Group with the role
        // lost — `role:"Button"` should still find it by name + actability.
        let mut clickable = leaf(AxRole::Group, "Submit");
        clickable.states = AxStates {
            focusable: true,
            enabled: true,
            ..Default::default()
        };
        let inert = leaf(AxRole::Group, "Panel"); // a non-actable Group must NOT match Button
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("App".into()),
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![clickable, inert],
        };
        let t = AxTree::new(root);
        assert!(
            matches!(
                element_match(&t, Some("Submit"), Some(AxRole::Button), None, ElementCondition::Appears),
                ElementMatch::Satisfied(Some(n)) if n.name.as_deref() == Some("Submit")
            ),
            "clickable Group should satisfy role:Button"
        );
        assert!(
            matches!(
                element_match(
                    &t,
                    Some("Panel"),
                    Some(AxRole::Button),
                    None,
                    ElementCondition::Appears
                ),
                ElementMatch::Pending
            ),
            "a non-actable Group must not satisfy role:Button"
        );
    }

    #[test]
    fn role_alone_does_not_match_a_bare_focusable_container() {
        // A focusable container Group (e.g. a scrollable table/viewport) must NOT satisfy a
        // role-only interactable query: with no name/value to disambiguate, the generic
        // actable fallback would otherwise return the container as a confident wrong match.
        let container = AxNode {
            id: AxNodeId(0),
            role: AxRole::Group,
            raw_role: "panel".into(),
            name: None,
            value: None,
            states: AxStates {
                focusable: true,
                enabled: true,
                ..Default::default()
            },
            bounds: None,
            children: vec![],
        };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("App".into()),
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![container],
        };
        let t = AxTree::new(root);
        assert!(
            matches!(
                element_match(
                    &t,
                    None,
                    Some(AxRole::Button),
                    None,
                    ElementCondition::Appears
                ),
                ElementMatch::Pending
            ),
            "role:Button alone must not match a bare focusable container Group"
        );
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
        match element_match(
            &t,
            None,
            Some(AxRole::Label),
            Some("50%"),
            ElementCondition::Appears,
        ) {
            ElementMatch::Satisfied(Some(n)) => assert_eq!(n.name.as_deref(), Some("Ready")),
            other => panic!("expected the Label by value, got {other:?}"),
        }
        assert!(matches!(
            element_match(
                &t,
                None,
                Some(AxRole::Label),
                Some("99%"),
                ElementCondition::Appears
            ),
            ElementMatch::Pending
        ));
    }

    #[test]
    fn checked_conditions_require_checkable() {
        let non_toggle = AxStates {
            checkable: false,
            checked: false,
            ..Default::default()
        };
        let off = AxStates {
            checkable: true,
            checked: false,
            ..Default::default()
        };
        let on = AxStates {
            checkable: true,
            checked: true,
            ..Default::default()
        };
        // The asymmetric case: a backend that (incorrectly) reports `checked:true` without
        // `checkable:true` — this is what distinguishes the gated `s.checkable && s.checked`
        // arm from an ungated `s.checked` arm, which would wrongly satisfy `Checked` here.
        let checked_but_not_checkable = AxStates {
            checkable: false,
            checked: true,
            ..Default::default()
        };
        let pred = |c: ElementCondition| c.state_pred();
        // non-checkable matches NEITHER (the fix)
        assert!(!(pred(ElementCondition::Unchecked))(&non_toggle));
        assert!(!(pred(ElementCondition::Checked))(&non_toggle));
        // real toggle matches per its checked state
        assert!((pred(ElementCondition::Unchecked))(&off));
        assert!(!(pred(ElementCondition::Checked))(&off));
        assert!((pred(ElementCondition::Checked))(&on));
        assert!(!(pred(ElementCondition::Unchecked))(&on));
        // checked:true with checkable:false still matches NEITHER — checked alone is not
        // enough without checkable.
        assert!(!(pred(ElementCondition::Checked))(
            &checked_but_not_checkable
        ));
        assert!(!(pred(ElementCondition::Unchecked))(
            &checked_but_not_checkable
        ));
    }

    #[test]
    fn active_renders_toggle_state_only_when_checkable() {
        let on = AxStates {
            checkable: true,
            checked: true,
            ..Default::default()
        };
        let off = AxStates {
            checkable: true,
            checked: false,
            ..Default::default()
        };
        let plain = AxStates {
            checkable: false,
            checked: false,
            ..Default::default()
        };
        // The asymmetric case: `checked:true` without `checkable:true` — distinguishes
        // `active()`'s `if self.checkable { push checked/unchecked }` gating from a
        // hypothetical ungated version that renders off `self.checked` alone.
        let checked_but_not_checkable = AxStates {
            checkable: false,
            checked: true,
            ..Default::default()
        };
        assert!(on.active().contains(&"checked"));
        assert!(off.active().contains(&"unchecked"));
        assert!(!plain.active().contains(&"checked") && !plain.active().contains(&"unchecked"));
        assert!(
            !checked_but_not_checkable.active().contains(&"checked")
                && !checked_but_not_checkable.active().contains(&"unchecked")
        );
    }

    #[test]
    fn walk_budget_records_the_first_limit_hit_not_the_last() {
        // The FIRST bound is the cause; a later one is a consequence of continuing to walk.
        let mut b = WalkBudget::new();
        b.visit();
        b.hit(TruncationLimit::Depth);
        b.hit(TruncationLimit::Nodes);
        assert_eq!(
            b.truncation().map(|t| t.limit),
            Some(TruncationLimit::Depth)
        );
    }

    #[test]
    fn walk_budget_reports_no_truncation_when_no_limit_was_hit() {
        let mut b = WalkBudget::new();
        b.visit();
        assert_eq!(b.truncation(), None);
    }

    #[test]
    fn walk_budget_nodes_exhausted_flips_at_the_node_cap() {
        let mut b = WalkBudget::new();
        for _ in 0..MAX_NODES - 1 {
            b.visit();
        }
        assert!(!b.nodes_exhausted(), "one visit short of the cap");
        b.visit();
        assert!(
            b.nodes_exhausted(),
            "the cap is reached at exactly MAX_NODES"
        );
    }

    #[test]
    fn truncation_notice_states_elements_are_missing_and_names_the_pixel_fallback() {
        let n = Truncation {
            limit: TruncationLimit::Nodes,
            nodes_walked: 1500,
        }
        .notice();
        assert!(
            n.contains("NOT shown") && n.contains("glass_screenshot"),
            "notice must be unmissable and steer to pixels: {n}"
        );
    }

    #[test]
    fn outline_appends_the_truncation_notice_when_the_walk_stopped_early() {
        let mut t = sample_tree();
        t.assign_ids();
        t.truncated = Some(Truncation {
            limit: TruncationLimit::Depth,
            nodes_walked: 42,
        });
        assert!(
            t.to_outline().contains("truncated"),
            "a truncated tree must never render as if complete"
        );
    }

    #[test]
    fn outline_appends_nothing_when_the_tree_is_complete() {
        let mut t = sample_tree();
        t.assign_ids();
        assert!(!t.to_outline().contains("truncated"));
    }

    #[test]
    fn new_builds_a_complete_tree() {
        assert_eq!(AxTree::new(leaf(AxRole::Window, "App")).truncated, None);
    }
}
