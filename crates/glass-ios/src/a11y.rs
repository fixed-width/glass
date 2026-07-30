//! iOS accessibility reader over idb's `accessibility_info`. Snapshot maps the
//! nested JSON to an AxTree; set_value re-verifies the target element (guarding
//! against a stale id landing on a different element), focuses it, clears it, and
//! types the new text via synthetic HID input.
use glass_core::accessibility::{Accessibility, AxContext, AxRect, AxTarget, AxTree};
use std::time::Duration;

use glass_core::{GlassError, KeyEvent, MouseButton, PointerEvent, Result, read_back_confirms};

use crate::axmap;
use crate::idb::client::IdbClient;
use crate::injector::IdbInjector;

/// Reads and writes the accessibility tree of the app under test in the
/// Simulator, over idb's `accessibility_info` and HID RPCs.
pub struct IosA11y {
    client: IdbClient,
    /// The target's scale, fetched on first need and kept: a property of the device, so it
    /// does not change between snapshots.
    scale: Option<f64>,
}

impl IosA11y {
    pub(crate) fn new(client: IdbClient) -> Self {
        IosA11y {
            client,
            scale: None,
        }
    }

    /// The target's point→pixel scale, from the device rather than from the tree.
    ///
    /// Not the tree's own `capture width / root point width`: that divides by the widest
    /// *top-level* element, which equals the screen only when one spans it, and it is
    /// unavailable at all for the second or so an app takes to render. The platform's
    /// injector converts with the device's scale, so a reader using a different one would
    /// report bounds that tap somewhere else.
    fn scale(&mut self) -> Result<f64> {
        if let Some(scale) = self.scale {
            return Ok(scale);
        }
        let scale = crate::platform::checked_scale(self.client.describe()?.density)?;
        self.scale = Some(scale);
        Ok(scale)
    }

    /// One describe round-trip: fetch the accessibility JSON and map the id-assigned tree at
    /// the target's scale. Returns the scale alongside it, since `set_value` places synthetic
    /// input at the same one.
    fn describe(&mut self, ctx: &AxContext) -> Result<(AxTree, f64)> {
        let scale = self.scale()?;
        let json = self.client.describe_all()?;
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

/// How long to let the app commit typed text before reading it back. Generous next to a keystroke,
/// small next to the `describe` that follows it.
const VERIFY_SETTLE: Duration = Duration::from_millis(300);

/// Judge a `set_value` write from a tree described after it, against the value the target held
/// before. `Ok(())` only when the read-back proves the write landed.
///
/// A tap-and-type can miss: the field may reject input, the keyboard may not have come up, or the
/// tap may land on a neighbour. Reporting success then tells an agent a field holds text it does
/// not hold. A target no longer at that id is [`GlassError::AxElementChanged`], since the write may
/// have landed on something that then moved.
fn verify_write(
    after_tree: &AxTree,
    target: &AxTarget,
    before: Option<&str>,
    text: &str,
) -> Result<()> {
    let node = after_tree
        .find(target.id)
        .ok_or(GlassError::AxElementChanged(target.id.0))?;
    if !target.matches(node.role, node.name.as_deref()) {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    if read_back_confirms(node.value.as_deref(), before, text) {
        Ok(())
    } else {
        Err(GlassError::AxValueNotApplied(target.id.0))
    }
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
        // The baseline for the read-back, from the describe that already located the target.
        let before = tree.find(target.id).and_then(|n| n.value.clone());
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

        // Read back once, not in a loop: a re-read is a whole `describe` round trip through idb.
        std::thread::sleep(VERIFY_SETTLE);
        let (after, _) = self.describe(ctx)?;
        verify_write(&after, target, before.as_deref(), text)
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
            description: None,
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
            description: None,
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

    const FIELD: AxRect = AxRect {
        x: 10,
        y: 20,
        width: 100,
        height: 40,
    };

    /// The one-field tree a read-back sees, holding `value`.
    fn tree_with_value(value: Option<&str>) -> AxTree {
        let mut field = leaf(0, AxRole::TextField, "Note", FIELD);
        field.value = value.map(Into::into);
        tree_with(field)
    }

    /// A target matching that field as the snapshot before the write saw it.
    fn matching_target() -> AxTarget {
        AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("Note".into()),
            bounds: Some(FIELD),
        }
    }

    #[test]
    fn a_write_that_landed_is_reported_as_success() {
        let after = tree_with_value(Some("world"));
        let t = matching_target();
        assert!(verify_write(&after, &t, Some("hello"), "world").is_ok());
    }

    #[test]
    fn a_field_that_never_changed_is_not_a_successful_write() {
        let after = tree_with_value(Some("hello"));
        let t = matching_target();
        assert!(matches!(
            verify_write(&after, &t, Some("hello"), "world"),
            Err(GlassError::AxValueNotApplied(_))
        ));
    }

    #[test]
    fn a_target_that_moved_is_reported_as_changed_not_as_a_failed_write() {
        let after = tree_with_value(Some("world"));
        let mut t = matching_target();
        t.name = Some("A different field".into());
        assert!(matches!(
            verify_write(&after, &t, Some("hello"), "world"),
            Err(GlassError::AxElementChanged(_))
        ));
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
