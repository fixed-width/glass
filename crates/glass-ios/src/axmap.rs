//! Pure mapping from idb's `accessibility_info` (nested) JSON into glass's
//! [`AxTree`]. idb reports frames in logical points; we scale them to
//! window-relative pixels (`px = pt * scale`) so accessibility bounds share the
//! capture frame's coordinate space (the simulator's backing scale is ×3).
//!
//! Field names below are the external contract, taken verbatim from real
//! `idb ui describe-all --nested --json` output (`tests/fixtures/describe_nested.json`):
//! - role in `role` (AX-prefixed, e.g. `AXButton`; the sibling `type` field holds the
//!   un-prefixed form `Button` and is not used here), and its `subrole`, present on every node and
//!   null where there is none — a `UISwitch` is the case that carries one (`AXSwitch`),
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
use glass_core::accessibility::{
    AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTree, WalkBudget, WalkLimits,
    normalize_description, normalize_name,
};
use glass_core::{GlassError, Result, WindowGeometry};
use serde_json::Value;

/// Every iOS role string glass maps, as reported through `idb`.
pub const ROLE_TOKENS: &[(&str, AxRole)] = &[
    ("AXButton", AxRole::Button),
    ("AXStaticText", AxRole::Label),
    ("AXText", AxRole::Label),
    ("AXTextField", AxRole::TextField),
    ("AXSearchField", AxRole::TextField),
    ("AXTextView", AxRole::TextArea),
    ("AXImage", AxRole::Image),
    ("AXCheckBox", AxRole::CheckBox),
    ("AXSlider", AxRole::Slider),
    ("AXLink", AxRole::Link),
    ("AXCell", AxRole::Cell),
    ("AXNavigationBar", AxRole::Toolbar),
    ("AXToolbar", AxRole::Toolbar),
    ("AXTabBar", AxRole::TabList),
    ("AXApplication", AxRole::Application),
    ("AXWindow", AxRole::Window),
    // A content container, and the only structural token most probed screens exposed:
    // navigation bars, tab bars and collections all arrived as AXGroup, named by the element's
    // identifier rather than by its role. That is what was observed, not a rule about UIKit —
    // an app that does report AXNavigationBar or AXTabBar still maps to Toolbar or TabList
    // through the rows above.
    ("AXGroup", AxRole::Group),
    // A screen or section title.
    ("AXHeading", AxRole::Heading),
];

/// Map an idb AX role string (e.g. `AXButton`) to a normalized [`AxRole`].
/// Unrecognized roles become [`AxRole::Other`]; the caller preserves the native
/// string in [`AxNode::raw_role`].
pub fn ax_role(role: &str) -> AxRole {
    ROLE_TOKENS
        .iter()
        .find(|(token, _)| *token == role)
        .map(|(_, r)| *r)
        .unwrap_or(AxRole::Other)
}

/// Subroles that decide a role on their own, whatever base role carries them.
///
/// Subroles that decide a role, and the base roles that can carry one.
///
/// A `UISwitch` reports `role=AXCheckBox subrole=AXSwitch` (measured on iOS 26.5). Matched against
/// the base roles known to carry it, so one stray attribute cannot re-role an unrelated node.
pub const SUBROLE_TOKENS: &[(&str, AxRole, &[&str])] =
    &[("AXSwitch", AxRole::ToggleButton, &["AXCheckBox"])];

/// [`ax_role`], but a subrole in [`SUBROLE_TOKENS`] wins over the base role.
pub fn ax_role_with_subrole(role: &str, subrole: Option<&str>) -> AxRole {
    subrole
        .and_then(|sub| {
            SUBROLE_TOKENS
                .iter()
                .find(|(token, _, bases)| *token == sub && bases.contains(&role))
                .map(|(_, r, _)| *r)
        })
        .unwrap_or_else(|| ax_role(role))
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
///
/// The walk is bounded by a [`WalkBudget`] (node count, nesting depth, and per-level
/// sibling scan) shared with every other backend; a bound firing stops the walk rather
/// than dropping nodes out of the middle, so ids assigned afterward by
/// [`AxTree::assign_ids`] stay stable for every surviving node. [`AxTree::truncated`]
/// records which bound fired, if any.
pub fn build_tree(
    json: &str,
    scale: f64,
    window: &WindowGeometry,
    limits: WalkLimits,
) -> Result<AxTree> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| GlassError::AccessibilityUnavailable(format!("idb a11y JSON parse: {e}")))?;
    let mut budget = WalkBudget::with_limits(limits);
    // idb may return either a single root object or an array of top-level elements.
    let children: Vec<AxNode> = match &v {
        Value::Array(a) => {
            let mut out = Vec::new();
            for (i, n) in a.iter().enumerate() {
                if !budget.may_visit_sibling(i) {
                    break;
                }
                out.push(map_node(n, scale, 0, &mut budget));
            }
            out
        }
        obj @ Value::Object(_) => vec![map_node(obj, scale, 0, &mut budget)],
        _ => {
            return Err(GlassError::AccessibilityUnavailable(
                "idb a11y JSON: unexpected root".into(),
            ));
        }
    };
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "AXWindow".into(),
        name: None,
        description: None,
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

/// iOS `(checkable, checked)` from the normalized role and idb's `AXValue` string. A `UISwitch`
/// reports `role=AXCheckBox` with `subrole=AXSwitch` — normalized to `ToggleButton` by
/// [`ax_role_with_subrole`] — and `AXValue` `"0"`/`"1"`. Claims `checkable` ONLY for a
/// determinate "0"/"1" on a checkable role (the #170 invariant); anything else → (false, false).
fn checkable_checked(role: AxRole, ax_value: Option<&str>) -> (bool, bool) {
    match role {
        AxRole::CheckBox | AxRole::ToggleButton => match ax_value {
            Some("1") => (true, true),
            Some("0") => (true, false),
            _ => (false, false),
        },
        _ => (false, false),
    }
}

fn map_node(n: &Value, scale: f64, depth: usize, budget: &mut WalkBudget) -> AxNode {
    budget.visit();
    // Read a string field, collapsing both a JSON `null` (missing/non-string) and an
    // empty string to `None` so absent and blank values are treated alike.
    let s = |k: &str| {
        n.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let ax_type = s("role").unwrap_or_default();
    // `subrole` is a real key in idb's node schema — present on every node, null where there is
    // none — so this is a plain field read, not an extra round-trip.
    let role = ax_role_with_subrole(&ax_type, s("subrole").as_deref());
    let editable = matches!(role, AxRole::TextField | AxRole::TextArea);
    let uid = s("AXUniqueId").and_then(|value| normalize_name(&value));
    let label = s("AXLabel").and_then(|value| normalize_name(&value));
    // Prefer the stable identifier for semantic addressing; fall back to the label.
    let name = uid.clone().or_else(|| label.clone());
    // Editable → value is `AXValue`; non-editable with a uid → the uid already displaced the
    // label out of `name`, so the label becomes `value` instead (e.g. a status line's
    // READY→TAPPED lives in `AXLabel`, never `AXValue`). Editable-with-a-uid is the mirror
    // case: the label has nowhere to go but `description` (`orphan_label`). One `match`, not
    // two sequential `if`s, so the compiler sees the label-moving arms are disjoint and skips
    // a clone.
    let (value, orphan_label) = match (editable, uid.is_some()) {
        (true, true) => (s("AXValue"), label),
        (true, false) => (s("AXValue"), None),
        (false, true) => (label, None),
        (false, false) => (None, None),
    };
    // `help` is idb's key for the element's accessibility hint. Each candidate normalizes on its
    // own: picking first and normalizing after would let a hint rejected as a duplicate of the
    // name take the displaced label down with it, losing the label from every field.
    let description = s("help")
        .and_then(|hint| normalize_description(&hint, name.as_deref()))
        .or_else(|| orphan_label.and_then(|label| normalize_description(&label, name.as_deref())));
    // `checkable`/`checked` from the switch's AXValue (see `checkable_checked`). idb exposes no
    // per-element focus state, so `focused` stays false — a real limitation of this backend.
    let (checkable, checked) = checkable_checked(role, s("AXValue").as_deref());
    let states = AxStates {
        enabled: n.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        visible: true,
        focused: false,
        editable,
        checkable,
        checked,
        ..AxStates::default()
    };
    let bounds = n.get("frame").and_then(|f| frame_to_rect(f, scale));
    // Recursion is bounded by `budget` (depth, node count, siblings per level), so a
    // pathologically deep or wide accessibility tree cannot blow the stack or the token
    // budget. The child array is resolved before either bound is consulted: a childless
    // node must never be reported truncated for declining to explore a list that was
    // already empty.
    let children = match n.get("children").and_then(Value::as_array) {
        None => vec![],
        Some(arr) if arr.is_empty() => vec![],
        Some(_) if !budget.may_explore_children(depth) => vec![],
        Some(kids) => {
            let mut out = Vec::new();
            for (i, c) in kids.iter().enumerate() {
                if !budget.may_visit_sibling(i) {
                    break;
                }
                out.push(map_node(c, scale, depth + 1, budget));
            }
            out
        }
    };
    AxNode {
        id: AxNodeId(0),
        role,
        raw_role: ax_type,
        name,
        description,
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
    use glass_core::accessibility::{AxNode, AxRole, TruncationLimit};
    use glass_core::{DescribedSample, WindowGeometry};

    const FIXTURE: &str = include_str!("../tests/fixtures/describe_nested.json");
    const HINT_FIXTURE: &str = include_str!("../tests/fixtures/describe_hint.json");

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
        let mut tree =
            build_tree(FIXTURE, SCALE, &win(), WalkLimits::DEFAULT).expect("fixture must parse");
        tree.assign_ids();
        tree
    }

    /// The lone top-level element of a single-node JSON literal, built and id-assigned.
    fn only_node(json: &str) -> AxNode {
        let tree = build_tree(json, 1.0, &win(), WalkLimits::DEFAULT).unwrap();
        tree.root.children[0].clone()
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
        // The checkable roles the toggle-state derivation depends on.
        assert_eq!(ax_role("AXCheckBox"), AxRole::CheckBox);
        assert_eq!(ax_role("AXWhatever"), AxRole::Other);
    }

    /// Read on the iOS 26.5 Simulator (2026-08-24, companions 1.5.0b3 and 1.1.8): a WKWebView
    /// page exposes no element through idb — not the page, not an empty web area, not the view
    /// itself. `AXWebArea` came from the macOS token table and was never seen here.
    #[test]
    fn no_token_stands_for_a_web_page_on_this_backend() {
        assert_eq!(ax_role("AXWebArea"), AxRole::Other);
    }

    #[test]
    fn an_empty_tree_maps_to_an_empty_window_rather_than_an_error() {
        // What an app reports for the second or so it takes to render. `IosA11y` relies on
        // this arriving as an empty tree: an error here would abort `wait_for_element`'s
        // poll, which treats a tick error as fatal, so an agent could not wait it out.
        let tree = build_tree("[]", 3.0, &win(), WalkLimits::DEFAULT)
            .expect("an unrendered app is an empty tree, not a failure");
        assert!(tree.root.children.is_empty());
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
    fn a_hint_becomes_the_description() {
        let node = only_node(
            r#"{"role":"AXButton","AXUniqueId":"bold","AXLabel":"Bold",
                "help":"Makes the selection bold","frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.name.as_deref(), Some("bold"));
        assert_eq!(
            node.description.as_deref(),
            Some("Makes the selection bold")
        );
    }

    #[test]
    fn an_editable_elements_displaced_label_becomes_the_description() {
        // uid takes the name and AXValue takes the value, so the label has nowhere else to go.
        let node = only_node(
            r#"{"role":"AXTextField","AXUniqueId":"query","AXLabel":"Search query",
                "AXValue":"typed text","frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.name.as_deref(), Some("query"));
        assert_eq!(node.value.as_deref(), Some("typed text"));
        assert_eq!(node.description.as_deref(), Some("Search query"));
    }

    #[test]
    fn a_non_editable_elements_label_stays_the_value_and_is_not_repeated() {
        // Already surfaced as `value` (the READY→TAPPED case); duplicating it into
        // `description` would print it twice.
        let node = only_node(
            r#"{"role":"AXStaticText","AXUniqueId":"status","AXLabel":"READY",
                "frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.value.as_deref(), Some("READY"));
        assert_eq!(node.description, None);
    }

    #[test]
    fn a_label_that_became_the_name_is_not_a_description() {
        let node = only_node(
            r#"{"role":"AXButton","AXLabel":"Tap Me","frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.name.as_deref(), Some("Tap Me"));
        assert_eq!(node.description, None);
    }

    #[test]
    fn a_hint_identical_to_the_name_is_not_a_description() {
        // Would leak through as a duplicate without `normalize_description`.
        let node = only_node(
            r#"{"role":"AXButton","AXUniqueId":"ok","AXLabel":"OK",
                "help":"ok","frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.name.as_deref(), Some("ok"));
        assert_eq!(node.description, None);
    }

    #[test]
    fn a_hint_rejected_as_a_duplicate_name_leaves_the_displaced_label_describing_the_node() {
        // Editable + uid, so the label's only home is `description` — and the hint is dropped as
        // a duplicate of the name. Normalizing before choosing would take the label with it and
        // "Search query" would appear in no field at all.
        let node = only_node(
            r#"{"role":"AXTextField","AXUniqueId":"query","AXLabel":"Search query",
                "AXValue":"typed text","help":"query",
                "frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.description.as_deref(), Some("Search query"));
    }

    #[test]
    fn a_hint_wins_over_a_displaced_label_when_a_node_carries_both() {
        // Four distinct labels, so the assertion fails if the fallback order ever flips.
        let node = only_node(
            r#"{"role":"AXTextField","AXUniqueId":"query","AXLabel":"Search query",
                "AXValue":"typed text","help":"Filters the list as you type",
                "frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(
            node.description.as_deref(),
            Some("Filters the list as you type")
        );
    }

    #[test]
    fn a_hint_describes_a_node_that_carries_no_unique_id() {
        // The commonest hint-bearing node in a real app: a plain button with a visible label and
        // a hint, no developer-assigned identifier. The hint is not gated on the identifier the
        // way `value` is.
        let node = only_node(
            r#"{"role":"AXButton","AXLabel":"Send","help":"Sends the message",
                "frame":{"x":0,"y":0,"width":10,"height":10}}"#,
        );
        assert_eq!(node.name.as_deref(), Some("Send"));
        assert_eq!(node.description.as_deref(), Some("Sends the message"));
    }

    /// Every described node of the captured tree. The fixture is a verbatim
    /// `idb ui describe-all` capture, so what these assert is the platform's behaviour, not a
    /// JSON literal written to agree with the reader.
    fn captured_descriptions() -> Vec<DescribedSample> {
        let mut tree = build_tree(HINT_FIXTURE, SCALE, &win(), WalkLimits::DEFAULT).unwrap();
        tree.assign_ids();
        glass_core::description_census(&tree).samples
    }

    /// The description the captured tree gives the node named `name`, if any.
    fn captured_description(name: &str) -> Option<String> {
        captured_descriptions()
            .into_iter()
            .find(|s| s.name.as_deref() == Some(name))
            .map(|s| s.description)
    }

    #[test]
    fn the_captured_hinted_button_is_described_by_its_hint() {
        assert_eq!(
            captured_description("the-hinted-button").as_deref(),
            Some("Saves and closes the sheet")
        );
    }

    #[test]
    fn the_captured_text_field_is_described_by_the_label_its_identifier_displaced() {
        assert_eq!(
            captured_description("the-described-field").as_deref(),
            Some("Search query")
        );
    }

    #[test]
    fn the_captured_simulator_tree_describes_those_two_nodes_and_no_others() {
        let described = captured_descriptions();
        assert_eq!(described.len(), 2, "described nodes: {described:?}");
    }

    #[test]
    fn build_tree_errors_on_malformed_json() {
        let err = build_tree("this is not json", SCALE, &win(), WalkLimits::DEFAULT).unwrap_err();
        assert!(matches!(err, GlassError::AccessibilityUnavailable(_)));
    }

    #[test]
    fn build_tree_errors_on_unexpected_root_shape() {
        // A bare scalar is neither an element object nor an array of elements.
        let err = build_tree("42", SCALE, &win(), WalkLimits::DEFAULT).unwrap_err();
        assert!(matches!(err, GlassError::AccessibilityUnavailable(_)));
    }

    #[test]
    fn ios_checkable_checked_only_claims_a_determinate_toggle() {
        use AxRole::*;
        assert_eq!(checkable_checked(CheckBox, Some("1")), (true, true));
        assert_eq!(checkable_checked(CheckBox, Some("0")), (true, false));
        assert_eq!(checkable_checked(ToggleButton, Some("1")), (true, true));
        assert_eq!(checkable_checked(ToggleButton, Some("0")), (true, false));
        // Not a determinate 0/1, or not a checkable role → neither (the #170 invariant).
        assert_eq!(checkable_checked(CheckBox, Some("mixed")), (false, false));
        assert_eq!(checkable_checked(CheckBox, None), (false, false));
        assert_eq!(checkable_checked(TextField, Some("1")), (false, false));
        assert_eq!(checkable_checked(Button, Some("0")), (false, false));
    }

    /// Array JSON of `n` flat top-level elements, each a distinctly-labeled `AXButton`
    /// with no `AXUniqueId` (so `map_node` falls back to `AXLabel` for the name).
    fn wide_array_json(n: usize) -> String {
        let mut elems = Vec::with_capacity(n);
        for i in 0..n {
            elems.push(format!(
                r#"{{"role":"AXButton","AXLabel":"btn{i}","frame":{{"x":0,"y":0,"width":10,"height":10}}}}"#
            ));
        }
        format!("[{}]", elems.join(","))
    }

    #[test]
    fn truncation_stops_the_walk_and_never_shifts_surviving_ids() {
        // ids are assigned in the same pre-order the tree is walked, so a truncation
        // that dropped a node from the middle rather than stopping at the end would
        // shift every later id off the node its own index implies.
        let json = wide_array_json(glass_core::MAX_NODES + 50);
        let mut tree = build_tree(&json, 1.0, &win(), WalkLimits::DEFAULT).expect("tree builds");
        tree.assign_ids();

        assert!(tree.truncated.is_some(), "the node cap must have been hit");
        // The synthetic Window root is id 0 (never counted against the budget); the
        // top-level element at array index i is id i+1.
        let third = tree.find(AxNodeId(3)).expect("id 3 survives");
        assert_eq!(third.name.as_deref(), Some("btn2"));
    }

    #[test]
    fn a_complete_tree_of_exactly_max_nodes_reports_no_truncation() {
        // MAX_NODES flat top-level elements: the walk visits every one of them, and the
        // LAST one is what pushes the running count to MAX_NODES. Nothing was declined, so
        // this must NOT be reported truncated (regression for the false-positive-at-the-cap
        // bug).
        let json = wide_array_json(glass_core::MAX_NODES);
        let tree = build_tree(&json, 1.0, &win(), WalkLimits::DEFAULT).expect("tree builds");
        assert_eq!(tree.truncated, None);
    }

    #[test]
    fn a_tree_of_max_nodes_plus_one_still_reports_nodes_truncation() {
        // One more element than the complete case above: now there really is a node the
        // walk declines to visit, so the cap must still fire — proving the fix didn't just
        // disable it.
        let json = wide_array_json(glass_core::MAX_NODES + 1);
        let tree = build_tree(&json, 1.0, &win(), WalkLimits::DEFAULT).expect("tree builds");
        assert_eq!(
            tree.truncated.map(|t| t.limit),
            Some(TruncationLimit::Nodes)
        );
    }

    #[test]
    fn a_childless_node_at_the_spent_node_budget_records_no_truncation() {
        // A leaf with no "children" key, reached once the node budget is already spent,
        // must not be reported truncated merely for declining to explore an empty list.
        let leaf = serde_json::json!({
            "role": "AXButton",
            "AXLabel": "leaf",
            "frame": {"x": 0, "y": 0, "width": 10, "height": 10}
        });
        let mut budget = WalkBudget::new();
        for _ in 0..glass_core::MAX_NODES {
            budget.visit();
        }
        let _ = map_node(&leaf, 1.0, 0, &mut budget);
        assert!(budget.truncation().is_none());
    }

    #[test]
    fn build_tree_reads_switch_checked_state_and_keeps_label_as_value() {
        // A UISwitch as idb reports it: role AXCheckBox with subrole AXSwitch (so the mapped role
        // is ToggleButton), stable id, label, AXValue "1" (on).
        let j = r#"[{"role":"AXCheckBox","subrole":"AXSwitch","AXUniqueId":"boldText",
                     "AXLabel":"Bold Text","AXValue":"1",
                     "frame":{"x":0,"y":0,"width":100,"height":30}}]"#;
        let mut tree = build_tree(j, 1.0, &win(), WalkLimits::DEFAULT).expect("parses");
        tree.assign_ids();
        let sw = find_by_name(&tree.root, "boldText").expect("switch present");
        assert!(
            sw.states.checkable && sw.states.checked,
            "an on switch → checkable + checked"
        );
        // `checked` carries the state; the label still lives in `value` (unchanged behavior).
        assert_eq!(sw.value.as_deref(), Some("Bold Text"));
    }

    #[test]
    fn tokens_the_probe_found_map_to_their_roles() {
        // Both were observed across most stock apps probed: AXGroup wraps content — in those
        // apps the navigation bar and the tab bar arrived as AXGroup too, named by the
        // element identifier rather than by the role — and AXHeading is a screen or section
        // title. An app that reports AXNavigationBar or AXTabBar instead still maps to
        // Toolbar or TabList; those rows are unchanged.
        assert_eq!(ax_role("AXGroup"), AxRole::Group);
        assert_eq!(ax_role("AXHeading"), AxRole::Heading);
        // A generic element carries no role at all, so it stays Other and the native token
        // stays visible in the outline. Mapping it to Group would claim a structure that
        // the platform never reported.
        assert_eq!(ax_role("AXGenericElement"), AxRole::Other);
        // Two tokens the role fixture (examples/ios-role-fixture) was seen to emit that nothing
        // maps yet: a segmented control reports AXTabGroup, and a menu-style SwiftUI Picker
        // reports AXPopUpButton. Pinned as Other so the observation lives somewhere a reader
        // hits, and so mapping either later is a deliberate edit rather than a silent one — see
        // the TabList and ComboBox rows of `glass_core::role_support::ROLE_SUPPORT`.
        assert_eq!(ax_role("AXTabGroup"), AxRole::Other);
        assert_eq!(ax_role("AXPopUpButton"), AxRole::Other);
    }

    #[test]
    fn role_tokens_have_no_duplicates() {
        for (i, (token, _)) in ROLE_TOKENS.iter().enumerate() {
            assert!(
                !ROLE_TOKENS[i + 1..].iter().any(|(other, _)| other == token),
                "{token} listed twice"
            );
        }
    }

    #[test]
    fn a_uiswitch_is_a_togglebutton_not_a_checkbox() {
        // Measured on iOS 26.5: a UISwitch reports role=AXCheckBox subrole=AXSwitch. Reading the
        // base role alone reports it as the checkbox it is not, and disagrees with the role the
        // other four backends give the same control.
        assert_eq!(
            ax_role_with_subrole("AXCheckBox", Some("AXSwitch")),
            AxRole::ToggleButton
        );
    }

    #[test]
    fn the_switch_tokens_are_subroles_not_roles() {
        // They were role-table entries until a device read showed idb sends them only as subroles.
        // Pinned as `Other` so mapping either as a role again is a deliberate edit, the same
        // convention `tokens_the_probe_found_map_to_their_roles` applies to every unmapped token.
        assert_eq!(ax_role("AXSwitch"), AxRole::Other);
        assert_eq!(ax_role("AXToggle"), AxRole::Other);
    }

    #[test]
    fn a_real_checkbox_is_still_a_checkbox() {
        assert_eq!(ax_role_with_subrole("AXCheckBox", None), AxRole::CheckBox);
        // A subrole on a base role that does not carry one is not a switch.
        assert_eq!(
            ax_role_with_subrole("AXCell", Some("AXSwitch")),
            AxRole::Cell
        );
        assert_eq!(
            ax_role_with_subrole("AXCheckBox", Some("AXSomethingElse")),
            AxRole::CheckBox
        );
    }

    #[test]
    fn a_switch_keeps_its_checked_state_through_the_role_change() {
        // The regression this slice must not cause: `checkable_checked` keys off the role, so a
        // switch that becomes a ToggleButton has to keep reporting checkable/checked.
        assert_eq!(
            checkable_checked(AxRole::ToggleButton, Some("1")),
            (true, true)
        );
        assert_eq!(
            checkable_checked(AxRole::ToggleButton, Some("0")),
            (true, false)
        );
    }

    #[test]
    fn the_fixture_of_record_carries_a_switch_and_maps_it() {
        // Checked against the captured device output rather than JSON written in this file.
        let tree = build_tree(FIXTURE, SCALE, &win(), WalkLimits::DEFAULT).expect("parse");
        let sw = find_by_name(&tree.root, "toggleSwitch").expect("the fixture carries a switch");
        assert_eq!(sw.role, AxRole::ToggleButton);
        assert!(sw.states.checkable && sw.states.checked, "{:?}", sw.states);
    }

    #[test]
    fn a_switch_maps_end_to_end_from_the_json_idb_actually_sends() {
        // The node verbatim from the scratch fixture read, so the parse is pinned to the shape the
        // device sends rather than to one written here.
        let j = r#"[{"role":"AXCheckBox","subrole":"AXSwitch","type":"CheckBox",
            "AXUniqueId":"the-switch","AXLabel":null,"AXValue":"1","enabled":true,
            "frame":{"x":0,"y":0,"width":51,"height":31},"children":[]}]"#;
        let tree = build_tree(
            j,
            1.0,
            &WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            WalkLimits::DEFAULT,
        )
        .expect("parse");
        let n = &tree.root.children[0];
        assert_eq!(n.role, AxRole::ToggleButton);
        assert!(n.states.checkable && n.states.checked, "{:?}", n.states);
    }

    #[test]
    fn map_matches_declared_column() {
        use glass_core::role_support::{AxBackend, RoleSupport, support};
        for role in AxRole::ALL {
            // Both tables: a role this backend can only produce from a subrole (a switch) is as
            // mapped as one produced from a role token, and the matrix does not distinguish them.
            let mapped = ROLE_TOKENS.iter().any(|(_, r)| *r == role)
                || SUBROLE_TOKENS.iter().any(|(sub, _, bases)| {
                    bases
                        .iter()
                        .any(|base| ax_role_with_subrole(base, Some(sub)) == role)
                });
            match support(role, AxBackend::Ios).expect("declared in ROLE_SUPPORT") {
                RoleSupport::Mapped => {
                    assert!(mapped, "{role:?} declared Mapped but no token maps to it")
                }
                cell => {
                    assert!(
                        !mapped,
                        "{role:?} is produced by a token but the matrix does not declare it"
                    );
                    // The named token must not resolve to the very role the cell says is out
                    // of reach. Largely a second line behind the `!mapped` assertion above,
                    // but it pins the token itself, so a cell rewritten to name a different
                    // token is checked against the map. A misspelt token resolves to `Other`
                    // and passes — only an on-box read catches that.
                    if let Some(token) = cell.named_token() {
                        assert_ne!(
                            ax_role(token),
                            role,
                            "{role:?} is declared out of reach naming {token}, which maps to it"
                        );
                    }
                }
            }
        }
    }
}
