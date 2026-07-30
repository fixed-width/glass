//! iOS accessibility reader over idb's `accessibility_info`. Snapshot maps the
//! nested JSON to an AxTree; set_value re-verifies the target element (guarding
//! against a stale id landing on a different element), focuses it, clears it, and
//! types the new text via synthetic HID input, then reads the element back and reports the write as not applied if it does not hold it.
use glass_core::accessibility::{Accessibility, AxContext, AxRect, AxTarget, AxTree};
use std::time::Duration;

use glass_core::{
    GlassError, KeyEvent, MouseButton, PointerEvent, Result, typed_clear_landed, typed_text_landed,
};

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
    // Android and macOS gate on this and iOS did not, which mattered once the write is checked: for
    // a non-editable node `axmap` puts the element's AXLabel in `value`, so a label that changed for
    // any reason inside the settle window would have read as a successful write into a Label.
    if !node.states.editable {
        return Err(GlassError::AxElementNotEditable(target.id.0));
    }
    node.bounds
        .ok_or_else(|| GlassError::Backend("a11y set_value: element has no bounds".into()))
}

/// How long to let the app commit typed text before each read-back attempt. Generous next to a
/// keystroke and small next to the `describe` that follows it.
const VERIFY_SETTLE: Duration = Duration::from_millis(300);

/// How many times to read the element back before reporting the write as not applied. A landed write
/// confirms on the first attempt and pays for one describe; the retries cover a field that commits a
/// frame or two later.
const VERIFY_ATTEMPTS: usize = 3;

/// Judge a typed `set_value` from a tree described after it. `Ok(())` only when the field holds
/// exactly what was asked for, or — for a clear — reads back empty.
///
/// Exact match, not "changed from before": tap-and-type is not atomic, so a dropped key or a field
/// that filters input leaves something that is neither the request nor the old value, and calling
/// that success is the failure this check exists to prevent. `glass_core::typed_text_landed` and
/// `glass_core::typed_clear_landed` carry the rules and the cost of them. Twin of `verify_write` in
/// `glass-android/src/a11y.rs` — keep the two in step.
///
/// The element is re-resolved by its pre-order id, which the write itself can perturb (the keyboard
/// appears between the two describes), so a mismatch is [`GlassError::AxElementChanged`] —
/// "re-snapshot" — rather than a claim about the write. A tree cut short by the walk caps explains an
/// absent element better than drift does, so that case says so instead.
///
/// The node must also still be editable: for a non-editable node `axmap` puts the element's AXLabel
/// in `value`, so a label that changed inside the settle window would otherwise read as a successful
/// write into a Label.
fn verify_write(after_tree: &AxTree, target: &AxTarget, text: &str) -> Result<()> {
    let Some(node) = after_tree.find(target.id) else {
        return Err(match &after_tree.truncated {
            // `Truncation::notice()` is written to close a rendered outline — a leading ellipsis and
            // its own pixel-fallback advice — so this states the cap itself instead.
            Some(t) => GlassError::AccessibilityUnavailable(format!(
                "a11y set_value: the text was typed, but the read-back could not find element {} \
                 because the tree was truncated at {} {}; re-snapshot rather than retyping",
                target.id.0,
                t.limit_value,
                t.limit.label(),
            )),
            None => GlassError::AxElementChanged(target.id.0),
        });
    };
    if !target.matches(node.role, node.name.as_deref()) || !node.states.editable {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    let landed = if text.is_empty() {
        typed_clear_landed(node.value.as_deref())
    } else {
        typed_text_landed(node.value.as_deref(), text)
    };
    if landed {
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

        // A failure of this read is not a failure of the write — the field has already been cleared
        // and typed into — so it says so rather than letting a caller retry blindly and type twice.
        let mut last = None;
        for _ in 0..VERIFY_ATTEMPTS {
            std::thread::sleep(VERIFY_SETTLE);
            let (after, _) = self.describe(ctx).map_err(|e| {
                GlassError::Backend(format!(
                    "a11y set_value: the text was typed, but reading the element back failed: {e}; \
                     re-snapshot to see whether it landed rather than retyping"
                ))
            })?;
            match verify_write(&after, target, text) {
                Ok(()) => return Ok(()),
                // Only a not-applied verdict can change on a later describe: drift and truncation
                // are structural, so re-describing for them reaches the same answer more slowly.
                Err(e @ GlassError::AxValueNotApplied(_)) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or(GlassError::AxValueNotApplied(target.id.0)))
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
        assert!(verify_write(&after, &matching_target(), "world").is_ok());
    }

    #[test]
    fn clearing_a_field_is_a_write_that_can_succeed() {
        // An empty field reports no value at all, so a rule that read `None` as "unknown" made
        // `set_value(id, "")` fail every time.
        let after = tree_with_value(None);
        assert!(verify_write(&after, &matching_target(), "").is_ok());
    }

    #[test]
    fn a_cleared_field_still_reporting_text_reads_as_not_applied() {
        // A clear is judged by "reads back empty", not by "the value changed": the tap that begins
        // the clear can itself change a field that reformats on focus, and accepting that would
        // confirm a clear whose delete never fired.
        let after = tree_with_value(Some("Search settings"));
        assert!(matches!(
            verify_write(&after, &matching_target(), ""),
            Err(GlassError::AxValueNotApplied(_))
        ));
    }

    #[test]
    fn a_truncated_read_back_says_so_rather_than_blaming_drift() {
        // The iOS tree carries the same truncation record Android's does; reporting "it moved" would
        // throw away the one fact that explains the absence.
        let mut after = tree_with_value(Some("world"));
        after.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 1,
            nodes_walked: 1,
        });
        let mut t = matching_target();
        t.id = AxNodeId(99);
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AccessibilityUnavailable(_))
        ));
    }

    #[test]
    fn a_field_that_never_changed_is_not_a_successful_write() {
        let after = tree_with_value(Some("hello"));
        assert!(matches!(
            verify_write(&after, &matching_target(), "world"),
            Err(GlassError::AxValueNotApplied(1))
        ));
    }

    #[test]
    fn a_partly_typed_write_is_not_a_successful_write() {
        // A dropped keystroke: differs from the old value, so "changed from before" would have
        // called it success.
        let after = tree_with_value(Some("worl"));
        assert!(matches!(
            verify_write(&after, &matching_target(), "world"),
            Err(GlassError::AxValueNotApplied(1))
        ));
    }

    #[test]
    fn a_target_that_moved_is_reported_as_changed_not_as_a_failed_write() {
        let after = tree_with_value(Some("world"));
        let mut t = matching_target();
        t.name = Some("A different field".into());
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_non_editable_node_at_the_id_is_reported_as_changed() {
        // For a non-editable node this reader puts the element's AXLabel in `value`, so without the
        // editable gate a label that changed inside the settle window read as a successful write.
        let mut after = tree_with_value(Some("world"));
        after.root.children[0].states.editable = false;
        assert!(matches!(
            verify_write(&after, &matching_target(), "world"),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_target_missing_from_the_read_back_is_reported_as_changed() {
        let after = tree_with_value(Some("world"));
        let mut t = matching_target();
        t.id = AxNodeId(9);
        assert!(matches!(
            verify_write(&after, &t, "world"),
            Err(GlassError::AxElementChanged(9))
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
