//! iOS accessibility reader over idb's `accessibility_info`. Snapshot maps the nested JSON to an
//! AxTree; set_value re-finds the target element by identity (guarding against a stale id landing
//! on a different element), focuses it, then clears and types it in one batch of synthetic HID
//! input, and finally reads the element back and reports the write as not applied if it does not
//! hold the text.
use glass_core::accessibility::{Accessibility, AxContext, AxRect, AxTarget, AxTree, Located};
use std::time::Duration;

use glass_core::{
    GlassError, KeyEvent, MouseButton, PointerEvent, Result, typed_clear_landed, typed_text_landed,
};

use crate::axmap;
use crate::idb::client::IdbClient;
use crate::idb::proto;
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

/// Locate `target` in a tree described for the write and return its window-relative pixel bounds —
/// the guard every write runs before it dispatches.
///
/// Re-resolved by identity via [`AxTarget::relocate`], not by pre-order id alone. The caller's
/// snapshot and this describe are separate reads of a live app, so anything that shifted the tree
/// between them renumbers the field without moving it — the soft keyboard an earlier write raised is
/// the measured case, inserting two nodes ahead of it (glass#361). The read-back resolves the same
/// way, so no write is refused here for an identity the read-back would have accepted.
///
/// Identity alone does not decide it. Role and name repeat across a form's rows and are both `None`
/// on an unlabelled field, and [`Located::AtId`] is granted on that key plus a pre-order index this
/// very write perturbs — so on its own it would accept whatever editable renumbering slid onto the
/// id, tap it, and type into it, and the read-back would confirm the text in that same wrong node.
/// Bounds and value are therefore checked on whichever node was reached: overlap rather than
/// equality, so a field the keyboard shifted is still written where it now is, and value because a
/// recycled row keeps its identifier and its rect and differs only there — the discriminator
/// `AxTarget::value` exists for and Android's `fingerprinted` has always compared.
///
/// Every refusal is pre-dispatch. One the tree can no longer place, or that no longer looks like
/// what was addressed, answers with a typed drift verdict from [`AxTarget::drift_error`] —
/// `AxElementGone` where nothing presents as the element, `AxElementChanged` otherwise, truncation
/// included. A node that *is* the element but is no longer editable, or reports no bounds, answers
/// `AxElementNotEditable` / `AxElementNotClickable`. glass#361 replaced the `Backend` strings that
/// told an agent to re-snapshot without saying which had happened.
fn verify(tree: &AxTree, target: &AxTarget) -> Result<AxRect> {
    let node = match target.relocate(tree) {
        Located::AtId(node) | Located::Moved(node) => node,
        Located::Ambiguous(_) | Located::Gone | Located::Unproven => {
            return Err(target.drift_error(tree));
        }
    };
    if !target.bounds_overlap(node.bounds) || !target.value_consistent(node.value.as_deref()) {
        return Err(target.drift_error(tree));
    }
    // Nothing upstream checks this: `glass_set_value` reaches here for any element the caller
    // names, so a Label addressed by mistake resolves and matches. Without this the write would tap
    // an inert label and type into whatever had focus instead.
    if !node.states.editable {
        return Err(GlassError::AxElementNotEditable(target.id.0));
    }
    node.bounds
        .ok_or(GlassError::AxElementNotClickable(target.id.0))
}

/// How long to let the app commit typed text before each read-back attempt. Generous next to a
/// keystroke and small next to the `describe` that follows it.
const VERIFY_SETTLE: Duration = Duration::from_millis(300);

/// How many times to read the element back before reporting the write as not applied. A landed write
/// confirms on the first attempt and pays for one describe; the retries cover a field that commits a
/// frame or two later.
const VERIFY_ATTEMPTS: usize = 3;

/// Judge a typed `set_value` from a tree described after it. `Ok(())` only when the field holds
/// exactly what was asked for, or — for a named field — reads back empty after a clear.
///
/// Exact match, not "changed from before": tap-and-type is not atomic, so a dropped key or a field
/// that filters input leaves something that is neither the request nor the old value, and calling
/// that success is the failure this check exists to prevent. `glass_core::typed_text_landed` and
/// `glass_core::typed_clear_landed` carry the rules and the cost of them. Twin of `verify_write` in
/// `glass-android/src/a11y.rs` — keep the two in step.
///
/// The element is re-resolved by identity (role + name) via [`AxTarget::relocate`], not by its
/// pre-order id: measured on the Simulator, the focusing tap raises the soft keyboard and inserts
/// two nodes ahead of the field (glass#359), so the id itself is not stable across the write. A node
/// re-found that way is accepted only from a complete tree, and whichever node was reached must
/// still overlap where the target was — so a same-named field on a screen the tap navigated to is
/// refused instead. Overlap is checked here and not left to [`Located::Moved`]: the write's own tap
/// is what renumbers the tree, so the id can land on a neighbouring row that shares role and name,
/// and [`Located::AtId`] grants that without consulting bounds. An untouched neighbour reads back
/// empty, which is indistinguishable from a landed clear.
///
/// The node must also still be editable: for a non-editable node `axmap` puts the element's AXLabel
/// in `value`, so a label that changed inside the settle window would otherwise read as a successful
/// write into a Label. [`Located::AtId`] and [`Located::Moved`] both already carry the target's role
/// and name, so a node reached here that is not editable is not a neighbour that inherited the id —
/// it is the identified element having lost editability. Every refusal here is post-dispatch, hence
/// [`GlassError::AxWriteUnconfirmed`] throughout, where `verify` above refuses before the write goes
/// out and answers in pre-write verdicts — drift, not-editable or not-clickable.
///
/// A clear (`text` empty) is confirmed only for a target that carries a name. An empty read-back is
/// what any untouched same-role, same-name field would also show, so it is evidence of the write
/// only inasmuch as the identity that found the field is discriminating — a non-empty write's
/// exact-text check grounds itself, a clear has nothing to. A nameless target is refused however it
/// was reached, its own id included: [`AxTarget::matches`] compares `None` to `None`, so `AtId`
/// there accepts whatever nameless editable renumbering slid onto the id.
fn verify_write(after_tree: &AxTree, target: &AxTarget, text: &str) -> Result<()> {
    let node = match target.relocate(after_tree) {
        Located::AtId(node) | Located::Moved(node) => node,
        Located::Ambiguous(candidates) => {
            return Err(GlassError::AxWriteUnconfirmed(
                target.id.0,
                format!(
                    "{} elements now match its role and name, so which one holds it cannot be told",
                    candidates.len()
                ),
            ));
        }
        Located::Gone => {
            return Err(GlassError::AxWriteUnconfirmed(
                target.id.0,
                "nothing in the tree carries its role and name, so the screen it was on was \
                 replaced"
                    .into(),
            ));
        }
        Located::Unproven => {
            // Keep the cap in the message: it is what tells a caller to raise `max_nodes`. A
            // complete tree has no hole to blame, so the one way it
            // reaches here is a lone match drawn clear of where the element was.
            let why = if let Some(t) = &after_tree.truncated {
                format!(
                    "the tree was truncated at {} {}, so the element — or a second one matching \
                     it — may be past the cap",
                    t.limit_value,
                    t.limit.label()
                )
            } else if after_tree.unreadable > 0 {
                "a subtree could not be read, so the element — or a second one matching it — may \
                 be inside it"
                    .to_string()
            } else {
                "the only element carrying its role and name is drawn clear of where this one \
                 was, so it may be a different one"
                    .to_string()
            };
            return Err(GlassError::AxWriteUnconfirmed(target.id.0, why));
        }
    };
    if !target.bounds_overlap(node.bounds) {
        return Err(GlassError::AxWriteUnconfirmed(
            target.id.0,
            "the element carrying its role and name is drawn clear of where this one was, so the \
             text may have gone to a different one"
                .into(),
        ));
    }
    if !node.states.editable {
        return Err(GlassError::AxWriteUnconfirmed(
            target.id.0,
            "the element at that id is no longer editable, so the value could not be read back"
                .into(),
        ));
    }
    if text.is_empty() && target.name.is_none() {
        return Err(GlassError::AxWriteUnconfirmed(
            target.id.0,
            "the element has no name, so nothing but its position identifies it and an empty \
             field cannot confirm a clear landed on it"
                .into(),
        ));
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

/// The write's keystrokes as one batch of HID events: select-all, delete, then the text.
///
/// One batch because each `IdbClient::hid` is its own RPC, and a pause between the delete and the
/// text loses the text. Measured on the Simulator against `examples/ios-role-fixture`: back to back
/// the text lands 12/12; with a screenshot in between (0.14s) 2/4, with an accessibility read
/// (0.34s) 0/4, and with a bare sleep making no call at all (2s) 0/4 — so it is elapsed time, not
/// the extra request. Sending the groups a call each let one slow round-trip open that window
/// (glass#363). Do not split this back into a call per group for readability — the failure it
/// prevents is invisible on a fast, idle host.
fn clear_and_type_keys(injector: &IdbInjector, text: &str) -> Result<Vec<proto::HidEvent>> {
    let mut events = injector.key_events(&KeyEvent::Chord("super+a".into()))?;
    events.extend(injector.key_events(&KeyEvent::Chord("Delete".into()))?);
    events.extend(injector.key_events(&KeyEvent::Text(text.to_string()))?);
    Ok(events)
}

/// Send the whole write: the focusing tap, then the keystrokes, in exactly two calls.
///
/// Over a `send` seam rather than [`IdbClient`] directly so the call *count* is testable without a
/// device. That count is the fix for glass#363 — [`clear_and_type_keys`] returns a plain `Vec`, so
/// nothing but this function stops a caller sending it in pieces again.
///
/// The batch is built before the tap goes out: `KeyEvent::Text` refuses a non-ASCII character, and
/// building afterwards spent a tap that raised the keyboard and moved first responder before
/// reporting a write that never happened.
///
/// A failure of the second send is post-dispatch and says so. The batch clears the field before it
/// types, and `IdbClient::hid` streams it, so a failure part-way through can leave the field emptied
/// with the text lost — reporting that as a bare transport error would tell an agent nothing was
/// sent, and the session would keep the value it cached.
fn dispatch_write(
    send: &mut dyn FnMut(Vec<proto::HidEvent>) -> Result<()>,
    injector: &IdbInjector,
    tap: &PointerEvent,
    target_id: u32,
    text: &str,
) -> Result<()> {
    let keys = clear_and_type_keys(injector, text)?;
    send(injector.pointer_events(tap)?)?;
    send(keys).map_err(|e| {
        GlassError::AxWriteUnconfirmed(
            target_id,
            format!("sending the keystrokes failed part-way through ({e}), so the field may have been cleared without receiving the text"),
        )
    })
}

/// The read-back's own failure, reported as an unconfirmed write.
///
/// Post-dispatch by construction: its only caller is the loop that runs after the keystrokes went
/// out. A verdict `GlassError::set_value_failed_after_writing` rejects would leave the session
/// holding the value it cached for a write that already landed, and refuse the retry as drift.
fn read_back_failed(target: &AxTarget, e: &GlassError) -> GlassError {
    GlassError::AxWriteUnconfirmed(target.id.0, format!("reading the element back failed: {e}"))
}

impl Accessibility for IosA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        Ok(self.describe(ctx)?.0)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        // One describe serves both the guard and the injector's scale — no second read before the
        // keystrokes go out.
        let (tree, scale) = self.describe(ctx)?;
        let bounds = verify(&tree, target)?;
        let (cx, cy) = bounds
            .clamped_center(ctx.window.width, ctx.window.height)
            .ok_or(GlassError::AxElementNotClickable(target.id.0))?;
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
        // The tap keeps its own call — the field has to become first responder before the
        // keystrokes arrive, and a gap there is harmless (measured: the same write with 400ms
        // inserted between the two passes the smoke 6/6). A gap *within* the keystrokes is not.
        let client = &self.client;
        dispatch_write(
            &mut |events| client.hid(events),
            &injector,
            &tap,
            target.id.0,
            text,
        )?;

        // A failure of this read is not a failure of the write — the field has already been cleared
        // and typed into — so it says so rather than letting a caller retry blindly and type twice.
        let mut last = None;
        for _ in 0..VERIFY_ATTEMPTS {
            std::thread::sleep(VERIFY_SETTLE);
            let (after, _) = self
                .describe(ctx)
                .map_err(|e| read_back_failed(target, &e))?;
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

    fn tree_with(children: Vec<AxNode>) -> AxTree {
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
            children,
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
        tree_with(vec![field])
    }

    /// A target whose captured rect overlaps `shifted` in
    /// `a_renumbered_field_that_also_shifted_is_tapped_where_it_now_is` — the keyboard moving a
    /// field a little, not a different field elsewhere.
    fn shifted_target() -> AxTarget {
        AxTarget {
            bounds: Some(AxRect { y: 30, ..FIELD }),
            ..matching_target()
        }
    }

    /// A target matching that field as the snapshot before the write saw it.
    fn matching_target() -> AxTarget {
        AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("Note".into()),
            bounds: Some(FIELD),
            value: None,
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
        // throw away the one fact that explains the absence. No node here carries the target's role
        // and name, so a complete tree would call this Gone — the cap is what keeps it Unproven.
        let mut after = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        after.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 1,
            nodes_walked: 1,
        });
        let err = verify_write(&after, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("truncated at 1 nodes"), "{err}");
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
    fn a_relocated_write_still_checks_what_landed() {
        // Re-finding by identity must not shortcut the value check: something else now occupies the
        // target's id, the field is elsewhere in the tree, and it holds text other than what was
        // typed — still not a successful write.
        let mut root_field = leaf(0, AxRole::Label, "Heading", FIELD);
        let mut moved = leaf(0, AxRole::TextField, "Note", FIELD);
        moved.value = Some("hello".into());
        root_field.children.push(moved);
        let after = tree_with(vec![root_field]);
        assert!(matches!(
            verify_write(&after, &matching_target(), "world"),
            Err(GlassError::AxValueNotApplied(1))
        ));
    }

    #[test]
    fn a_node_that_lost_editability_is_unconfirmed_not_changed() {
        // Not the `AxElementNotEditable` the pre-write `verify` raises: a caller reading "not
        // dispatched" off that would retype into whatever the element collapsed into.
        let mut after = tree_with_value(Some("world"));
        after.root.children[0].states.editable = false;
        let err = verify_write(&after, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("editable"), "{err}");
    }

    /// The field with no name at all, which is where role+name identity runs out.
    fn nameless(value: Option<&str>) -> (AxTree, AxTarget) {
        let mut field = leaf(0, AxRole::TextField, "Note", FIELD);
        field.name = None;
        field.value = value.map(Into::into);
        let mut target = matching_target();
        target.name = None;
        (tree_with(vec![field]), target)
    }

    #[test]
    fn a_named_field_clears_after_being_re_found() {
        // The write's own focusing tap inserts two nodes ahead of the field, so a clear into a field
        // that was not already focused ALWAYS relocates. Keying the refusal on that made it fail by
        // construction; the name is what identifies the field, and the name survives the move.
        let moved = leaf(0, AxRole::TextField, "Note", FIELD);
        let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
        assert!(verify_write(&after, &matching_target(), "").is_ok());
    }

    #[test]
    fn a_nameless_field_cannot_confirm_a_clear_at_its_own_id() {
        // `AxTarget::matches` compares None to None, so the id alone accepts whatever nameless
        // editable renumbering slid onto it — and an empty read-back is exactly what that one shows
        // too. Nothing but the position says the keystrokes reached this field.
        let (after, target) = nameless(None);
        let err = verify_write(&after, &target, "").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("clear"), "{err}");
    }

    #[test]
    fn a_nameless_field_cannot_confirm_a_clear_after_being_re_found_either() {
        // The `Moved` half of the same hole: `matches` compares None to None during the search too,
        // so one nameless editable elsewhere is found by identity alone. Refusing only on `AtId`
        // would let this one through.
        let mut moved = leaf(0, AxRole::TextField, "Note", FIELD);
        moved.name = None;
        let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
        let mut target = matching_target();
        target.name = None;
        // Pin the arm: without this the fixture could drift onto AtId and still refuse, for the
        // reason the test above already covers.
        assert!(
            matches!(target.relocate(&after), Located::Moved(n) if n.id == AxNodeId(2)),
            "fixture must reach the Moved arm"
        );
        let err = verify_write(&after, &target, "").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(
            err.to_string().contains("no name"),
            "refused for the identity, not for having relocated: {err}"
        );
    }

    #[test]
    fn a_nameless_field_still_confirms_the_text_it_was_given() {
        // The identity is just as weak here; the typed text is what grounds it instead.
        let (after, target) = nameless(Some("world"));
        assert!(verify_write(&after, &target, "world").is_ok());
    }

    #[test]
    fn losing_editability_outranks_the_clear_refusal() {
        // Both refusals fit a nameless non-editable node. Editability is the specific fact, and a
        // caller told only "an empty field cannot confirm a clear" would go on hunting for a field.
        let (mut after, target) = nameless(None);
        after.root.children[0].states.editable = false;
        let err = verify_write(&after, &target, "").unwrap_err();
        assert!(err.to_string().contains("editable"), "{err}");
    }

    #[test]
    fn a_field_drawn_clear_of_the_target_does_not_confirm_the_write() {
        // Role and name pick out exactly one node, and it is nowhere near where the element was —
        // the shape of a tap that navigated, where the keystrokes went to another screen's field.
        let mut moved = leaf(0, AxRole::TextField, "Note", AxRect { y: 600, ..FIELD });
        moved.value = Some("world".into());
        let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
        let err = verify_write(&after, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("drawn clear of where"), "{err}");
    }

    #[test]
    fn a_truncated_tree_cannot_confirm_a_write_against_its_one_match() {
        // One match in the part that was kept is not one match in the tree: the second may be past
        // the cap, which is the refusal a complete tree makes as Ambiguous.
        let mut moved = leaf(0, AxRole::TextField, "Note", FIELD);
        moved.value = Some("world".into());
        let mut after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
        after.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 3,
            nodes_walked: 3,
        });
        let err = verify_write(&after, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("truncated at 3 nodes"), "{err}");
    }

    #[test]
    fn a_failed_read_back_reports_the_write_as_unconfirmed() {
        // The read runs after the keystrokes went out, so a transport failure here must be a verdict
        // `set_value_failed_after_writing` accepts — otherwise the session keeps the value it cached
        // for a write that already landed and refuses the retry as drift.
        let err = read_back_failed(
            &matching_target(),
            &GlassError::Backend("idb: connection reset".into()),
        );
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.set_value_failed_after_writing(), "{err}");
        assert!(err.to_string().contains("connection reset"), "{err}");
    }

    #[test]
    fn a_clear_is_not_confirmed_against_a_sibling_row_at_the_id() {
        // The write's own tap is what renumbers the tree, so at read-back the id can resolve to a
        // neighbouring field that shares role and name. An untouched one reads back empty, which is
        // exactly what a landed clear looks like — so without a positional check the clear is
        // confirmed against a row nothing was typed into.
        let sibling = leaf(0, AxRole::TextField, "Note", AxRect { y: 600, ..FIELD });
        let after = tree_with(vec![sibling]);
        assert!(
            matches!(matching_target().relocate(&after), Located::AtId(_)),
            "the fixture must reach AtId, the arm relocate grants without checking bounds"
        );
        let err = verify_write(&after, &matching_target(), "").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("drawn clear of where"), "{err}");
    }

    #[test]
    fn a_write_confirmed_at_a_new_id_is_a_success() {
        // glass#359, the measured case: the keyboard renumbered the field and the text landed.
        let mut moved = leaf(0, AxRole::TextField, "Note", FIELD);
        moved.value = Some("world".into());
        let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
        assert!(verify_write(&after, &matching_target(), "world").is_ok());
    }

    #[test]
    fn an_id_past_the_end_still_confirms_a_landed_write() {
        // The mirror image of the renumbering case: the keyboard dismissing after a landed write
        // can shrink the tree back down, leaving the field's old id past the end of it — nothing
        // resolves there, but the field itself never moved and still holds the text.
        let after = tree_with_value(Some("world"));
        let mut t = matching_target();
        t.id = AxNodeId(9);
        assert!(verify_write(&after, &t, "world").is_ok());
    }

    #[test]
    fn a_write_that_replaced_the_screen_says_it_was_typed_but_unconfirmed() {
        let replaced = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        // A capped tree would answer Unproven, not Gone; pin the fixture so a future change to
        // `tree_with` cannot silently move this test onto the wrong arm and still pass.
        assert!(
            replaced.is_complete(),
            "a capped tree would answer Unproven, not Gone"
        );
        let err = verify_write(&replaced, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        // "the text was typed" is common to every AxWriteUnconfirmed arm (it's in the #[error]
        // template, not this arm's own clause) — assert the Gone arm's own sentence too, or a
        // change to just this arm's message would leave the suite green.
        assert!(err.to_string().contains("the text was typed"), "{err}");
        assert!(err.to_string().contains("replaced"), "{err}");
    }

    #[test]
    fn two_matching_fields_refuse_and_say_how_many() {
        // The target's own id must land on something else first — a node still holding it would
        // take the AtId fast path and never learn a second candidate exists.
        let mut a = leaf(0, AxRole::TextField, "Note", FIELD);
        a.value = Some("world".into());
        let mut b = leaf(0, AxRole::TextField, "Note", FIELD);
        b.value = Some("world".into());
        let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), a, b]);
        let err = verify_write(&after, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(
            err.to_string().contains("2 elements now match"),
            "names how many, not just some digit: {err}"
        );
    }

    #[test]
    fn an_incomplete_tree_cannot_report_the_element_gone() {
        // A cap that fired hides elements, so absence from what was kept is not absence.
        let mut replaced = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        replaced.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 2,
            nodes_walked: 2,
        });
        let mut t = matching_target();
        t.id = AxNodeId(15);
        let err = verify_write(&replaced, &t, "world").unwrap_err();
        assert!(
            matches!(err, GlassError::AxWriteUnconfirmed(15, _)),
            "{err}"
        );
        assert!(err.to_string().contains("truncated at 2 nodes"), "{err}");
    }

    #[test]
    fn an_id_resolving_to_a_mismatch_takes_the_same_truncation_branch() {
        // The id-mismatch arm used to skip the truncation check entirely; it must fall through to
        // the same search, and the same cap, as an out-of-range id.
        let mut replaced = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        replaced.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 2,
            nodes_walked: 2,
        });
        let err = verify_write(&replaced, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("truncated at 2 nodes"), "{err}");
    }

    #[test]
    fn an_incomplete_tree_does_not_claim_the_element_vanished() {
        let mut replaced = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        replaced.unreadable = 1;
        let err = verify_write(&replaced, &matching_target(), "world").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(
            !err.to_string().contains("replaced"),
            "an incomplete read must not assert the screen changed: {err}"
        );
    }

    /// A `send` that records what it was handed instead of reaching a device, so a test can assert
    /// how many calls the write made — the property glass#363 turns on.
    fn recording_send(
        log: &mut Vec<Vec<proto::HidEvent>>,
    ) -> impl FnMut(Vec<proto::HidEvent>) -> Result<()> {
        move |events| {
            log.push(events);
            Ok(())
        }
    }

    fn a_tap() -> PointerEvent {
        PointerEvent::Click {
            x: 10,
            y: 20,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        }
    }

    #[test]
    fn the_whole_write_goes_out_in_exactly_two_calls() {
        // glass#363: each call is its own RPC, and a pause between the clear and the text loses the
        // text. One call for the tap, one for every keystroke — three calls reinstates the bug, and
        // `clear_and_type_keys` returning a plain Vec is what would let someone split it again.
        let injector = IdbInjector::new(2.0);
        let mut log = Vec::new();
        dispatch_write(&mut recording_send(&mut log), &injector, &a_tap(), 1, "hi").unwrap();

        assert_eq!(log.len(), 2, "the tap, then every keystroke in one call");
        assert_eq!(log[0], injector.pointer_events(&a_tap()).unwrap());
        assert_eq!(log[1], clear_and_type_keys(&injector, "hi").unwrap());
    }

    #[test]
    fn a_failed_focus_tap_never_dispatches_the_keystrokes() {
        // Typing into whatever had focus before is worse than not writing at all.
        let injector = IdbInjector::new(2.0);
        let mut calls = 0;
        let mut send = |_events| {
            calls += 1;
            Err(GlassError::Backend("idb: connection reset".into()))
        };
        let err = dispatch_write(&mut send, &injector, &a_tap(), 1, "hi").unwrap_err();
        assert_eq!(calls, 1, "the keystrokes must not follow a tap that failed");
        assert!(
            !err.set_value_failed_after_writing(),
            "nothing was typed, so the session must keep its cached value: {err}"
        );
    }

    #[test]
    fn keystrokes_lost_part_way_report_the_field_may_be_cleared() {
        // The batch clears before it types, so a stream that dies between the two leaves the field
        // empty. Reporting that as a bare transport error would read as "nothing was sent".
        let injector = IdbInjector::new(2.0);
        let mut calls = 0;
        let mut send = |_events| {
            calls += 1;
            if calls == 1 {
                Ok(())
            } else {
                Err(GlassError::Backend("idb hid timed out after 30s".into()))
            }
        };
        let err = dispatch_write(&mut send, &injector, &a_tap(), 7, "hi").unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(7, _)), "{err}");
        assert!(
            err.set_value_failed_after_writing(),
            "the session must drop the value it cached: {err}"
        );
        assert!(err.to_string().contains("may have been cleared"), "{err}");
    }

    #[test]
    fn text_the_backend_cannot_type_refuses_before_anything_is_sent() {
        // Building the batch before the tap is what makes this a true no-op: under the old order
        // the clear had already emptied the field when the text was found to be untypeable.
        let injector = IdbInjector::new(2.0);
        let mut log = Vec::new();
        let err = dispatch_write(
            &mut recording_send(&mut log),
            &injector,
            &a_tap(),
            1,
            "café",
        )
        .unwrap_err();
        assert!(matches!(err, GlassError::Unsupported(_)), "{err}");
        assert!(
            log.is_empty(),
            "nothing may be sent for a write that cannot be built"
        );
        assert!(!err.set_value_failed_after_writing(), "{err}");
    }

    #[test]
    fn clear_and_type_keys_concatenates_the_three_groups_in_order() {
        // Content and order only — that these leave in ONE call is
        // `the_whole_write_goes_out_in_exactly_two_calls`.
        let injector = IdbInjector::new(2.0);
        let select_all = injector
            .key_events(&KeyEvent::Chord("super+a".into()))
            .unwrap();
        let delete = injector
            .key_events(&KeyEvent::Chord("Delete".into()))
            .unwrap();
        let text = injector.key_events(&KeyEvent::Text("hi".into())).unwrap();

        let batch = clear_and_type_keys(&injector, "hi").unwrap();

        assert_eq!(
            batch.len(),
            select_all.len() + delete.len() + text.len(),
            "every keystroke of all three groups travels in the one batch"
        );
        assert_eq!(batch[..select_all.len()], select_all[..]);
        assert_eq!(
            batch[select_all.len()..select_all.len() + delete.len()],
            delete[..]
        );
        assert_eq!(batch[select_all.len() + delete.len()..], text[..]);
    }

    #[test]
    fn a_clear_still_sends_the_keystrokes_that_empty_the_field() {
        // `set_value(id, "")` types nothing, so the batch is select-all then delete and nothing
        // else. Assert both, in order: a length total alone passes with the two swapped, which on
        // a device deletes one character instead of the field.
        let injector = IdbInjector::new(2.0);
        let mut expected = injector
            .key_events(&KeyEvent::Chord("super+a".into()))
            .unwrap();
        expected.extend(
            injector
                .key_events(&KeyEvent::Chord("Delete".into()))
                .unwrap(),
        );

        let batch = clear_and_type_keys(&injector, "").unwrap();

        assert!(!batch.is_empty(), "a clear must still dispatch keystrokes");
        assert_eq!(batch, expected);
    }

    #[test]
    fn verify_accepts_matching_target() {
        let r = AxRect {
            x: 10,
            y: 20,
            width: 100,
            height: 30,
        };
        let tree = tree_with(vec![leaf(0, AxRole::TextField, "inputField", r)]);
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("inputField".into()),
            bounds: Some(r),
            value: None,
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
        let tree = tree_with(vec![leaf(0, AxRole::Button, "inputField", r)]);
        let target = AxTarget {
            id: AxNodeId(1),
            role: AxRole::TextField,
            name: Some("inputField".into()),
            bounds: Some(r),
            value: None,
        };
        // Nothing in a complete tree carries the target's role, so the element is gone rather
        // than merely changed.
        assert!(matches!(
            verify(&tree, &target),
            Err(GlassError::AxElementGone(1))
        ));
    }

    #[test]
    fn a_renumbered_field_is_located_by_identity_before_the_write() {
        // glass#361, the measured case: between the caller's snapshot and the write's own describe
        // the soft keyboard inserted nodes ahead of the field, so its id now resolves elsewhere.
        // The field itself never moved, and the write must reach it rather than refuse.
        let tree = tree_with(vec![
            leaf(0, AxRole::Label, "Suggestions", FIELD),
            leaf(0, AxRole::TextField, "Note", FIELD),
        ]);
        // Pin the arm: `AtId` returns the same rect, so without this the test would keep passing
        // while no longer exercising the relocation it is named for.
        assert!(
            matches!(matching_target().relocate(&tree), Located::Moved(_)),
            "the fixture must reach Moved"
        );
        assert_eq!(verify(&tree, &matching_target()).unwrap(), FIELD);
    }

    #[test]
    fn a_renumbered_field_that_also_shifted_is_tapped_where_it_now_is() {
        // The tap goes to the rect this returns, so it must be the node's current one and not the
        // target's — the keyboard both renumbers the field and moves it.
        let shifted = AxRect { y: 45, ..FIELD };
        let tree = tree_with(vec![
            leaf(0, AxRole::Label, "Suggestions", FIELD),
            leaf(0, AxRole::TextField, "Note", shifted),
        ]);
        assert!(
            matches!(shifted_target().relocate(&tree), Located::Moved(_)),
            "the fixture must reach Moved"
        );
        assert_eq!(verify(&tree, &shifted_target()).unwrap(), shifted);
    }

    #[test]
    fn a_same_named_field_elsewhere_at_the_id_refuses_the_write() {
        // A form's rows share role and name, and an unlabelled field has neither, so identity plus
        // a pre-order index the write itself perturbs is not identification. Without a positional
        // check this is tapped and typed into, and the read-back — resolving the same way — reads
        // the text back out of it and calls the write a success.
        let other_row = AxRect { y: 600, ..FIELD };
        let tree = tree_with(vec![leaf(0, AxRole::TextField, "Note", other_row)]);
        assert!(
            matches!(matching_target().relocate(&tree), Located::AtId(_)),
            "the fixture must reach AtId, the arm without a positional check of its own"
        );
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_recycled_row_holding_a_different_value_refuses_the_write() {
        // A scrolled table reuses the cell, so role, name and rect all survive and only the value
        // differs — the one discriminator left, and the reason `AxTarget::value` is captured.
        let mut recycled = leaf(0, AxRole::TextField, "Note", FIELD);
        recycled.value = Some("row 9".into());
        let tree = tree_with(vec![recycled]);
        let mut target = matching_target();
        target.value = Some("row 3".into());
        assert!(matches!(
            verify(&tree, &target),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_field_drawn_clear_of_the_target_refuses_the_write_before_it_dispatches() {
        // The tap navigated to another screen carrying a same-named field. The read-back refuses
        // this after the write; the guard must refuse it before one goes out.
        let tree = tree_with(vec![
            leaf(0, AxRole::Label, "Suggestions", FIELD),
            leaf(0, AxRole::TextField, "Note", AxRect { y: 600, ..FIELD }),
        ]);
        assert!(
            matches!(matching_target().relocate(&tree), Located::Unproven),
            "the fixture must reach Unproven"
        );
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_tree_with_an_unreadable_subtree_refuses_the_write_as_changed_not_gone() {
        // Independent of the node cap: a subtree that could not be read hides elements just as a
        // cap does, so absence from what was read is not absence.
        let mut tree = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        tree.unreadable = 1;
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn two_matching_fields_refuse_the_write_as_changed() {
        // The id must land on something else first, or the AtId path would take the first one
        // and never learn a second exists.
        let tree = tree_with(vec![
            leaf(0, AxRole::Label, "Suggestions", FIELD),
            leaf(0, AxRole::TextField, "Note", FIELD),
            leaf(0, AxRole::TextField, "Note", FIELD),
        ]);
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_truncated_tree_refuses_the_write_as_changed_not_gone() {
        // A cap that fired hides elements, so absence from what was kept is not absence — the
        // element may be past the cap, which `AxElementGone` would assert it is not.
        let mut tree = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
        tree.truncated = Some(glass_core::accessibility::Truncation {
            limit: glass_core::accessibility::TruncationLimit::Nodes,
            limit_value: 2,
            nodes_walked: 2,
        });
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementChanged(1))
        ));
    }

    #[test]
    fn a_non_editable_element_refuses_the_write() {
        let mut field = leaf(0, AxRole::TextField, "Note", FIELD);
        field.states.editable = false;
        let tree = tree_with(vec![field]);
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementNotEditable(1))
        ));
    }

    #[test]
    fn an_element_without_bounds_refuses_the_write_as_not_clickable() {
        // The write's only way in is a tap at the element's center, so a node reporting no
        // geometry has to say that rather than fail later as a write that did not take.
        let mut field = leaf(0, AxRole::TextField, "Note", FIELD);
        field.bounds = None;
        let tree = tree_with(vec![field]);
        assert!(matches!(
            verify(&tree, &matching_target()),
            Err(GlassError::AxElementNotClickable(1))
        ));
    }
}
