//! Pure mapping from idb's `accessibility_info` (nested) JSON into glass's
//! [`AxTree`]. idb reports frames in logical points; we scale them to
//! window-relative pixels (`px = pt * scale`) so accessibility bounds share the
//! capture frame's coordinate space (the simulator's backing scale is ×3).
//!
//! Field names below are the external contract, taken verbatim from real
//! `idb ui describe-all --nested --json` output (`tests/fixtures/describe_nested.json`):
//! - role in `role` (AX-prefixed, e.g. `AXButton`; the sibling `type` field holds the
//!   un-prefixed form `Button` and is not used here),
//! - stable id in `AXUniqueId`, display label in `AXLabel`, value in `AXValue`,
//! - frame in the structured `frame` object `{x, y, width, height}` — note the
//!   sibling `AXFrame` is a *stringified* CGRect (`"{{x, y}, {w, h}}"`), so the
//!   structured `frame` is the one we read,
//! - `enabled` bool, and nested elements in `children`.
use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTree};
use glass_core::{GlassError, Result, WindowGeometry};
use serde_json::Value;

/// Map an idb AX role string (e.g. `AXButton`) to a normalized [`AxRole`].
/// Unrecognized roles become [`AxRole::Other`]; the caller preserves the native
/// string in [`AxNode::raw_role`].
#[allow(dead_code)]
pub fn ax_role(ax_type: &str) -> AxRole {
    match ax_type {
        "AXButton" => AxRole::Button,
        "AXStaticText" | "AXText" => AxRole::Label,
        "AXTextField" | "AXSearchField" => AxRole::TextField,
        "AXTextView" => AxRole::TextArea,
        "AXImage" => AxRole::Image,
        "AXSwitch" | "AXToggle" => AxRole::ToggleButton,
        "AXCheckBox" => AxRole::CheckBox,
        "AXSlider" => AxRole::Slider,
        "AXLink" => AxRole::Link,
        "AXCell" => AxRole::Cell,
        "AXNavigationBar" | "AXToolbar" => AxRole::Toolbar,
        "AXTabBar" => AxRole::TabList,
        "AXApplication" => AxRole::Application,
        "AXWindow" => AxRole::Window,
        _ => AxRole::Other,
    }
}

/// Parse idb's nested accessibility JSON into an [`AxTree`]. Each element's
/// logical-point frame is converted to a window-relative pixel [`AxRect`] via
/// `scale`, and the top-level elements are wrapped under a synthetic
/// [`AxRole::Window`] root sized to `window`. Node ids are left `AxNodeId(0)`;
/// the caller runs [`AxTree::assign_ids`].
///
/// Returns [`GlassError::AccessibilityUnavailable`] if the JSON does not parse or
/// its root is neither an element object nor an array of elements — a malformed
/// response never yields an empty tree passed off as success.
#[allow(dead_code)]
pub fn build_tree(json: &str, scale: f64, window: &WindowGeometry) -> Result<AxTree> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| GlassError::AccessibilityUnavailable(format!("idb a11y JSON parse: {e}")))?;
    // idb may return either a single root object or an array of top-level elements.
    let children: Vec<AxNode> = match &v {
        Value::Array(a) => a.iter().map(|n| map_node(n, scale)).collect(),
        obj @ Value::Object(_) => vec![map_node(obj, scale)],
        _ => {
            return Err(GlassError::AccessibilityUnavailable(
                "idb a11y JSON: unexpected root".into(),
            ))
        }
    };
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "AXWindow".into(),
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
    Ok(AxTree { root, count: 0 })
}

fn map_node(n: &Value, scale: f64) -> AxNode {
    let s = |k: &str| {
        n.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let ax_type = s("role").unwrap_or_default();
    let role = ax_role(&ax_type);
    let editable = matches!(role, AxRole::TextField | AxRole::TextArea);
    // Prefer the stable identifier for semantic addressing; fall back to the label.
    let name = s("AXUniqueId").or_else(|| s("AXLabel"));
    let states = AxStates {
        enabled: n.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        visible: true,
        focused: n.get("AXFocused").and_then(Value::as_bool).unwrap_or(false),
        editable,
        ..AxStates::default()
    };
    let bounds = n.get("frame").and_then(|f| frame_to_rect(f, scale));
    let children = n
        .get("children")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|c| map_node(c, scale)).collect())
        .unwrap_or_default();
    AxNode {
        id: AxNodeId(0),
        role,
        raw_role: ax_type,
        name,
        value: if editable { s("AXValue") } else { None },
        states,
        bounds,
        children,
    }
}

/// Convert idb's structured `frame` object (logical points) to a pixel [`AxRect`]
/// via `scale`. `None` if any coordinate is missing or non-numeric.
fn frame_to_rect(f: &Value, scale: f64) -> Option<AxRect> {
    let g = |k: &str| f.get(k).and_then(Value::as_f64);
    let (x, y, w, h) = (g("x")?, g("y")?, g("width")?, g("height")?);
    Some(AxRect {
        x: (x * scale) as i32,
        y: (y * scale) as i32,
        width: (w * scale).max(0.0) as u32,
        height: (h * scale).max(0.0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::{AxNode, AxRole};
    use glass_core::WindowGeometry;

    const FIXTURE: &str = include_str!("../tests/fixtures/describe_nested.json");

    /// The fixture app runs full-screen on an iPhone 17 simulator: 402x874 logical
    /// points at ×3 backing scale => 1206x2622 pixels.
    const SCALE: f64 = 3.0;
    fn win() -> WindowGeometry {
        WindowGeometry {
            x: 0,
            y: 0,
            width: 1206,
            height: 2622,
        }
    }

    fn built() -> AxTree {
        let mut tree = build_tree(FIXTURE, SCALE, &win()).expect("fixture must parse");
        tree.assign_ids();
        tree
    }

    /// First node (pre-order) whose `name` equals `name`.
    fn find_by_name<'a>(n: &'a AxNode, name: &str) -> Option<&'a AxNode> {
        if n.name.as_deref() == Some(name) {
            return Some(n);
        }
        n.children.iter().find_map(|c| find_by_name(c, name))
    }

    #[test]
    fn role_mapping_covers_fixture_types_and_falls_back_to_other() {
        assert_eq!(ax_role("AXButton"), AxRole::Button);
        assert_eq!(ax_role("AXStaticText"), AxRole::Label);
        assert_eq!(ax_role("AXTextField"), AxRole::TextField);
        assert_eq!(ax_role("AXApplication"), AxRole::Application);
        assert_eq!(ax_role("AXImage"), AxRole::Image);
        assert_eq!(ax_role("AXWhatever"), AxRole::Other);
    }

    #[test]
    fn build_tree_wraps_elements_in_a_window_root_sized_to_geometry() {
        let tree = built();
        assert_eq!(tree.root.role, AxRole::Window);
        assert_eq!(
            tree.root.bounds,
            Some(AxRect {
                x: 0,
                y: 0,
                width: 1206,
                height: 2622,
            })
        );
        // The single top-level element (the application) hangs under the synthetic root.
        assert_eq!(tree.root.children.len(), 1);
        assert_eq!(tree.root.children[0].role, AxRole::Application);
    }

    #[test]
    fn build_tree_names_application_from_label_when_unique_id_is_null() {
        let tree = built();
        // The application node has a null AXUniqueId, so its name falls back to AXLabel.
        assert_eq!(tree.root.children[0].name.as_deref(), Some("Glass Fixture"));
    }

    #[test]
    fn build_tree_maps_each_element_to_its_role() {
        let tree = built();
        assert_eq!(
            find_by_name(&tree.root, "tapButton").map(|n| n.role),
            Some(AxRole::Button)
        );
        assert_eq!(
            find_by_name(&tree.root, "inputField").map(|n| n.role),
            Some(AxRole::TextField)
        );
        assert_eq!(
            find_by_name(&tree.root, "statusLabel").map(|n| n.role),
            Some(AxRole::Label)
        );
        assert_eq!(
            find_by_name(&tree.root, "echoLabel").map(|n| n.role),
            Some(AxRole::Label)
        );
    }

    #[test]
    fn build_tree_scales_point_frames_to_window_pixels() {
        let tree = built();
        // statusLabel logical frame is x=129 w=144; ×3 => x=387 w=432 (both exact).
        let status = find_by_name(&tree.root, "statusLabel").expect("statusLabel present");
        let b = status.bounds.expect("statusLabel has bounds");
        assert_eq!(b.x, 387);
        assert_eq!(b.width, 432);
    }

    #[test]
    fn build_tree_carries_editable_value_and_state_for_text_field() {
        let tree = built();
        let field = find_by_name(&tree.root, "inputField").expect("inputField present");
        assert_eq!(field.value.as_deref(), Some("type here"));
        assert!(field.states.editable);
    }

    #[test]
    fn build_tree_errors_on_malformed_json() {
        let err = build_tree("this is not json", SCALE, &win()).unwrap_err();
        assert!(matches!(err, GlassError::AccessibilityUnavailable(_)));
    }

    #[test]
    fn build_tree_errors_on_unexpected_root_shape() {
        // A bare scalar is neither an element object nor an array of elements.
        let err = build_tree("42", SCALE, &win()).unwrap_err();
        assert!(matches!(err, GlassError::AccessibilityUnavailable(_)));
    }
}
