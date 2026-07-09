//! Pure mapping from idb's `accessibility_info` (nested) JSON into glass's
//! [`AxTree`]. idb reports frames in logical points; we scale them to
//! window-relative pixels (`px = pt * scale`) so accessibility bounds share the
//! capture frame's coordinate space (the simulator's backing scale is ×3).
//!
//! Field names below are the external contract, taken verbatim from real
//! `idb ui describe-all --nested --json` output (`tests/fixtures/describe_nested.json`):
//! - role in `role` (AX-prefixed, e.g. `AXButton`; the sibling `type` field holds the
//!   un-prefixed form `Button` and is not used here),
//! - stable id in `AXUniqueId`, display label in `AXLabel`, value in `AXValue`.
//!   The id becomes the node `name` when present; a non-editable element's `AXLabel`
//!   is then surfaced as its `value` so the visible text is not lost,
//! - frame in the structured `frame` object `{x, y, width, height}` — note the
//!   sibling `AXFrame` is a *stringified* CGRect (`"{{x, y}, {w, h}}"`), so the
//!   structured `frame` is the one we read,
//! - `enabled` bool, and nested elements in `children`.
//!
//! idb's `accessibility_info` exposes no per-element focus state (there is no focus
//! key anywhere in the output), so [`AxStates::focused`] is always false from this
//! backend — a known limitation.
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

/// The widest top-level element's logical-point width from idb's nested
/// `accessibility_info` JSON — the describe root's `frame.width`. This is the point
/// counterpart to the capture frame's pixel width, so dividing the two yields the
/// device's point→pixel scale (`scale = pixel_width / point_width`).
///
/// Reads the structured `frame` object, matching [`build_tree`]: the sibling `AXFrame`
/// is a stringified CGRect, not a number, so it is deliberately ignored. Returns `None`
/// when the JSON does not parse, carries no top-level element, or no element has a
/// numeric `frame.width` — the caller treats that as "scale undetermined" rather than
/// assuming a default.
pub fn root_point_width(json: &str) -> Option<f64> {
    fn frame_width(n: &Value) -> Option<f64> {
        n.get("frame")?.get("width")?.as_f64()
    }
    let v: Value = serde_json::from_str(json).ok()?;
    match v {
        Value::Array(a) => a.iter().filter_map(frame_width).reduce(f64::max),
        obj @ Value::Object(_) => frame_width(&obj),
        _ => None,
    }
}

fn map_node(n: &Value, scale: f64) -> AxNode {
    // Read a string field, collapsing both a JSON `null` (missing/non-string) and an
    // empty string to `None` so absent and blank values are treated alike.
    let s = |k: &str| {
        n.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let ax_type = s("role").unwrap_or_default();
    let role = ax_role(&ax_type);
    let editable = matches!(role, AxRole::TextField | AxRole::TextArea);
    let uid = s("AXUniqueId");
    let label = s("AXLabel");
    // Prefer the stable identifier for semantic addressing; fall back to the label.
    let name = uid.clone().or_else(|| label.clone());
    // An editable element's value is its text content (`AXValue`). A non-editable
    // element whose stable id displaced its visible label out of `name` surfaces that
    // label as the value instead, so its text stays observable — e.g. a status line
    // whose text flips (READY→TAPPED) lives in `AXLabel`, not `AXValue`. With no id the
    // label already is the name, so there is nothing left to surface.
    let value = if editable {
        s("AXValue")
    } else if uid.is_some() {
        label
    } else {
        None
    };
    let states = AxStates {
        enabled: n.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        visible: true,
        // idb's accessibility_info exposes no per-element focus state, so `focused`
        // is always false here (a known limitation of this backend).
        focused: false,
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
        value,
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
    // Round to the nearest pixel before casting: `as` truncates toward zero, which
    // loses a pixel on fractional-point frames (e.g. 145.333pt × 3 = 435.9999… would
    // truncate to 435 instead of 436).
    Some(AxRect {
        x: (x * scale).round() as i32,
        y: (y * scale).round() as i32,
        width: (w * scale).round().max(0.0) as u32,
        height: (h * scale).round().max(0.0) as u32,
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
    fn build_tree_rounds_fractional_point_frames_to_nearest_pixel() {
        let tree = built();
        // tapButton logical frame x=145.33333…, y=404, w=111.66666…, h=47.66666…; ×3 gives
        // 435.9999…/1212/335.0/143.0. Truncation would drop x to 435 and w to 334 — rounding
        // must land x=436 and w=335. This is the case exact-integer frames sidestep.
        let button = find_by_name(&tree.root, "tapButton").expect("tapButton present");
        let b = button.bounds.expect("tapButton has bounds");
        assert_eq!(b.x, 436);
        assert_eq!(b.y, 1212);
        assert_eq!(b.width, 335);
        assert_eq!(b.height, 143);
    }

    #[test]
    fn build_tree_surfaces_a_static_labels_text_as_value_when_the_id_shadows_it() {
        let tree = built();
        // statusLabel carries both a stable id ("statusLabel", used as the name) and a
        // visible label ("READY"); the label is surfaced as the value so a caller can
        // observe its text (and any later flip) rather than losing it behind the id.
        let status = find_by_name(&tree.root, "statusLabel").expect("statusLabel present");
        assert_eq!(status.value.as_deref(), Some("READY"));
    }

    #[test]
    fn build_tree_leaves_value_empty_when_the_label_is_the_name() {
        let tree = built();
        // The application node has a null AXUniqueId, so its label became the name; the
        // label is not also duplicated into the value.
        assert_eq!(tree.root.children[0].name.as_deref(), Some("Glass Fixture"));
        assert_eq!(tree.root.children[0].value, None);
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

    #[test]
    fn root_point_width_reads_widest_frame() {
        let j = r#"[{"frame":{"x":0,"y":0,"width":402,"height":874}}]"#;
        assert_eq!(root_point_width(j), Some(402.0));
    }

    #[test]
    fn root_point_width_picks_the_widest_top_level_element() {
        let j = r#"[{"frame":{"width":320}},{"frame":{"width":402}}]"#;
        assert_eq!(root_point_width(j), Some(402.0));
    }

    #[test]
    fn root_point_width_reads_a_single_object_root() {
        let j = r#"{"frame":{"width":390}}"#;
        assert_eq!(root_point_width(j), Some(390.0));
    }

    #[test]
    fn root_point_width_is_none_without_a_numeric_frame_width() {
        // `AXFrame` is a stringified CGRect, not a number, and there is no structured
        // `frame` here — so there is no usable width to read.
        let j = r#"[{"AXFrame":"{{0, 0}, {402, 874}}"}]"#;
        assert_eq!(root_point_width(j), None);
    }

    #[test]
    fn root_point_width_is_none_on_malformed_json() {
        assert_eq!(root_point_width("not json"), None);
    }

    #[test]
    fn root_point_width_matches_the_fixture_application_width() {
        // The real describe-all fixture's application root is 402 logical points wide.
        assert_eq!(root_point_width(FIXTURE), Some(402.0));
    }
}
