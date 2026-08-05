//! Platform-agnostic accessibility model + backend seam.
//!
//! Accessibility is a **per-OS** concern, distinct from the per-display-server
//! [`crate::platform::Platform`] seam: AT-SPI serves both X11 and Wayland, and
//! macOS/Windows each expose exactly one accessibility API. Backends (e.g.
//! `glass-a11y-linux`) map their native roles/states into the normalized types
//! here; no OS/AT-SPI/D-Bus types appear in this module.

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
    /// Every role except [`AxRole::Other`], which is the sink for unmapped native tokens
    /// rather than a mapping target. Used by the per-backend role-parity tests and by
    /// [`crate::role_support::ROLE_SUPPORT`].
    pub const ALL: [AxRole; 33] = [
        AxRole::Application,
        AxRole::Window,
        AxRole::Dialog,
        AxRole::Group,
        AxRole::Button,
        AxRole::ToggleButton,
        AxRole::RadioButton,
        AxRole::CheckBox,
        AxRole::MenuBar,
        AxRole::Menu,
        AxRole::MenuItem,
        AxRole::Label,
        AxRole::TextField,
        AxRole::TextArea,
        AxRole::ComboBox,
        AxRole::List,
        AxRole::ListItem,
        AxRole::Table,
        AxRole::Cell,
        AxRole::Tree,
        AxRole::TreeItem,
        AxRole::TabList,
        AxRole::Tab,
        AxRole::ScrollBar,
        AxRole::Slider,
        AxRole::SpinButton,
        AxRole::ProgressBar,
        AxRole::Image,
        AxRole::Link,
        AxRole::Separator,
        AxRole::Toolbar,
        AxRole::StatusBar,
        AxRole::Heading,
    ];

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
        // No zero-area early return: the emptiness test below already covers it — a zero-width
        // rect or window forces `right <= left`, so a separate guard is unobservable.
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
    /// A secondary human label the platform exposes separately from `name`: help/tooltip text
    /// on desktop, or the human label where `name` is a developer-assigned id. Kept out of
    /// `name` because `name` is half the [`AxTarget`] fingerprint `set_value` re-walks against
    /// and has to stay stable. Assign it through [`normalize_description`], which is what drops
    /// a blank label or one that only repeats `name`.
    pub description: Option<String>,
    pub value: Option<String>,
    pub states: AxStates,
    pub bounds: Option<AxRect>,
    pub children: Vec<AxNode>,
}

/// A node's description, or `None` when it would add nothing: empty, whitespace-only, or the
/// same string as `name` (platforms routinely report both fields with one label in them).
/// Every backend normalizes through this so the rule cannot drift per-platform.
pub fn normalize_description(raw: &str, name: Option<&str>) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || Some(trimmed) == name.map(str::trim) {
        return None;
    }
    Some(trimmed.to_string())
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

/// Runtime bounds for one accessibility walk: the caps that were compile-time consts
/// (`MAX_NODES`/`MAX_DEPTH`/`MAX_SIBLINGS`). `usize::MAX` in a field means "unbounded"; a
/// plain `>=`/`>` check never trips there, so unbounded needs no special-casing. Carried by
/// [`WalkBudget`] and threaded through [`AxContext`] so a caller can raise or lift the caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkLimits {
    pub nodes: usize,
    pub depth: usize,
    pub siblings: usize,
}

impl WalkLimits {
    /// The historical caps — the behavior when a caller passes no override.
    pub const DEFAULT: WalkLimits = WalkLimits {
        nodes: MAX_NODES,
        depth: MAX_DEPTH,
        siblings: MAX_SIBLINGS,
    };

    /// Map the MCP `max_nodes` surface to limits: `None` → default; `Some(0)` → lift the node
    /// cap (`nodes = usize::MAX`) for the full tree; `Some(n)` → cap nodes at `n`. `max_nodes`
    /// controls the node count ONLY — `depth` and `siblings` always keep their defaults. Those
    /// two are structural safety rails, not a size budget: the recursive native-tree walkers
    /// (AT-SPI / AX / UIA) have no cycle detection, so an unbounded depth on a cyclic or
    /// pathological tree would recurse to a stack overflow (which aborts the process). The
    /// generous defaults (`MAX_DEPTH`/`MAX_SIBLINGS`) never bite a real UI, so keeping them costs
    /// the caller nothing while preserving that backstop even under `max_nodes: 0`.
    pub fn from_max_nodes(max_nodes: Option<usize>) -> WalkLimits {
        match max_nodes {
            None => WalkLimits::DEFAULT,
            Some(0) => WalkLimits {
                nodes: usize::MAX,
                ..WalkLimits::DEFAULT
            },
            Some(n) => WalkLimits {
                nodes: n,
                ..WalkLimits::DEFAULT
            },
        }
    }
}

impl Default for WalkLimits {
    fn default() -> WalkLimits {
        WalkLimits::DEFAULT
    }
}

/// Which bound stopped a walk early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationLimit {
    Nodes,
    Depth,
    Siblings,
}

impl TruncationLimit {
    /// Human-readable unit for a disclosure notice: pairs with [`Truncation::limit_value`].
    pub fn label(self) -> &'static str {
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
    /// The actual limit value that fired — the runtime [`WalkLimits`] field in effect, not the
    /// compile-time default — so the notice reports the cap the caller was really subject to
    /// (e.g. a `max_nodes: 42` snapshot reads "truncated at 42 nodes", not "1500").
    pub limit_value: usize,
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
            self.limit_value,
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
    unreadable: usize,
    limits: WalkLimits,
}

impl WalkBudget {
    /// A budget with the default (historical) limits.
    pub fn new() -> WalkBudget {
        WalkBudget::with_limits(WalkLimits::DEFAULT)
    }

    /// A budget bounded by `limits`.
    pub fn with_limits(limits: WalkLimits) -> WalkBudget {
        WalkBudget {
            count: 0,
            truncated: None,
            unreadable: 0,
            limits,
        }
    }

    /// The per-level sibling-scan bound (readers/mappers compare against this).
    pub fn max_siblings(&self) -> usize {
        self.limits.siblings
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
        self.count >= self.limits.nodes
    }

    /// Whether `depth` has reached the nesting bound (so children must not be walked).
    pub fn depth_exhausted(&self, depth: usize) -> bool {
        depth >= self.limits.depth
    }

    /// Record a subtree dropped because the platform refused to read a child.
    ///
    /// Distinct from [`WalkBudget::hit`]: a bound is a limit glass chose and can raise, while
    /// this is the walk wanting to continue and being unable to. Call it wherever a child read
    /// errors and the traversal skips on.
    pub fn note_unreadable(&mut self) {
        self.unreadable += 1;
    }

    /// How many subtrees were dropped because a child read failed.
    pub fn unreadable(&self) -> usize {
        self.unreadable
    }

    /// Record that a bound stopped the walk. Only the FIRST hit is kept: it is the cause,
    /// while any later hit is a consequence of having continued.
    pub fn hit(&mut self, limit: TruncationLimit) {
        let nodes_walked = self.count;
        let limit_value = match limit {
            TruncationLimit::Nodes => self.limits.nodes,
            TruncationLimit::Depth => self.limits.depth,
            TruncationLimit::Siblings => self.limits.siblings,
        };
        self.truncated.get_or_insert(Truncation {
            limit,
            limit_value,
            nodes_walked,
        });
    }

    /// The recorded truncation, or `None` when the walk completed.
    pub fn truncation(&self) -> Option<Truncation> {
        self.truncated
    }
}

/// What a caller asked a backend about, and what it answered about, when those differ.
///
/// Only a backend addressed by *name* can tell — the desktop readers are handed a window and have
/// nothing to compare. `None` on a tree means no claim either way, not agreement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub asked: String,
    pub actual: String,
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
    /// Subtrees dropped because a child read failed. Independent of [`AxTree::truncated`]: a
    /// tree can hit no bound at all and still be missing elements this way.
    pub unreadable: usize,
    /// `Some` when the backend answered about something other than what it was asked about —
    /// see [`Subject`].
    pub subject: Option<Subject>,
}

impl AxTree {
    /// A complete (non-truncated) tree. Callers still run [`AxTree::assign_ids`]. A backend
    /// that stopped early sets [`AxTree::truncated`] afterward.
    pub fn new(root: AxNode) -> AxTree {
        AxTree {
            root,
            count: 0,
            truncated: None,
            unreadable: 0,
            subject: None,
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

    /// [`Self::find`], mutably — for patching a field of a cached node in place rather than
    /// re-walking the whole tree.
    pub fn find_mut(&mut self, id: AxNodeId) -> Option<&mut AxNode> {
        fn walk(node: &mut AxNode, id: AxNodeId) -> Option<&mut AxNode> {
            if node.id == id {
                return Some(node);
            }
            node.children.iter_mut().find_map(|c| walk(c, id))
        }
        walk(&mut self.root, id)
    }

    /// Render a compact indented outline, one line per node, in `outline::write_line`'s format —
    /// the single definition of it, shared with [`crate::outline::render_compact`]. This render
    /// differs only in keeping every node: nothing is collapsed.
    ///
    /// Pure tree text — no truncation notice is appended here. Keeping this render pure
    /// means `scroll_to_element`'s saturation check (which diffs consecutive `to_outline`
    /// strings) can't be perturbed by a truncation-status flip that has nothing to do with
    /// scrolling, and it keeps the notice out of the untrusted envelope the caller wraps
    /// this text in at the MCP boundary. See [`Self::truncation_notice`].
    pub fn to_outline(&self) -> String {
        fn walk(node: &AxNode, depth: usize, out: &mut String) {
            crate::outline::write_line(node, depth, out);
            for child in &node.children {
                walk(child, depth + 1, out);
            }
        }
        let mut out = String::new();
        walk(&self.root, 0, &mut out);
        out
    }

    /// The trusted truncation steer for the MCP boundary to surface as its own content
    /// block — NOT baked into a render, so it is never buried inside the untrusted-content
    /// envelope the app-derived outline is wrapped in. `None` when the tree is complete.
    pub fn truncation_notice(&self) -> Option<String> {
        self.truncated.map(|t| t.notice())
    }

    /// Disclosure for subtrees the platform refused to read. Separate from
    /// [`AxTree::truncation_notice`] because the recourse differs: a bound is raisable and
    /// deterministic, whereas a failed child read is usually an element that went away
    /// mid-walk, so retrying is worth a try where widening a cap is not.
    ///
    /// Without this, such a tree renders exactly like one that genuinely had nothing there,
    /// and an agent concludes the element does not exist.
    pub fn unreadable_notice(&self) -> Option<String> {
        (self.unreadable > 0).then(|| {
            let (n, s) = (self.unreadable, if self.unreadable == 1 { "" } else { "s" });
            format!(
                "… {n} subtree{s} could not be read and {} missing from this outline. Those \
                 elements are NOT shown and cannot be addressed by id. The read usually fails \
                 because the element went away mid-walk, so a fresh glass_a11y_snapshot may \
                 show them; otherwise drive that area by pixels: glass_screenshot, then \
                 glass_click at x,y.",
                if self.unreadable == 1 { "is" } else { "are" },
            )
        })
    }

    /// Disclosure for a tree that describes something other than what was asked for — the ids in
    /// it still address what it actually describes.
    pub fn subject_notice(&self) -> Option<String> {
        self.subject.as_ref().map(|s| {
            format!(
                "… this describes {}, not the {} that was asked for — the ids above address that \
                 window, and a fresh glass_a11y_snapshot will follow the foreground.",
                s.actual, s.asked,
            )
        })
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
    /// The size bounds for this walk. The reader/mapper builds its `WalkBudget` from these.
    /// Set from the session's stored limits so a snapshot and a later `set_value` walk the
    /// tree with the same bounds (ids stay resolvable).
    pub limits: WalkLimits,
}

/// A fingerprint identifying the element a value-set targets: its synthetic id
/// (pre-order index), the role/name the caller saw in the snapshot, the
/// element's window-relative bounds when known, and the value it held. The
/// backend re-walks to the id and verifies role+name (and bounds, when present;
/// on Android the value too) so a stale id — or tree drift that lands a
/// *different* same-role+name element on the id — errors rather than
/// overwriting the wrong element.
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
    /// The element's value at snapshot time, when it had one. Captured on every backend, but
    /// compared only by Android's `set_value` guard (`editable_target`), where a recycled list
    /// row reuses the same view — role, name and rect all identical, only this different. The
    /// other backends carry it without reading it; see [`Self::value_consistent`].
    ///
    /// After a successful write the session cache patches this to the text that was requested —
    /// an exact fact only on a typed-write backend (Android/iOS, verified by
    /// `typed_text_landed`/`typed_clear_landed`). An atomic-write backend (Windows/macOS) may
    /// have reformatted the value instead (`read_back_confirms`), and Linux's AT-SPI writer
    /// doesn't read back at all, so on those a future consumer should treat this as a best
    /// available label, not a proven one.
    pub value: Option<String>,
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

    /// Whether a reached element's value `got` is consistent with the value captured for this
    /// target. `true` when none was captured: a captured `None` says the element held no value
    /// then, not which element it was, so gating on it would make every element that never held
    /// one unwritable. A live `None` against a captured value still rejects — Android reports an
    /// emptied field as no value at all, which is a real change, not a missing observation.
    pub fn value_consistent(&self, got: Option<&str>) -> bool {
        self.value.is_none() || self.value.as_deref() == got
    }
}

/// A backend's notification that the app's accessibility tree may have changed.
///
/// Exists so a wait can stop re-reading the whole tree on a timer. A walk is not free and grows
/// with the tree — measured on AT-SPI, 36ms for a small fixture and 732ms at the 1500-node cap —
/// so a wait for something that has not happened yet spends its whole budget re-reading a tree
/// that did not change.
///
/// Public and object-safe because it crosses the `Accessibility` seam, which backends implement
/// out-of-crate.
pub trait ChangeSignal: Send {
    /// Block until a change arrives or `timeout` elapses.
    ///
    /// Must never block past `timeout`, and must actually block for it when there is nothing to
    /// report: the deadline is the only thing standing between a subscription that stopped
    /// delivering and a hung wait, and an implementation that returns instantly every time turns
    /// the caller's poll loop into a spin.
    fn wait(&mut self, timeout: std::time::Duration) -> ChangeWait;
}

/// What a [`ChangeSignal`] learned while waiting.
///
/// `Quiet` and `Unusable` are separate answers because a caller does opposite things with them: on
/// `Quiet` it can skip re-reading the tree, which is the entire saving; on `Unusable` it must go
/// back to re-reading on the interval, because a signal that can no longer tell would otherwise
/// look like a permanently quiet app and the wait would never notice the state it is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeWait {
    /// Something changed — read the tree.
    Changed,
    /// Nothing changed within the timeout, and the signal is still trustworthy.
    Quiet,
    /// The subscription can no longer report changes (the stream ended, the bus dropped). The
    /// caller must stop trusting it and resume polling.
    Unusable,
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

    /// Subscribe to change notifications for the app described by `ctx`.
    ///
    /// `None` — the default — means this reader has no event stream, and its callers keep polling
    /// exactly as they did before. Two readers cannot have one as built: Android's `uiautomator`
    /// reader is a dump per call and iOS's is an `idb describe` per call. Android's *other* reader,
    /// the on-device accessibility service, is driven by events and is the natural next one.
    ///
    /// Subscribe before the first read, not after: a change that lands after a read but before the
    /// subscription is announced to nobody, and the caller then waits out its *entire* budget on a
    /// condition that already holds. (A change between subscribing and reading is safe — the read
    /// sees it.)
    fn subscribe_changes(&mut self, _ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        None
    }

    /// Set the editable element identified by `target` to `text`. The backend
    /// re-walks pre-order to `target.id`, verifies role+name, then sets via the
    /// native editable interface. Default: unsupported.
    fn set_value(&mut self, _ctx: &AxContext, _target: &AxTarget, _text: &str) -> Result<()> {
        Err(crate::error::GlassError::AxUnsupported)
    }

    /// Actuate the element identified by `target` via the platform's native
    /// accessibility action (the OS-level "press this control" verb). The
    /// backend re-walks pre-order to `target.id`, verifies the fingerprint
    /// (role+name, and bounds where its `set_value` does), then fires the
    /// action.
    ///
    /// Returns the element the action actually fired on, when that is **not**
    /// `target.id` — a backend whose toolkit carries a control's activation on an
    /// ancestor of the node that carries its label actuates that ancestor, and the
    /// caller reports the substitution. `None` means the target itself was actuated.
    ///
    /// **Error contract.** The caller falls back to a synthetic pointer click for
    /// exactly two outcomes, both meaning *nothing was dispatched*:
    /// [`crate::GlassError::AxUnsupported`] (this backend has no invoke) and
    /// [`crate::GlassError::AxActionUnavailable`] (the element exposes no activation
    /// action). Every other error propagates to the agent. So an implementation MUST
    /// report anything that may have dispatched — an action the toolkit ran but
    /// reported failed, a transport error whose answer was lost, a timeout — as
    /// `AxActionFailed`/`AccessibilityUnavailable`, and MUST NOT flatten such a case
    /// into `AxActionUnavailable`: a pointer click layered on top of a native action
    /// that still lands actuates the control twice. `AxElementChanged` (fingerprint
    /// mismatch) likewise propagates — a drifted tree would mis-click by stale
    /// coordinates too.
    ///
    /// Default: unsupported.
    fn invoke(&mut self, _ctx: &AxContext, _target: &AxTarget) -> Result<Option<AxNodeId>> {
        Err(crate::error::GlassError::AxUnsupported)
    }
}

/// How `click_element` actuated the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClickMethod {
    /// The platform's native accessibility action fired; no pointer was synthesized.
    /// `actuated` names the element the action fired on when it is not the one the
    /// caller asked for, and is `None` when they are the same element.
    NativeAction { actuated: Option<AxNodeId> },
    /// The synthetic pointer path ran; `native_fallback` says why the native
    /// action was not used (there is always a reason — invoke is attempted first).
    Pointer { native_fallback: String },
}

impl ClickMethod {
    /// Stable label for result payloads and the audit log.
    pub fn label(&self) -> &'static str {
        match self {
            ClickMethod::NativeAction { .. } => "native-action",
            ClickMethod::Pointer { .. } => "pointer",
        }
    }

    /// The fallback reason, when the pointer path ran.
    pub fn native_fallback(&self) -> Option<&str> {
        match self {
            ClickMethod::NativeAction { .. } => None,
            ClickMethod::Pointer { native_fallback } => Some(native_fallback),
        }
    }

    /// The element actuated in the caller's place, when the backend resolved the click
    /// onto a different one. `None` when the element asked for is the element clicked.
    pub fn actuated(&self) -> Option<AxNodeId> {
        match self {
            ClickMethod::NativeAction { actuated } => *actuated,
            ClickMethod::Pointer { .. } => None,
        }
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
    /// Every condition a wait can be given. Mirrors [`AxRole::ALL`]. `glass-a11y-windows` walks
    /// this array to build the set of UIA properties it registers, so a condition absent here is
    /// one that backend cannot be woken by, with nothing to fail. Not every subscribing backend
    /// derives its subscription this way — the AT-SPI reader registers by event class instead.
    pub const ALL: [ElementCondition; 13] = [
        ElementCondition::Appears,
        ElementCondition::Disappears,
        ElementCondition::Enabled,
        ElementCondition::Disabled,
        ElementCondition::Checked,
        ElementCondition::Unchecked,
        ElementCondition::Selected,
        ElementCondition::Unselected,
        ElementCondition::Expanded,
        ElementCondition::Collapsed,
        ElementCondition::Focused,
        ElementCondition::Visible,
        ElementCondition::Hidden,
    ];

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
    /// Carried so an element the outline labels `desc="…"` still has a label here; reported
    /// only, since the selector matched on `name`.
    pub description: Option<String>,
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
            description: n.description.clone(),
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
    // Jetpack Compose surfaces a real button as a clickable `Group`/`Other` with the role
    // lost, so an exact filter misses it; name + actability finds it anyway.
    //
    // The disambiguator is required: without it a role-only query would match the first
    // focusable container in the tree — a confident wrong match, not an honest miss.
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

    /// Compile-time guard for [`AxRole::ALL`] — never called, and exists only for its exhaustive
    /// match. The role-parity tests and [`crate::role_support::ROLE_SUPPORT`] quantify their
    /// completeness claims over `ALL`, so a new variant missing from it would silently weaken
    /// every one of them; this match stops compiling until the variant is classified — listed in
    /// the first arm and in `ALL`, or in the second arm as a deliberate exclusion.
    #[expect(dead_code, reason = "exists only for its exhaustive match")]
    fn all_is_exhaustive(role: AxRole) {
        match role {
            AxRole::Application
            | AxRole::Window
            | AxRole::Dialog
            | AxRole::Group
            | AxRole::Button
            | AxRole::ToggleButton
            | AxRole::RadioButton
            | AxRole::CheckBox
            | AxRole::MenuBar
            | AxRole::Menu
            | AxRole::MenuItem
            | AxRole::Label
            | AxRole::TextField
            | AxRole::TextArea
            | AxRole::ComboBox
            | AxRole::List
            | AxRole::ListItem
            | AxRole::Table
            | AxRole::Cell
            | AxRole::Tree
            | AxRole::TreeItem
            | AxRole::TabList
            | AxRole::Tab
            | AxRole::ScrollBar
            | AxRole::Slider
            | AxRole::SpinButton
            | AxRole::ProgressBar
            | AxRole::Image
            | AxRole::Link
            | AxRole::Separator
            | AxRole::Toolbar
            | AxRole::StatusBar
            | AxRole::Heading => {}
            // Deliberately excluded from `ALL`: the sink for unmapped native tokens, not a
            // mapping target.
            AxRole::Other => {}
        }
    }

    /// The budget's accessors report what it actually counted, and the two exhaustion tests
    /// fire at their own bound rather than sharing one.
    #[test]
    fn walk_budget_reports_and_bounds_what_it_counted() {
        let limits = WalkLimits {
            nodes: 3,
            depth: 2,
            siblings: 4,
        };
        let mut b = WalkBudget::with_limits(limits);
        assert_eq!(b.nodes_walked(), 0);
        assert!(!b.nodes_exhausted());

        b.visit();
        assert_eq!(b.nodes_walked(), 1, "one visit is one node");
        assert!(!b.nodes_exhausted());
        b.visit();
        b.visit();
        assert_eq!(b.nodes_walked(), 3);
        assert!(b.nodes_exhausted(), "the bound is reached, not exceeded");

        // Depth is a separate bound, read per call rather than from the running count.
        assert!(!b.depth_exhausted(0));
        assert!(!b.depth_exhausted(1));
        assert!(b.depth_exhausted(2), "reaching the bound stops the descent");
        assert!(b.depth_exhausted(3));
    }

    /// The disclosure notice names the unit that stopped the walk, so the three are distinct.
    #[test]
    fn truncation_limits_have_distinct_labels() {
        assert_eq!(TruncationLimit::Nodes.label(), "nodes");
        assert_eq!(TruncationLimit::Depth.label(), "levels deep");
        assert_eq!(TruncationLimit::Siblings.label(), "siblings per level");
    }

    /// A backend that does not implement value-setting must say so, not silently succeed:
    /// reporting Ok having changed nothing is the "no silent fallbacks" invariant inverted.
    #[test]
    fn set_value_defaults_to_unsupported() {
        struct Bare;
        impl Accessibility for Bare {
            fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
                Err(crate::error::GlassError::AxUnsupported)
            }
        }
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: None,
            bounds: None,
            value: None,
        };
        let ctx = AxContext {
            pids: vec![],
            window: WindowGeometry::default(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
        };
        let err = Bare
            .set_value(&ctx, &target, "x")
            .expect_err("the default must refuse, not report success");
        assert!(matches!(err, crate::error::GlassError::AxUnsupported));
    }

    /// Every arm of the condition table, plus the predicate each one selects. `Appears` and
    /// `Disappears` share an always-true predicate; the rest split into a pair per state, so
    /// each is asserted both ways round.
    #[test]
    fn every_condition_parses_and_selects_its_predicate() {
        use ElementCondition::*;
        let pairs: [(&str, ElementCondition); 13] = [
            ("appears", Appears),
            ("disappears", Disappears),
            ("enabled", Enabled),
            ("disabled", Disabled),
            ("checked", Checked),
            ("unchecked", Unchecked),
            ("selected", Selected),
            ("unselected", Unselected),
            ("expanded", Expanded),
            ("collapsed", Collapsed),
            ("focused", Focused),
            ("visible", Visible),
            ("hidden", Hidden),
        ];
        for (name, cond) in pairs {
            assert_eq!(ElementCondition::from_name(name), Some(cond), "{name}");
            assert_eq!(
                ElementCondition::from_name(&name.to_ascii_uppercase()),
                Some(cond),
                "{name} uppercased"
            );
        }
        assert_eq!(ElementCondition::from_name("nosuchcondition"), None);

        // `on` sets every flag; `off` clears them. A predicate wired to the wrong field, or
        // negated, disagrees with one of the two.
        let on = AxStates {
            focused: true,
            focusable: true,
            enabled: true,
            visible: true,
            selected: true,
            checkable: true,
            checked: true,
            expanded: true,
            editable: true,
        };
        let off = AxStates::default();

        for (cond, want_on, want_off) in [
            (Appears, true, true),
            (Disappears, true, true),
            (Enabled, true, false),
            (Disabled, false, true),
            (Checked, true, false),
            (Unchecked, false, false),
            (Selected, true, false),
            (Unselected, false, true),
            (Expanded, true, false),
            (Collapsed, false, true),
            (Focused, true, false),
            (Visible, true, false),
            (Hidden, false, true),
        ] {
            assert_eq!(cond.state_pred()(&on), want_on, "{cond:?} against all-set");
            assert_eq!(
                cond.state_pred()(&off),
                want_off,
                "{cond:?} against all-clear"
            );
        }

        // Checkable gates both check predicates: a node that cannot be checked satisfies
        // neither, which is not the same as being unchecked. That is why `Unchecked` is false
        // against the all-clear state above — it is not checkable there.
        let not_checkable = AxStates {
            checkable: false,
            checked: false,
            ..on
        };
        assert!(!Checked.state_pred()(&not_checkable));
        assert!(!Unchecked.state_pred()(&not_checkable));

        // The all-set / all-clear pair moves every flag together, so a predicate wired to a
        // neighbouring field agrees with the correct one on both. These split them apart.
        let visible_not_focused = AxStates {
            visible: true,
            focused: false,
            ..off
        };
        assert!(Visible.state_pred()(&visible_not_focused));
        assert!(!Hidden.state_pred()(&visible_not_focused));
        assert!(!Focused.state_pred()(&visible_not_focused));
        let focused_not_visible = AxStates {
            visible: false,
            focused: true,
            ..off
        };
        assert!(!Visible.state_pred()(&focused_not_visible));
        assert!(Hidden.state_pred()(&focused_not_visible));
        assert!(Focused.state_pred()(&focused_not_visible));
        // Same for the enabled/selected pair.
        let enabled_not_selected = AxStates {
            enabled: true,
            selected: false,
            ..off
        };
        assert!(Enabled.state_pred()(&enabled_not_selected));
        assert!(!Selected.state_pred()(&enabled_not_selected));
        assert!(Unselected.state_pred()(&enabled_not_selected));

        // Checkable and not checked is the one state `Unchecked` accepts.
        let checkable_off = AxStates {
            checkable: true,
            checked: false,
            ..off
        };
        assert!(Unchecked.state_pred()(&checkable_off));
        assert!(!Checked.state_pred()(&checkable_off));
    }

    /// Tolerance is inclusive and applied per axis, so a rect that is within it on three axes
    /// and past it on the fourth is still inconsistent.
    #[test]
    fn bounds_consistent_compares_every_axis_within_tolerance() {
        let want = AxRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        let target = |bounds| AxTarget {
            id: AxNodeId(1),
            role: AxRole::Button,
            name: None,
            bounds,
            value: None,
        };
        let t = target(Some(want));

        assert!(t.bounds_consistent(Some(want), 0));
        // No expectation accepts anything; an expectation with nothing to compare does not.
        assert!(target(None).bounds_consistent(None, 0));
        assert!(!t.bounds_consistent(None, 1000));

        // Exactly at the tolerance, on each axis in turn.
        for shift in [
            AxRect { x: 12, ..want },
            AxRect { y: 22, ..want },
            AxRect { width: 32, ..want },
            AxRect { height: 42, ..want },
        ] {
            assert!(
                t.bounds_consistent(Some(shift), 2),
                "{shift:?} at tolerance 2"
            );
            assert!(
                !t.bounds_consistent(Some(shift), 1),
                "{shift:?} at tolerance 1"
            );
        }

        // Negative differences count the same: the comparison is on the absolute value.
        assert!(t.bounds_consistent(Some(AxRect { x: 8, ..want }), 2));
        assert!(!t.bounds_consistent(Some(AxRect { x: 8, ..want }), 1));
    }

    /// The click point is the centre of the *visible* intersection, so an element hanging off
    /// an edge aims inside the window rather than at its own off-screen middle.
    #[test]
    fn clamped_center_uses_the_visible_intersection() {
        // Fully inside: the element's own centre.
        let inside = AxRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        assert_eq!(inside.clamped_center(100, 100), Some((25, 40)));

        // Hanging off the right and bottom: clipped to the window before centring.
        let over = AxRect {
            x: 80,
            y: 80,
            width: 40,
            height: 40,
        };
        assert_eq!(over.clamped_center(100, 100), Some((90, 90)));

        // Hanging off the left and top: the negative side is clamped to zero.
        let under = AxRect {
            x: -20,
            y: -20,
            width: 40,
            height: 40,
        };
        assert_eq!(under.clamped_center(100, 100), Some((10, 10)));

        // Nothing to click: zero-sized, zero-sized window, or entirely outside.
        assert_eq!(
            AxRect {
                x: 0,
                y: 0,
                width: 0,
                height: 10
            }
            .clamped_center(100, 100),
            None
        );
        assert_eq!(
            AxRect {
                x: 0,
                y: 0,
                width: 10,
                height: 0
            }
            .clamped_center(100, 100),
            None
        );
        assert_eq!(inside.clamped_center(0, 100), None);
        assert_eq!(inside.clamped_center(100, 0), None);
        assert_eq!(
            AxRect {
                x: 200,
                y: 0,
                width: 10,
                height: 10
            }
            .clamped_center(100, 100),
            None
        );
        assert_eq!(
            AxRect {
                x: -50,
                y: 0,
                width: 10,
                height: 10
            }
            .clamped_center(100, 100),
            None
        );
        // Starting exactly on the far edge clips to zero width, which is empty, not a
        // one-pixel sliver at the boundary.
        assert_eq!(
            AxRect {
                x: 100,
                y: 10,
                width: 10,
                height: 10
            }
            .clamped_center(100, 100),
            None
        );
        assert_eq!(
            AxRect {
                x: 10,
                y: 100,
                width: 10,
                height: 10
            }
            .clamped_center(100, 100),
            None
        );
    }

    /// Every arm of the role table, and the guarantee that the table covers `AxRole::ALL`:
    /// a variant added without a parse arm fails here rather than silently becoming
    /// unparseable. `Other` is deliberately outside `ALL` but still has a name.
    #[test]
    fn every_role_parses_from_its_name() {
        use AxRole::*;
        let pairs: [(&str, AxRole); 34] = [
            ("application", Application),
            ("window", Window),
            ("dialog", Dialog),
            ("group", Group),
            ("button", Button),
            ("togglebutton", ToggleButton),
            ("radiobutton", RadioButton),
            ("checkbox", CheckBox),
            ("menubar", MenuBar),
            ("menu", Menu),
            ("menuitem", MenuItem),
            ("label", Label),
            ("textfield", TextField),
            ("textarea", TextArea),
            ("combobox", ComboBox),
            ("list", List),
            ("listitem", ListItem),
            ("table", Table),
            ("cell", Cell),
            ("tree", Tree),
            ("treeitem", TreeItem),
            ("tablist", TabList),
            ("tab", Tab),
            ("scrollbar", ScrollBar),
            ("slider", Slider),
            ("spinbutton", SpinButton),
            ("progressbar", ProgressBar),
            ("image", Image),
            ("link", Link),
            ("separator", Separator),
            ("toolbar", Toolbar),
            ("statusbar", StatusBar),
            ("heading", Heading),
            ("other", Other),
        ];

        for (name, role) in pairs {
            assert_eq!(AxRole::from_name(name), Some(role), "{name}");
            // Documented as case-insensitive, so the folding is part of the contract.
            assert_eq!(
                AxRole::from_name(&name.to_ascii_uppercase()),
                Some(role),
                "{name} uppercased"
            );
        }

        for role in AxRole::ALL {
            assert!(
                pairs.iter().any(|&(_, r)| r == role),
                "{role:?} is in ALL but no name parses to it"
            );
        }

        assert_eq!(AxRole::from_name("nosuchrole"), None);
        assert_eq!(AxRole::from_name(""), None);
    }

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
            value: None,
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
            value: None,
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
            value: None,
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
            value: None,
        };
        assert!(t_nofp.bounds_consistent(Some(r), 8));
        assert!(t_nofp.bounds_consistent(None, 8));
    }

    #[test]
    fn ax_target_value_consistent_rejects_an_element_holding_other_data() {
        let with_value = |value: Option<&str>| AxTarget {
            id: AxNodeId(3),
            role: AxRole::TextField,
            name: None,
            bounds: None,
            value: value.map(Into::into),
        };
        let t = with_value(Some("Alice"));
        assert!(t.value_consistent(Some("Alice")));
        // A recycled row: same role, name and rect, different data → rejected.
        assert!(!t.value_consistent(Some("Zara")));
        // An emptied field reports no value at all, which is still a change.
        assert!(!t.value_consistent(None));
        // Nothing captured → nothing to verify, accept, or every element that never held a
        // value would be unwritable.
        assert!(with_value(None).value_consistent(Some("Alice")));
        assert!(with_value(None).value_consistent(None));
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
            description: None,
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
            description: None,
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
    fn find_mut_patches_the_node_in_place_without_touching_the_rest() {
        let mut t = sample_tree();
        t.assign_ids();
        t.find_mut(AxNodeId(2)).unwrap().value = Some("sibling".into());
        t.find_mut(AxNodeId(1)).unwrap().value = Some("patched".into());
        assert_eq!(
            t.find(AxNodeId(1)).unwrap().value.as_deref(),
            Some("patched")
        );
        // "the rest": the sibling keeps its own value, so a patch reaching past the one id it was
        // handed is caught rather than named away.
        assert_eq!(
            t.find(AxNodeId(2)).unwrap().value.as_deref(),
            Some("sibling")
        );
        assert!(t.find_mut(AxNodeId(99)).is_none());
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
            description: None,
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
            description: None,
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
            description: None,
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
    fn walklimits_default_matches_the_legacy_consts() {
        assert_eq!(WalkLimits::DEFAULT.nodes, MAX_NODES);
        assert_eq!(WalkLimits::DEFAULT.depth, MAX_DEPTH);
        assert_eq!(WalkLimits::DEFAULT.siblings, MAX_SIBLINGS);
    }

    #[test]
    fn from_max_nodes_controls_nodes_only_keeping_depth_and_sibling_rails() {
        assert_eq!(WalkLimits::from_max_nodes(None), WalkLimits::DEFAULT);
        // Some(0) lifts the node cap for the full tree, but depth/siblings keep their defaults
        // (structural safety rails against a cyclic/pathological native tree — see from_max_nodes).
        let unbounded = WalkLimits::from_max_nodes(Some(0));
        assert_eq!(unbounded.nodes, usize::MAX);
        assert_eq!(unbounded.depth, WalkLimits::DEFAULT.depth);
        assert_eq!(unbounded.siblings, WalkLimits::DEFAULT.siblings);
        // Some(n) caps nodes at n, depth/siblings default.
        let capped = WalkLimits::from_max_nodes(Some(42));
        assert_eq!(capped.nodes, 42);
        assert_eq!(capped.depth, WalkLimits::DEFAULT.depth);
        assert_eq!(capped.siblings, WalkLimits::DEFAULT.siblings);
    }

    #[test]
    fn with_limits_stops_nodes_at_the_configured_cap_not_the_default() {
        let mut b = WalkBudget::with_limits(WalkLimits::from_max_nodes(Some(3)));
        b.visit();
        b.visit();
        b.visit();
        assert!(b.nodes_exhausted(), "3 visits hits a cap of 3");
        assert_eq!(b.max_siblings(), MAX_SIBLINGS);
    }

    #[test]
    fn max_nodes_zero_lifts_the_node_cap_but_keeps_the_depth_rail() {
        let mut b = WalkBudget::with_limits(WalkLimits::from_max_nodes(Some(0)));
        // Visit well past the OLD node cap: under DEFAULT this would exhaust; a lifted node cap
        // must not, so this fails if `Some(0)` left `nodes` at MAX_NODES instead of lifting it.
        for _ in 0..(MAX_NODES + 10) {
            b.visit();
        }
        assert!(!b.nodes_exhausted(), "a lifted node cap never exhausts");
        // The depth rail is deliberately KEPT even under max_nodes:0 (cycle/stack-overflow guard).
        assert!(
            b.depth_exhausted(MAX_DEPTH),
            "the depth safety rail is preserved under a lifted node cap"
        );
    }

    #[test]
    fn a_complete_tree_discloses_nothing() {
        let t = AxTree::new(leaf(AxRole::Window, "w"));
        assert_eq!(t.unreadable_notice(), None);
        assert_eq!(t.truncation_notice(), None);
    }

    /// The bug this exists for — see [`AxTree::unreadable_notice`].
    #[test]
    fn an_unreadable_subtree_is_disclosed_even_when_no_bound_was_hit() {
        let mut t = AxTree::new(leaf(AxRole::Window, "w"));
        t.unreadable = 1;
        assert_eq!(t.truncated, None, "no bound fired");
        let n = t
            .unreadable_notice()
            .expect("an unreadable subtree must disclose");
        assert!(n.contains("1 subtree"), "{n}");
        assert!(n.contains("cannot be addressed by id"), "{n}");
        assert!(
            n.contains("glass_a11y_snapshot"),
            "retry is the recourse here: {n}"
        );
    }

    /// Singular and plural both read as English, since the count is agent-facing text.
    #[test]
    fn the_unreadable_notice_agrees_in_number() {
        let mut t = AxTree::new(leaf(AxRole::Window, "w"));
        t.unreadable = 1;
        let one = t.unreadable_notice().unwrap();
        assert!(one.contains("1 subtree could not be read and is"), "{one}");
        t.unreadable = 3;
        let many = t.unreadable_notice().unwrap();
        assert!(
            many.contains("3 subtrees could not be read and are"),
            "{many}"
        );
    }

    #[test]
    fn the_budget_counts_each_unreadable_subtree_separately_from_bounds() {
        let mut b = WalkBudget::new();
        assert_eq!(b.unreadable(), 0);
        b.note_unreadable();
        b.note_unreadable();
        assert_eq!(b.unreadable(), 2);
        assert_eq!(
            b.truncation(),
            None,
            "an unreadable read is not a bound hit"
        );
    }

    #[test]
    fn truncation_notice_states_elements_are_missing_and_names_the_pixel_fallback() {
        let n = Truncation {
            limit: TruncationLimit::Nodes,
            limit_value: MAX_NODES,
            nodes_walked: 1500,
        }
        .notice();
        assert!(
            n.contains("NOT shown") && n.contains("glass_screenshot"),
            "notice must be unmissable and steer to pixels: {n}"
        );
    }

    #[test]
    fn truncation_notice_reports_the_actual_runtime_cap_not_the_default() {
        // A raised/lowered cap that fires must render its OWN number, not the compile-time const.
        let mut b = WalkBudget::with_limits(WalkLimits::from_max_nodes(Some(42)));
        for _ in 0..42 {
            b.visit();
        }
        b.hit(TruncationLimit::Nodes);
        let notice = b.truncation().expect("hit recorded").notice();
        assert!(
            notice.contains("42 nodes"),
            "notice reports the runtime cap (42), not MAX_NODES: {notice}"
        );
        assert!(
            !notice.contains(&MAX_NODES.to_string()),
            "notice must not show the default cap when a custom one fired: {notice}"
        );
    }

    #[test]
    fn truncation_notice_is_some_when_the_walk_stopped_early() {
        // `to_outline` itself stays pure tree text (see its doc comment); the truncation
        // fact is surfaced separately via `truncation_notice` so a caller can never render
        // a truncated tree as if it were complete without actively dropping this `Some`.
        let mut t = sample_tree();
        t.assign_ids();
        t.truncated = Some(Truncation {
            limit: TruncationLimit::Depth,
            limit_value: MAX_DEPTH,
            nodes_walked: 42,
        });
        assert!(!t.to_outline().contains("truncated"), "pure tree text");
        let notice = t.truncation_notice().expect("a truncated tree has one");
        assert!(notice.contains("truncated"), "notice: {notice}");
    }

    #[test]
    fn truncation_notice_is_none_when_the_tree_is_complete() {
        let mut t = sample_tree();
        t.assign_ids();
        assert!(t.truncation_notice().is_none());
    }

    #[test]
    fn new_builds_a_complete_tree() {
        assert_eq!(AxTree::new(leaf(AxRole::Window, "App")).truncated, None);
    }

    #[test]
    fn a_tree_that_answered_for_another_subject_says_which() {
        let mut t = AxTree::new(leaf(AxRole::Window, "w"));
        t.subject = Some(Subject {
            asked: "com.example.app".into(),
            actual: "com.google.android.permissioncontroller".into(),
        });
        let notice = t
            .subject_notice()
            .expect("a mismatch owes the caller a notice");
        // Pin the order, not just presence: a swapped `actual`/`asked` pair would still
        // contain both strings but invert the claim — telling the caller it's looking at
        // what it asked for while the ids actually address the other app.
        assert!(
            notice.contains(
                "describes com.google.android.permissioncontroller, not the com.example.app"
            ),
            "{notice}"
        );
    }

    #[test]
    fn a_tree_that_answered_for_what_was_asked_says_nothing() {
        assert_eq!(
            AxTree::new(leaf(AxRole::Window, "w")).subject_notice(),
            None
        );
    }

    #[test]
    fn invoke_default_is_unsupported() {
        struct NoInvoke;
        impl Accessibility for NoInvoke {
            fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
                unreachable!("not exercised")
            }
        }
        let ctx = AxContext {
            pids: vec![],
            window: crate::platform::WindowGeometry {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
        };
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::Button,
            name: None,
            bounds: None,
            value: None,
        };
        assert!(matches!(
            NoInvoke.invoke(&ctx, &target),
            Err(crate::error::GlassError::AxUnsupported)
        ));
    }

    #[test]
    fn click_method_labels_and_fallback_access() {
        let n = ClickMethod::NativeAction { actuated: None };
        assert_eq!(n.label(), "native-action");
        assert_eq!(n.native_fallback(), None);
        assert_eq!(n.actuated(), None);
        let p = ClickMethod::Pointer {
            native_fallback: "reason".into(),
        };
        assert_eq!(p.label(), "pointer");
        assert_eq!(p.native_fallback(), Some("reason"));
        assert_eq!(p.actuated(), None);
    }

    #[test]
    fn a_native_action_on_a_substituted_element_reports_which_one() {
        // The caller named #4 and the backend actuated the control enclosing it. Reporting
        // only "native-action" would leave "I clicked the label" and "I clicked the row
        // around it, which navigated away" indistinguishable afterwards.
        let m = ClickMethod::NativeAction {
            actuated: Some(AxNodeId(2)),
        };
        assert_eq!(m.actuated(), Some(AxNodeId(2)));
        assert_eq!(m.label(), "native-action");
    }

    #[test]
    fn normalize_description_drops_what_adds_nothing() {
        // Empty and whitespace-only are "the platform exposed no description".
        assert_eq!(normalize_description("", None), None);
        assert_eq!(normalize_description("   ", None), None);
        // A description identical to the name would print the same label twice per line.
        assert_eq!(normalize_description("Save", Some("Save")), None);
        // Both sides are trimmed, and the name side is reachable: the readers' `nonempty`
        // helper does not trim, so a name can arrive as `Some("Save ")`.
        assert_eq!(normalize_description("  Save  ", Some("Save")), None);
        assert_eq!(normalize_description("Save", Some("  Save  ")), None);
    }

    #[test]
    fn normalize_description_keeps_an_informative_value() {
        assert_eq!(
            normalize_description("  Saves and closes  ", Some("Save")),
            Some("Saves and closes".to_string())
        );
        // With no name, any non-empty description is the only label the node has.
        assert_eq!(
            normalize_description("Bold", None),
            Some("Bold".to_string())
        );
        // Case and inner spacing are the platform's; only exact duplicates are dropped.
        assert_eq!(
            normalize_description("save", Some("Save")),
            Some("save".to_string())
        );
    }

    /// One slot per `ElementCondition`. Deliberately not `ElementCondition::ALL.len()`: sizing the
    /// check off the array under test lets an entry dropped from the *end* shrink the check along
    /// with it and pass.
    const CONDITION_COUNT: usize = 13;

    /// Adding an `ElementCondition` fails to compile in this match, which is where it is given its
    /// slot in [`ElementCondition::ALL`]. The test below then catches an entry dropped from that
    /// array or listed twice; nothing can force a *new* condition into it, since no test can
    /// enumerate an enum. That is worth care because `ALL` is what the Windows a11y crate's
    /// coverage test iterates, so a condition missing from it is one that check silently skips.
    const fn condition_index(c: ElementCondition) -> usize {
        match c {
            ElementCondition::Appears => 0,
            ElementCondition::Disappears => 1,
            ElementCondition::Enabled => 2,
            ElementCondition::Disabled => 3,
            ElementCondition::Checked => 4,
            ElementCondition::Unchecked => 5,
            ElementCondition::Selected => 6,
            ElementCondition::Unselected => 7,
            ElementCondition::Expanded => 8,
            ElementCondition::Collapsed => 9,
            ElementCondition::Focused => 10,
            ElementCondition::Visible => 11,
            ElementCondition::Hidden => 12,
        }
    }

    #[test]
    fn all_lists_every_condition_exactly_once() {
        let mut seen = [false; CONDITION_COUNT];
        for c in ElementCondition::ALL {
            let i = condition_index(c);
            assert!(!seen[i], "{c:?} appears twice in ElementCondition::ALL");
            seen[i] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "ElementCondition::ALL is missing a condition"
        );
    }
}
