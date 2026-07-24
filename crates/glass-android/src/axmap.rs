//! Pure mapping from `uiautomator dump` XML into glass's normalized `AxTree`.

use glass_core::accessibility::{
    AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTree, TruncationLimit, WalkBudget, MAX_SIBLINGS,
};
use glass_core::{GlassError, Result, WindowGeometry};

/// Map an Android widget class (`android.widget.Button`, …) to a normalized role.
pub fn class_to_role(class: &str) -> AxRole {
    let leaf = class.rsplit('.').next().unwrap_or(class);
    match leaf {
        "Button" | "ImageButton" | "MaterialButton" | "AppCompatButton" => AxRole::Button,
        "EditText" | "AppCompatEditText" | "AutoCompleteTextView" | "TextInputEditText" => {
            AxRole::TextField
        }
        "CheckBox" | "AppCompatCheckBox" | "CheckedTextView" => AxRole::CheckBox,
        "RadioButton" | "AppCompatRadioButton" => AxRole::RadioButton,
        "Switch" | "SwitchCompat" | "SwitchMaterial" | "ToggleButton" | "CompoundButton" => {
            AxRole::ToggleButton
        }
        "TextView" | "AppCompatTextView" => AxRole::Label,
        "ImageView" | "AppCompatImageView" => AxRole::Image,
        "Spinner" => AxRole::ComboBox,
        "SeekBar" => AxRole::Slider,
        "ProgressBar" => AxRole::ProgressBar,
        "ScrollView"
        | "HorizontalScrollView"
        | "NestedScrollView"
        | "RecyclerView"
        | "ListView"
        | "GridView" => AxRole::List,
        "WebView" => AxRole::Group,
        other if other.ends_with("Layout") || other == "View" || other == "ViewGroup" => {
            AxRole::Group
        }
        _ => AxRole::Other,
    }
}

/// Parse uiautomator `bounds="[l,t][r,b]"` (screen-absolute) into a window-relative `AxRect`.
pub fn parse_bounds(s: &str, window: &WindowGeometry) -> Option<AxRect> {
    let s = s.trim().strip_prefix('[')?;
    let (lt, rest) = s.split_once(']')?;
    let (l, t) = lt.split_once(',')?;
    let rest = rest.strip_prefix('[')?;
    let (rb, _) = rest.split_once(']')?;
    let (r, b) = rb.split_once(',')?;
    let l: i32 = l.trim().parse().ok()?;
    let t: i32 = t.trim().parse().ok()?;
    let r: i32 = r.trim().parse().ok()?;
    let b: i32 = b.trim().parse().ok()?;
    Some(AxRect {
        x: l - window.x,
        y: t - window.y,
        width: (r - l).max(0) as u32,
        height: (b - t).max(0) as u32,
    })
}

/// Surface the `uiautomator dump` idle-state / error failure mode loudly.
pub fn check_dump_status(stdout: &str) -> Result<()> {
    if stdout.contains("ERROR") || stdout.contains("could not get idle state") {
        return Err(GlassError::AccessibilityUnavailable(format!(
            "uiautomator dump failed: {}",
            stdout.trim()
        )));
    }
    Ok(())
}

/// Parse uiautomator XML into an `AxTree`. Ids are left unset (the caller runs
/// `AxTree::assign_ids`). The hierarchy is wrapped in a synthetic `Window` root
/// sized to the app window.
pub fn build_tree(xml: &str, window: &WindowGeometry) -> Result<AxTree> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| GlassError::AccessibilityUnavailable(format!("uiautomator XML parse: {e}")))?;
    let hierarchy = doc.root_element();
    if hierarchy.tag_name().name() != "hierarchy" {
        return Err(GlassError::AccessibilityUnavailable(
            "uiautomator XML has no <hierarchy> root".into(),
        ));
    }
    let mut budget = WalkBudget::new();
    let mut children = Vec::new();
    for (i, n) in hierarchy
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "node")
        .enumerate()
    {
        // Checked before processing each child (not after) so a child that merely
        // completes the tree — the last one, pushing the count to MAX_NODES — doesn't
        // get mistaken for a child the walk declined to visit.
        if budget.nodes_exhausted() {
            budget.hit(TruncationLimit::Nodes);
            break;
        }
        if i >= MAX_SIBLINGS {
            budget.hit(TruncationLimit::Siblings);
            break;
        }
        children.push(map_node(n, window, 0, &mut budget));
    }
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "hierarchy".into(),
        name: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: window.width,
            height: window.height,
        }),
        children,
    };
    let mut tree = AxTree::new(root);
    tree.truncated = budget.truncation();
    Ok(tree)
}

fn map_node(
    node: roxmltree::Node,
    window: &WindowGeometry,
    depth: usize,
    budget: &mut WalkBudget,
) -> AxNode {
    budget.visit();
    let attr = |k: &str| node.attribute(k).unwrap_or("");
    let boolean = |k: &str| node.attribute(k) == Some("true");
    let class = attr("class");
    let role = class_to_role(class);
    let editable = role == AxRole::TextField || boolean("password");
    let content_desc = non_empty(attr("content-desc"));
    let text = non_empty(attr("text"));
    let name = content_desc.or_else(|| if editable { None } else { text.clone() });
    let value = if editable { text } else { None };
    let states = AxStates {
        focused: boolean("focused"),
        focusable: boolean("focusable"),
        enabled: boolean("enabled"),
        visible: true,
        selected: boolean("selected"),
        checked: boolean("checked"),
        // OR in `checked`: uiautomator on a Compose toggle can under-report
        // `checkable="false"` while `checked="true"` (see
        // `checkable_reflects_the_uiautomator_attribute`). OR-ing keeps a `checked="true"`
        // reading trustworthy (its `Checked` wait condition still matches) while a plain
        // non-toggle (`checkable="false" checked="false"`) still matches neither.
        checkable: boolean("checkable") || boolean("checked"),
        expanded: false,
        editable,
    };
    // Recursion is bounded by `budget` (depth, node count, siblings per level), so a
    // pathologically deep or wide device tree cannot blow the stack or the token budget.
    // The child list is resolved before either bound is consulted: a childless node must
    // never be reported truncated for declining to explore a list that was already empty.
    let child_nodes: Vec<roxmltree::Node> = node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "node")
        .collect();
    let children = if child_nodes.is_empty() {
        vec![]
    } else if budget.depth_exhausted(depth) {
        budget.hit(TruncationLimit::Depth);
        vec![]
    } else if budget.nodes_exhausted() {
        budget.hit(TruncationLimit::Nodes);
        vec![]
    } else {
        let mut out = Vec::new();
        for (i, n) in child_nodes.into_iter().enumerate() {
            // Checked before processing each child (not after) so the child that merely
            // completes the tree doesn't get mistaken for one the walk declined to visit.
            if budget.nodes_exhausted() {
                budget.hit(TruncationLimit::Nodes);
                break;
            }
            if i >= MAX_SIBLINGS {
                budget.hit(TruncationLimit::Siblings);
                break;
            }
            out.push(map_node(n, window, depth + 1, budget));
        }
        out
    };
    AxNode {
        id: AxNodeId(0),
        role,
        raw_role: class.to_string(),
        name,
        value,
        states,
        bounds: parse_bounds(attr("bounds"), window),
        children,
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::AxRole;
    use glass_core::{GlassError, WindowGeometry};

    const XML: &str = concat!(
        "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>",
        "<hierarchy rotation=\"0\">",
        "<node index=\"0\" text=\"\" class=\"android.widget.FrameLayout\" package=\"com.x\" ",
        "content-desc=\"\" enabled=\"true\" focusable=\"false\" focused=\"false\" selected=\"false\" ",
        "checkable=\"false\" checked=\"false\" password=\"false\" bounds=\"[0,0][1080,2400]\">",
        "<node index=\"0\" text=\"Settings\" class=\"android.widget.TextView\" package=\"com.x\" ",
        "content-desc=\"\" enabled=\"true\" focusable=\"false\" focused=\"false\" selected=\"false\" ",
        "checkable=\"false\" checked=\"false\" password=\"false\" bounds=\"[40,100][300,160]\" />",
        "<node index=\"1\" text=\"joe@x.com\" class=\"android.widget.EditText\" package=\"com.x\" ",
        "content-desc=\"Email\" enabled=\"true\" focusable=\"true\" focused=\"true\" selected=\"false\" ",
        "checkable=\"false\" checked=\"false\" password=\"false\" bounds=\"[40,200][1040,280]\" />",
        "<node index=\"2\" text=\"\" class=\"android.widget.Button\" package=\"com.x\" ",
        "content-desc=\"Save\" enabled=\"true\" focusable=\"true\" focused=\"false\" selected=\"false\" ",
        "checkable=\"false\" checked=\"false\" password=\"false\" bounds=\"[40,300][1040,380]\" />",
        "</node></hierarchy>",
    );

    fn win() -> WindowGeometry {
        WindowGeometry {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        }
    }

    #[test]
    fn class_maps_to_roles() {
        assert_eq!(class_to_role("android.widget.Button"), AxRole::Button);
        assert_eq!(class_to_role("android.widget.EditText"), AxRole::TextField);
        assert_eq!(class_to_role("android.widget.TextView"), AxRole::Label);
        assert_eq!(class_to_role("android.widget.CheckBox"), AxRole::CheckBox);
        assert_eq!(
            class_to_role("androidx.recyclerview.widget.RecyclerView"),
            AxRole::List
        );
        assert_eq!(class_to_role("android.widget.FrameLayout"), AxRole::Group);
        assert_eq!(class_to_role("com.example.CustomThing"), AxRole::Other);
    }

    #[test]
    fn bounds_become_window_relative() {
        let win = WindowGeometry {
            x: 0,
            y: 63,
            width: 1080,
            height: 2337,
        };
        let r = parse_bounds("[40,100][300,160]", &win).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (40, 37, 260, 60));
        assert!(parse_bounds("garbage", &win).is_none());
    }

    #[test]
    fn dump_status_detects_idle_failure() {
        assert!(check_dump_status("UI hierchary dumped to: /sdcard/glass_dump.xml").is_ok());
        let err = check_dump_status("ERROR: could not get idle state!").unwrap_err();
        assert!(matches!(err, GlassError::AccessibilityUnavailable(_)));
    }

    #[test]
    fn build_tree_shapes_the_hierarchy() {
        let mut tree = build_tree(XML, &win()).unwrap();
        tree.assign_ids();
        assert_eq!(tree.root.role, AxRole::Window);
        let frame = &tree.root.children[0];
        assert_eq!(frame.role, AxRole::Group);
        let kids = &frame.children;
        assert_eq!(kids.len(), 3);
        assert_eq!(
            (kids[0].role, kids[0].name.as_deref()),
            (AxRole::Label, Some("Settings"))
        );
        assert_eq!(kids[1].role, AxRole::TextField);
        assert_eq!(kids[1].name.as_deref(), Some("Email"));
        assert_eq!(kids[1].value.as_deref(), Some("joe@x.com"));
        assert!(kids[1].states.editable && kids[1].states.focused);
        assert_eq!(
            (kids[2].role, kids[2].name.as_deref()),
            (AxRole::Button, Some("Save"))
        );
        assert_eq!(kids[2].bounds.unwrap().width, 1000);
    }

    /// A `<hierarchy>` with `n` flat top-level `<node>` elements, each a distinctly-named Button.
    fn wide_hierarchy_xml(n: usize) -> String {
        let mut kids = String::new();
        for i in 0..n {
            let top = i;
            let bottom = i + 10;
            kids.push_str(&format!(
                r#"<node text="btn{i}" class="android.widget.Button" bounds="[0,{top}][10,{bottom}]" />"#
            ));
        }
        format!(r#"<?xml version='1.0'?><hierarchy rotation="0">{kids}</hierarchy>"#)
    }

    #[test]
    fn truncation_stops_the_walk_and_never_shifts_surviving_ids() {
        // ids are assigned in the same pre-order the tree is walked, so a truncation that
        // dropped a node from the middle rather than stopping at the end would shift every
        // later id off the node its own index implies.
        let xml = wide_hierarchy_xml(glass_core::MAX_NODES + 50);
        let mut tree = build_tree(&xml, &win()).unwrap();
        tree.assign_ids();

        assert!(tree.truncated.is_some(), "the node cap must have been hit");
        // The synthetic Window root is id 0 (never counted against the budget); top-level
        // child at array index i is id i+1.
        let third = tree.find(AxNodeId(3)).expect("id 3 survives");
        assert_eq!(third.name.as_deref(), Some("btn2"));
    }

    #[test]
    fn a_complete_tree_of_exactly_max_nodes_reports_no_truncation() {
        // MAX_NODES flat top-level nodes: the walk visits every one of them, and the LAST
        // one is what pushes the running count to MAX_NODES. Nothing was declined, so this
        // must NOT be reported truncated (regression for the false-positive-at-the-cap bug).
        let xml = wide_hierarchy_xml(glass_core::MAX_NODES);
        let tree = build_tree(&xml, &win()).unwrap();
        assert_eq!(tree.truncated, None);
    }

    #[test]
    fn a_tree_of_max_nodes_plus_one_still_reports_nodes_truncation() {
        // One more node than the complete case above: now there really is a node the walk
        // declines to visit, so the cap must still fire — proving the fix didn't just
        // disable it.
        let xml = wide_hierarchy_xml(glass_core::MAX_NODES + 1);
        let tree = build_tree(&xml, &win()).unwrap();
        assert_eq!(
            tree.truncated.map(|t| t.limit),
            Some(TruncationLimit::Nodes)
        );
    }

    #[test]
    fn a_childless_node_at_the_spent_node_budget_records_no_truncation() {
        // A leaf with no <node> children, reached once the node budget is already spent,
        // must not be reported truncated merely for declining to explore an empty list.
        let doc = roxmltree::Document::parse(
            r#"<?xml version='1.0'?><node class="android.widget.TextView" text="leaf" bounds="[0,0][10,10]" />"#,
        )
        .unwrap();
        let mut budget = WalkBudget::new();
        for _ in 0..glass_core::MAX_NODES {
            budget.visit();
        }
        let _ = map_node(doc.root_element(), &win(), 0, &mut budget);
        assert!(budget.truncation().is_none());
    }

    #[test]
    fn build_tree_rejects_non_hierarchy_xml() {
        assert!(matches!(
            build_tree("<other/>", &win()),
            Err(GlassError::AccessibilityUnavailable(_))
        ));
    }

    #[test]
    fn checkable_reflects_the_uiautomator_attribute() {
        // A Switch reported NOT checkable (Compose gap) vs a real checkable Switch vs a
        // Compose-ON switch that under-reports checkable="false" while checked="true".
        let xml = concat!(
            "<?xml version='1.0'?><hierarchy rotation=\"0\">",
            "<node index=\"0\" text=\"\" class=\"android.widget.Switch\" content-desc=\"NotReal\" ",
            "enabled=\"true\" focusable=\"true\" focused=\"false\" selected=\"false\" ",
            "checkable=\"false\" checked=\"false\" password=\"false\" bounds=\"[0,0][100,50]\" />",
            "<node index=\"1\" text=\"\" class=\"android.widget.Switch\" content-desc=\"RealOn\" ",
            "enabled=\"true\" focusable=\"true\" focused=\"false\" selected=\"false\" ",
            "checkable=\"true\" checked=\"true\" password=\"false\" bounds=\"[0,60][100,110]\" />",
            "<node index=\"2\" text=\"\" class=\"android.widget.Switch\" content-desc=\"ComposeOn\" ",
            "enabled=\"true\" focusable=\"true\" focused=\"false\" selected=\"false\" ",
            "checkable=\"false\" checked=\"true\" password=\"false\" bounds=\"[0,120][100,170]\" />",
            "</hierarchy>",
        );
        let w = WindowGeometry {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        };
        let tree = build_tree(xml, &w).unwrap();
        let not_real = &tree.root.children[0];
        let real_on = &tree.root.children[1];
        let compose_on = &tree.root.children[2];
        assert!(
            !not_real.states.checkable,
            "checkable=false attr must map to checkable=false"
        );
        assert!(
            real_on.states.checkable && real_on.states.checked,
            "real Switch → checkable+checked"
        );
        assert!(
            compose_on.states.checkable && compose_on.states.checked,
            "checkable=false checked=true (Compose under-report) must still map to \
             checkable=true so the Checked condition matches"
        );
    }
}
