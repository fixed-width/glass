//! iOS accessibility reader over idb's `accessibility_info`. Snapshot maps the nested JSON to an
//! AxTree; set_value re-finds the target element by identity (guarding against a stale id landing
//! on a different element), focuses it, then clears and types it in one batch of synthetic HID
//! input, and finally reads the element back and reports the write as not applied if it does not
//! hold the text.
use glass_core::accessibility::{
    Accessibility, AxContext, AxNodeId, AxRect, AxTarget, AxTree, Located,
};
use std::time::Duration;

use glass_core::{
    Deadline, GlassError, KeyEvent, MouseButton, PointerEvent, Result, TAP_MAY_HAVE_MISSED, Whose,
    read_back_failed, verify_typed_write,
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

#[derive(Clone, Copy)]
enum SemanticPhase {
    Snapshot,
    Invoke,
    SetValue { dispatched: bool },
}

impl SemanticPhase {
    fn expired(self) -> GlassError {
        match self {
            Self::Snapshot => GlassError::AccessibilityNotReady(
                "no accessibility tree within the time this call allowed".into(),
            ),
            Self::Invoke => GlassError::deadline_not_started("native accessibility invoke"),
            Self::SetValue { dispatched: false } => {
                GlassError::deadline_not_started("native accessibility set_value")
            }
            Self::SetValue { dispatched: true } => {
                GlassError::caller_deadline_elapsed("native accessibility set_value")
            }
        }
    }

    fn require(self, deadline: Deadline) -> Result<()> {
        if deadline.has_passed() {
            Err(self.expired())
        } else {
            Ok(())
        }
    }

    fn finish<T>(self, deadline: Deadline, result: Result<T>) -> Result<T> {
        self.require(deadline)?;
        match result {
            Ok(value) => Ok(value),
            Err(error) if error.bound_owner() == Some(Whose::Caller) => Err(self.expired()),
            Err(error) => Err(error),
        }
    }

    fn run<T>(self, deadline: Deadline, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.require(deadline)?;
        self.finish(deadline, work())
    }
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
    fn scale(&mut self, deadline: Deadline, phase: SemanticPhase) -> Result<f64> {
        phase.require(deadline)?;
        if let Some(scale) = self.scale {
            phase.require(deadline)?;
            return Ok(scale);
        }
        let dimensions = phase.finish(deadline, self.client.describe_by(deadline))?;
        let scale = phase.run(deadline, || {
            crate::platform::checked_scale(dimensions.density)
        })?;
        self.scale = Some(scale);
        phase.require(deadline)?;
        Ok(scale)
    }

    /// One describe round-trip: fetch the accessibility JSON and map the id-assigned tree at
    /// the target's scale. Returns the scale alongside it, since `set_value` places synthetic
    /// input at the same one.
    fn describe(&mut self, ctx: &AxContext, phase: SemanticPhase) -> Result<(AxTree, f64)> {
        phase.require(ctx.deadline)?;
        let scale = self.scale(ctx.deadline, phase)?;
        let json = phase.finish(ctx.deadline, self.client.describe_all_by(ctx.deadline))?;
        let tree = phase.run(ctx.deadline, || {
            let mut tree = axmap::build_tree(&json, scale, &ctx.window, ctx.limits)?;
            tree.assign_ids();
            Ok(tree)
        })?;
        phase.require(ctx.deadline)?;
        Ok((tree, scale))
    }
}

/// Locate `target` in a tree described for the write and return its window-relative pixel bounds —
/// the guard every write runs before it dispatches.
///
/// Re-resolved by identity via [`AxTarget::relocate`], not by pre-order id alone: on the Simulator
/// a soft keyboard raised by an earlier write inserts nodes ahead of the field, renumbering it
/// without moving it (glass#361).
///
/// Bounds and value are then checked on whichever node was reached — do not drop them again.
/// [`Located::AtId`] is granted on role+name plus that same unstable id, and role and name repeat
/// across a form's rows and are absent on an unlabelled field, so identity alone accepts a
/// neighbour and the read-back confirms the text in it. Overlap, not equality, so a field the
/// keyboard shifted is still written where it now is.
///
/// Refusals are pre-dispatch: [`AxTarget::drift_error`] for a node the tree cannot place or that no
/// longer looks like what was addressed, `AxElementNotEditable` / `AxElementNotClickable` for one
/// that is the element but cannot be written or tapped.
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
    // Load-bearing, not belt-and-braces: nothing upstream rejects a non-editable target, so without
    // this a Label addressed by mistake is tapped and the text goes to whatever had focus.
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
const _: () = assert!(
    VERIFY_ATTEMPTS > 0,
    "set_value reports the last read-back, so there must be one"
);

fn bounded_sleep_at(deadline: Deadline, requested: Duration, now: std::time::Instant) -> Duration {
    deadline
        .remaining_at(now)
        .unwrap_or(requested)
        .min(requested)
}

/// The write's keystrokes as one batch of HID events: select-all, delete, then the text.
///
/// Do not send these a group at a time again: each `IdbClient::hid` is its own RPC, and the
/// Simulator drops typed text arriving a fraction of a second after the field is cleared
/// (glass#363).
fn clear_and_type_keys(injector: &IdbInjector, text: &str) -> Result<Vec<proto::HidEvent>> {
    let mut events = injector.key_events(&KeyEvent::Chord("super+a".into()))?;
    events.extend(injector.key_events(&KeyEvent::Chord("Delete".into()))?);
    events.extend(injector.key_events(&KeyEvent::Text(text.to_string()))?);
    Ok(events)
}

/// Send the whole write: the focusing tap, then the keystrokes, in exactly two calls.
///
/// Over a `send` seam so the call count — the fix for glass#363 — is testable without a device.
///
/// The batch is built before the tap goes out: `KeyEvent::Text` refuses a non-ASCII character, and
/// building afterwards spent a tap that had already moved first responder.
///
/// The second send's failure is post-dispatch: the batch clears before it types, so a stream dying
/// part-way through leaves the field emptied with the text lost.
fn dispatch_write(
    send: &mut dyn FnMut(Vec<proto::HidEvent>) -> Result<()>,
    injector: &IdbInjector,
    tap: &PointerEvent,
    target_id: u32,
    text: &str,
    deadline: Deadline,
) -> Result<()> {
    require_set_value_time(deadline, false)?;
    let keys = clear_and_type_keys(injector, text)?;
    require_set_value_time(deadline, false)?;
    send(injector.pointer_events(tap)?)?;
    require_set_value_time(deadline, true)?;
    send(keys).map_err(|e| {
        if e.bound_owner() == Some(Whose::Caller) {
            return e;
        }
        GlassError::AxWriteUnconfirmed(
            target_id,
            format!("sending the keystrokes failed part-way through ({e}), so the field may have been cleared without receiving the text"),
        )
    })?;
    require_set_value_time(deadline, true)
}

fn require_set_value_time(deadline: Deadline, dispatched: bool) -> Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    Err(if dispatched {
        GlassError::caller_deadline_elapsed("iOS accessibility set_value")
    } else {
        GlassError::deadline_not_started("iOS accessibility set_value")
    })
}

impl Accessibility for IosA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        Ok(self.describe(ctx, SemanticPhase::Snapshot)?.0)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let before_dispatch = SemanticPhase::SetValue { dispatched: false };
        before_dispatch.require(ctx.deadline)?;
        // One describe serves both the guard and the injector's scale — no second read before the
        // keystrokes go out.
        let (tree, scale) = self.describe(ctx, before_dispatch)?;
        let bounds = before_dispatch.run(ctx.deadline, || verify(&tree, target))?;
        let (cx, cy) = before_dispatch.run(ctx.deadline, || {
            bounds
                .clamped_center(ctx.window.width, ctx.window.height)
                .ok_or(GlassError::AxElementNotClickable(target.id.0))
        })?;
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
        // The tap keeps its own call: a delay here is harmless where one inside the keystrokes
        // loses the text.
        let client = &self.client;
        dispatch_write(
            &mut |events| client.hid_by(events, ctx.deadline),
            &injector,
            &tap,
            target.id.0,
            text,
            ctx.deadline,
        )?;

        // A failure of this read is not a failure of the write — the field has already been cleared
        // and typed into — so it says so rather than letting a caller retry blindly and type twice.
        let after_dispatch = SemanticPhase::SetValue { dispatched: true };
        let mut last = None;
        for _ in 0..VERIFY_ATTEMPTS {
            after_dispatch.require(ctx.deadline)?;
            let sleep = bounded_sleep_at(ctx.deadline, VERIFY_SETTLE, std::time::Instant::now());
            std::thread::sleep(sleep);
            after_dispatch.require(ctx.deadline)?;
            let (after, _) = self.describe(ctx, after_dispatch).map_err(|error| {
                if error.bound_owner() == Some(Whose::Caller) {
                    error
                } else {
                    read_back_failed(target, &error)
                }
            })?;
            match after_dispatch.run(ctx.deadline, || {
                verify_typed_write(&after, target, text, TAP_MAY_HAVE_MISSED)
            }) {
                Ok(()) => return Ok(()),
                // Only a not-applied verdict can change on a later describe: drift and truncation
                // are structural, so re-describing for them reaches the same answer more slowly.
                Err(e @ GlassError::AxValueNotApplied { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        // The const assert on `VERIFY_ATTEMPTS` is what makes `last` always set; the fallback only
        // avoids an unwrap.
        after_dispatch.require(ctx.deadline)?;
        Err(last.unwrap_or_else(|| GlassError::value_not_applied(target.id.0, text, None)))
    }

    fn invoke(&mut self, ctx: &AxContext, _target: &AxTarget) -> Result<Option<AxNodeId>> {
        SemanticPhase::Invoke.run(ctx.deadline, || Err(GlassError::AxUnsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree};

    fn ctx(deadline: glass_core::Deadline) -> AxContext {
        AxContext {
            pids: vec![],
            window: glass_core::WindowGeometry {
                x: 0,
                y: 0,
                width: 200,
                height: 200,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: glass_core::WalkLimits::DEFAULT,
            deadline,
        }
    }

    #[test]
    fn snapshot_with_a_spent_deadline_starts_no_describe() {
        let mut a11y = IosA11y::new(IdbClient::for_test());

        let error = a11y
            .snapshot(&ctx(glass_core::Deadline::from_millis(0)))
            .expect_err("a spent snapshot deadline must stop before idb describe");

        assert!(
            matches!(error, GlassError::AccessibilityNotReady(_)),
            "{error}"
        );
    }

    #[test]
    fn invoke_with_a_spent_deadline_is_not_dispatched() {
        let mut a11y = IosA11y::new(IdbClient::for_test());

        let error = a11y
            .invoke(
                &ctx(glass_core::Deadline::from_millis(0)),
                &matching_target(),
            )
            .expect_err("a spent invoke deadline must win over pointer fallback");

        assert_eq!(error.bound(), Some(glass_core::BoundKind::NotStarted));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn set_value_with_a_spent_deadline_starts_no_describe_or_hid() {
        let mut a11y = IosA11y::new(IdbClient::for_test());

        let error = a11y
            .set_value(
                &ctx(glass_core::Deadline::from_millis(0)),
                &matching_target(),
                "new",
            )
            .expect_err("a spent set_value deadline must stop before idb describe");

        assert_eq!(error.bound(), Some(glass_core::BoundKind::NotStarted));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn verification_sleep_is_capped_by_the_absolute_caller_deadline() {
        let now = std::time::Instant::now();
        let left = Duration::from_millis(5);

        assert_eq!(
            bounded_sleep_at(
                glass_core::Deadline::at(now + left),
                Duration::from_millis(300),
                now,
            ),
            left
        );
    }

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
        // Three calls reinstates glass#363: each is its own RPC, and a pause between the clear and
        // the text loses the text.
        let injector = IdbInjector::new(2.0);
        let mut log = Vec::new();
        dispatch_write(
            &mut recording_send(&mut log),
            &injector,
            &a_tap(),
            1,
            "hi",
            glass_core::Deadline::UNBOUNDED,
        )
        .unwrap();

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
        let err = dispatch_write(
            &mut send,
            &injector,
            &a_tap(),
            1,
            "hi",
            glass_core::Deadline::UNBOUNDED,
        )
        .unwrap_err();
        assert_eq!(calls, 1, "the keystrokes must not follow a tap that failed");
        assert!(
            !err.set_value_failed_after_writing(),
            "nothing was typed, so the session must keep its cached value: {err}"
        );
    }

    #[test]
    fn keystrokes_lost_part_way_report_the_field_may_be_cleared() {
        // The batch clears before it types, so a stream dying between the two leaves the field
        // empty — a bare transport error would read as "nothing was sent".
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
        let err = dispatch_write(
            &mut send,
            &injector,
            &a_tap(),
            7,
            "hi",
            glass_core::Deadline::UNBOUNDED,
        )
        .unwrap_err();
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(7, _)), "{err}");
        assert!(
            err.set_value_failed_after_writing(),
            "the session must drop the value it cached: {err}"
        );
        assert!(err.to_string().contains("may have been cleared"), "{err}");
    }

    #[test]
    fn text_the_backend_cannot_type_refuses_before_anything_is_sent() {
        // Under the old order the clear had already emptied the field by the time the text was
        // found to be untypeable.
        let injector = IdbInjector::new(2.0);
        let mut log = Vec::new();
        let err = dispatch_write(
            &mut recording_send(&mut log),
            &injector,
            &a_tap(),
            1,
            "café",
            glass_core::Deadline::UNBOUNDED,
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
    fn deadline_expiring_after_the_focus_tap_prevents_the_keystroke_batch() {
        let injector = IdbInjector::new(1.0);
        let deadline = glass_core::Deadline::from_millis(5);
        let mut sends = 0;
        let mut send = |_events| {
            sends += 1;
            if sends == 1 {
                while !deadline.has_passed() {
                    std::thread::yield_now();
                }
            }
            Ok(())
        };

        let error = dispatch_write(&mut send, &injector, &a_tap(), 1, "hi", deadline)
            .expect_err("the keystrokes must not begin after the shared deadline expires");

        assert_eq!(sends, 1, "the keystroke batch started after expiry");
        assert_eq!(error.bound_owner(), Some(glass_core::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn caller_timeout_from_the_keystroke_stream_keeps_its_dispatch_ownership() {
        let injector = IdbInjector::new(1.0);
        let mut sends = 0;
        let mut send = |_events| {
            sends += 1;
            if sends == 1 {
                Ok(())
            } else {
                Err(GlassError::caller_deadline_elapsed("idb hid"))
            }
        };

        let error = dispatch_write(&mut send, &injector, &a_tap(), 1, "hi", Deadline::UNBOUNDED)
            .expect_err("the caller-owned HID timeout must propagate without AxWriteUnconfirmed");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn an_ordinary_verification_error_observed_after_expiry_stays_caller_owned() {
        let deadline = glass_core::Deadline::at(std::time::Instant::now());

        let error = SemanticPhase::SetValue { dispatched: true }
            .finish(
                deadline,
                Err::<(), _>(GlassError::AxElementChanged(matching_target().id.0)),
            )
            .expect_err("expiry after HID dispatch must override an ordinary verification error");

        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::MayHaveDispatched)
        );
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
        // Assert content and order, not a length total: a total passes with the two swapped, which
        // on a device deletes one character instead of the field.
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
        // glass#361, the measured case: the soft keyboard inserted nodes ahead of the field between
        // the caller's snapshot and the write's describe, so its id now resolves elsewhere.
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
        // The tap goes to the rect this returns, so it must be the node's and not the target's.
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
        // an id the write itself perturbs is not identification — and the read-back, resolving the
        // same way, would read the text back out of the wrong node and call it a success.
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
        // differs.
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
        // A tap that navigated to another screen carrying a same-named field — refused after the
        // write, so it must be refused before one goes out.
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
