//! Pure mapping from `uiautomator dump` XML into glass's normalized `AxTree`.

use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTree};
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
        "ScrollView" | "HorizontalScrollView" | "NestedScrollView" | "RecyclerView"
        | "ListView" | "GridView" => AxRole::List,
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
    let children = hierarchy
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "node")
        .map(|n| map_node(n, window))
        .collect();
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "hierarchy".into(),
        name: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect { x: 0, y: 0, width: window.width, height: window.height }),
        children,
    };
    Ok(AxTree { root, count: 0 })
}

fn map_node(node: roxmltree::Node, window: &WindowGeometry) -> AxNode {
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
        expanded: false,
        editable,
    };
    let children = node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "node")
        .map(|n| map_node(n, window))
        .collect();
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
        WindowGeometry { x: 0, y: 0, width: 1080, height: 2400 }
    }

    #[test]
    fn class_maps_to_roles() {
        assert_eq!(class_to_role("android.widget.Button"), AxRole::Button);
        assert_eq!(class_to_role("android.widget.EditText"), AxRole::TextField);
        assert_eq!(class_to_role("android.widget.TextView"), AxRole::Label);
        assert_eq!(class_to_role("android.widget.CheckBox"), AxRole::CheckBox);
        assert_eq!(class_to_role("androidx.recyclerview.widget.RecyclerView"), AxRole::List);
        assert_eq!(class_to_role("android.widget.FrameLayout"), AxRole::Group);
        assert_eq!(class_to_role("com.example.CustomThing"), AxRole::Other);
    }

    #[test]
    fn bounds_become_window_relative() {
        let win = WindowGeometry { x: 0, y: 63, width: 1080, height: 2337 };
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
        assert_eq!((kids[0].role, kids[0].name.as_deref()), (AxRole::Label, Some("Settings")));
        assert_eq!(kids[1].role, AxRole::TextField);
        assert_eq!(kids[1].name.as_deref(), Some("Email"));
        assert_eq!(kids[1].value.as_deref(), Some("joe@x.com"));
        assert!(kids[1].states.editable && kids[1].states.focused);
        assert_eq!((kids[2].role, kids[2].name.as_deref()), (AxRole::Button, Some("Save")));
        assert_eq!(kids[2].bounds.unwrap().width, 1000);
    }

    #[test]
    fn build_tree_rejects_non_hierarchy_xml() {
        assert!(matches!(
            build_tree("<other/>", &win()),
            Err(GlassError::AccessibilityUnavailable(_))
        ));
    }
}
