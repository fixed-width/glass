//! iOS accessibility reader over idb's `accessibility_info`. Snapshot maps the
//! nested JSON to an AxTree; set_value re-verifies the target element (guarding
//! against a stale id landing on a different element), focuses it, clears it, and
//! types the new text via synthetic HID input.
use glass_core::accessibility::{Accessibility, AxContext, AxRect, AxTarget, AxTree};
use glass_core::{GlassError, KeyEvent, MouseButton, PointerEvent, Result, WindowGeometry};

use crate::axmap;
use crate::idb::client::IdbClient;
use crate::injector::IdbInjector;

/// Reads and writes the accessibility tree of the app under test in the
/// Simulator, over idb's `accessibility_info` and HID RPCs.
pub struct IosA11y {
    client: IdbClient,
}

/// The point→pixel scale for a describe response: the capture window's pixel width over
/// the describe root's logical-point width. Computed per describe rather than cached,
/// because this reader is built before the app launches — when the real scale is still
/// unknown — so a scale frozen at construction would be wrong. `None` if the tree carries
/// no positive root width. Delegates to [`axmap::scale_from_width`] so the reader and the
/// platform's scale discovery compute the ratio one way.
fn scale_from(json: &str, window: &WindowGeometry) -> Option<f64> {
    axmap::scale_from_width(json, window.width)
}

impl IosA11y {
    pub(crate) fn new(client: IdbClient) -> Self {
        IosA11y { client }
    }

    /// One describe round-trip: fetch the accessibility JSON, derive the point→pixel
    /// scale from `ctx.window` (pixels, valid once the app has started) and the describe
    /// root's point width, and map the id-assigned tree. Returns the tree and the scale,
    /// since `set_value` needs the same scale to place synthetic input.
    fn describe(&self, ctx: &AxContext) -> Result<(AxTree, f64)> {
        let json = self.client.describe_all()?;
        let scale = scale_from(&json, &ctx.window).ok_or_else(|| {
            GlassError::Backend(
                "could not determine the iOS accessibility scale from the tree".into(),
            )
        })?;
        let mut tree = axmap::build_tree(&json, scale, &ctx.window, ctx.limits)?;
        tree.assign_ids();
        Ok((tree, scale))
    }
}

/// Re-walk to `target.id`, confirm role+name (and bounds when known), return its
/// window-relative pixel bounds. Errors if the element drifted or vanished.
fn verify(tree: &AxTree, target: &AxTarget) -> Result<AxRect> {
    let node = tree
        .find(target.id)
        .ok_or(GlassError::AxElementNotFound(target.id.0))?;
    if !target.matches(node.role, node.name.as_deref()) {
        return Err(GlassError::Backend(
            "a11y set_value: element at that id changed since the snapshot; re-snapshot".into(),
        ));
    }
    if !target.bounds_consistent(node.bounds, 2) {
        return Err(GlassError::Backend(
            "a11y set_value: element moved since the snapshot; re-snapshot".into(),
        ));
    }
    node.bounds
        .ok_or_else(|| GlassError::Backend("a11y set_value: element has no bounds".into()))
}

impl Accessibility for IosA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        Ok(self.describe(ctx)?.0)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        // One describe yields both the id-assigned tree and the scale to place input at;
        // the ids here are final, not walked again.
        let (tree, scale) = self.describe(ctx)?;
        let bounds = verify(&tree, target)?;
        let (cx, cy) = bounds
            .clamped_center(ctx.window.width, ctx.window.height)
            .ok_or_else(|| GlassError::Backend("a11y set_value: element not on screen".into()))?;
        // Focus by tapping the element, select-all + delete to clear, then type — all
        // through an injector at this describe's scale.
        let injector = IdbInjector::new(scale);
        let tap = PointerEvent::Click {
            x: cx,
            y: cy,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        };
        self.client.hid(injector.pointer_events(&tap)?)?;
        self.client
            .hid(injector.key_events(&KeyEvent::Chord("super+a".into()))?)?;
        self.client
            .hid(injector.key_events(&KeyEvent::Chord("Delete".into()))?)?;
        self.client
            .hid(injector.key_events(&KeyEvent::Text(text.to_string()))?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree};

    fn leaf(id: u32, role: AxRole, name: &str, r: AxRect) -> AxNode {
        AxNode {
            id: AxNodeId(id),
            role,
            raw_role: String::new(),
            name: Some(name.into()),
            value: None,
            states: AxStates {
                editable: true,
                ..AxStates::default()
            },
            bounds: Some(r),
            children: vec![],
        }
    }

    fn tree_with(field: AxNode) -> AxTree {
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
                width: 400,
                height: 800,
            }),
            children: vec![field],
        };
        let mut t = AxTree::new(root);
        t.assign_ids();
        t
    }

    fn window_px(width: u32) -> WindowGeometry {
        WindowGeometry {
            x: 0,
            y: 0,
            width,
            height: 2622,
        }
    }

    #[test]
    fn scale_from_divides_window_pixels_by_root_point_width() {
        // A 1206px-wide capture over a 402pt describe root is the ×3 backing scale — the
        // real value this reader must recover per call, not the provisional 1.0.
        let json = r#"[{"frame":{"width":402}}]"#;
        assert_eq!(scale_from(json, &window_px(1206)), Some(3.0));
    }

    #[test]
    fn scale_from_is_none_without_a_root_point_width() {
        assert_eq!(scale_from("[]", &window_px(1206)), None);
    }

    #[test]
    fn verify_accepts_matching_target() {
        let r = AxRect {
            x: 10,
            y: 20,
            width: 100,
            height: 30,
        };
        let tree = tree_with(leaf(0, AxRole::TextField, "inputField", r));
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("inputField".into()),
            bounds: Some(r),
        };
        assert_eq!(verify(&tree, &target).unwrap(), r);
    }

    #[test]
    fn verify_rejects_role_mismatch() {
        let r = AxRect {
            x: 10,
            y: 20,
            width: 100,
            height: 30,
        };
        let tree = tree_with(leaf(0, AxRole::Button, "inputField", r));
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("inputField".into()),
            bounds: Some(r),
        };
        assert!(verify(&tree, &target).is_err());
    }

    #[test]
    fn verify_rejects_missing_id() {
        let r = AxRect {
            x: 10,
            y: 20,
            width: 100,
            height: 30,
        };
        // The tree only has ids 0 (root) and 1 (the field); id 9 resolves to nothing.
        let tree = tree_with(leaf(0, AxRole::TextField, "inputField", r));
        let target = AxTarget {
            id: AxNodeId(9),
            role: AxRole::TextField,
            name: Some("inputField".into()),
            bounds: Some(r),
        };
        // Pin the variant: an unresolved id must report AxElementNotFound (naming the id),
        // not the generic AxUnsupported — both are `Err`, so `.is_err()` alone wouldn't guard it.
        assert!(matches!(
            verify(&tree, &target),
            Err(GlassError::AxElementNotFound(id)) if id == target.id.0
        ));
    }

    #[test]
    fn verify_rejects_bounds_drift() {
        let r = AxRect {
            x: 10,
            y: 20,
            width: 100,
            height: 30,
        };
        let tree = tree_with(leaf(0, AxRole::TextField, "inputField", r));
        // Same role+name, but the target's captured bounds sit far from the node's —
        // beyond the tolerance, so a drifted id landing on a same-role element is rejected.
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("inputField".into()),
            bounds: Some(AxRect {
                x: 200,
                y: 400,
                width: 100,
                height: 30,
            }),
        };
        assert!(verify(&tree, &target).is_err());
    }
}
