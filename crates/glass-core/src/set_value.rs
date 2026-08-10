//! Judging whether a `set_value` write actually took, from the value read back afterwards.
//!
//! Every backend that writes a value faces the same problem: the platform can accept the write
//! without error and change nothing. A UIA `SetValue` on an egui read-only `TextEdit` returns
//! success and leaves the buffer alone; an AX write to a read-only-in-practice editable does the
//! same; a tap-and-type on a mobile field can land somewhere else entirely. Reporting `Ok` on any
//! of those is a false success — the agent believes a field holds text it does not hold.
//!
//! Two rules live here, because two kinds of write need different evidence.
//!
//! An *atomic platform write* (UIA `SetValue`, AX `AXValue`) either happens or does not, and may be
//! reformatted on the way in — a slider set to `"50"` reads back `"50.0"`. So a value that merely
//! differs from the old one is evidence it landed: [`read_back_confirms`]. The macOS and Windows
//! readers had each grown their own copy of that rule; it is theirs.
//!
//! A *typed write* (tap, select-all, delete, type) is not atomic. A dropped key, a `maxLength`, an
//! input filter or autocorrect all leave the field holding something that is neither the request nor
//! the old value, and calling that success is the very failure this module exists to prevent. So a
//! typed write must read back exactly what was asked: [`typed_text_landed`]. Android's on-device
//! service reader reached the same conclusion independently (`a11y_service.rs`).
//!
//! Two write paths deliberately do not use either rule: the AT-SPI (Linux) reader trusts the
//! toolkit's own `set_text_contents` answer without reading back, and Android's service reader keeps
//! its own exact-match loop.
//!
//! [`verify_typed_write`] applies the typed rule to a whole tree read back after the keystrokes, and
//! is what the tap-and-type backends (Android's `uiautomator` reader, iOS) call.

use crate::Result;
use crate::accessibility::{AxTarget, AxTree, Located};
use crate::error::GlassError;

/// Whether a `set_value` write took, judged from the value read back.
///
/// It took iff the read-back equals the request OR differs from the pre-set value. The second half
/// matters because a real write may be reformatted on the way in — a slider set to `"50"` reads
/// back `"50.0"` — while a write that silently did nothing reads back exactly what was there
/// before.
fn set_value_took(before: &str, after: &str, requested: &str) -> bool {
    after == requested || after != before
}

/// Whether a read-back can *confirm* the write took. `read_back` is the value read afterwards
/// (`None` when that read failed or the value was absent); `before` is the pre-write baseline
/// (`None` when that read failed — the baseline is unknown).
///
/// Confirms only when it can prove the write landed:
/// - a failed post-write read is inconclusive and never confirms;
/// - with a known baseline it delegates to [`set_value_took`];
/// - with an unknown baseline only an exact match with the request confirms, because "differs from
///   before" means nothing without a baseline to differ from.
///
/// That last rule is the guard against a *failed read* passing for a change: a reader that collapsed
/// both reads to `""` once reported false success for exactly that reason.
pub fn read_back_confirms(read_back: Option<&str>, before: Option<&str>, requested: &str) -> bool {
    match (read_back, before) {
        (None, _) => false,
        (Some(after), Some(before)) => set_value_took(before, after, requested),
        (Some(after), None) => after == requested,
    }
}

/// Whether a read-back shows the element holding exactly what it held before the write — the one
/// outcome that means the write never took effect, as against arriving and being transformed.
///
/// The caller passes a reading it actually took, with an absent value already normalized to `""`:
/// a mapper that drops empty values reports an empty field as no value, and a backend whose
/// read-back *failed* has no reading to compare and must not ask.
///
/// What a backend's own explanation of a write hangs on — attach it here and nowhere else, or a
/// field that transformed the text is handed a remedy for one that never received it.
pub fn write_took_no_effect(observed: &str, before: Option<&str>) -> bool {
    before.is_some_and(|b| b == observed)
}

/// Whether a *typed* write landed, judged from the value read back.
///
/// Exact match, unlike [`read_back_confirms`]: keystrokes can land partially, and a field holding
/// `"worl"` after `"world"` was typed has not received the write, even though it differs from what
/// was there before.
///
/// `read_back` is `None` when the element reports no value, which for a text field means it is
/// empty — the `uiautomator` and `idb` mappers both drop an empty value — so it compares as `""`.
///
/// The cost of the strictness: a field that reformats what it is given (a phone number becoming
/// `"(123) 456-7890"`) reports the write as not applied even though it landed. That is the safer
/// direction — [`crate::GlassError::AxValueNotApplied`] names the value it holds, whereas a false
/// success has an agent asserting against a screen that never changed.
///
/// Not for a clear: see [`typed_clear_landed`].
pub fn typed_text_landed(read_back: Option<&str>, requested: &str) -> bool {
    read_back.unwrap_or("") == requested
}

/// Whether a *clear* — a typed write of `""` — landed, judged from the value read back.
///
/// Empty only. Do not weaken this to "empty, or the value changed": the tap that begins a clear can
/// itself change a field that reformats on focus — a currency field showing `"$1,234.00"` unfocused
/// and `"1234.00"` while editing — so "changed" would confirm a clear whose delete never fired.
///
/// The cost is a false *failure*: on the dogfood AVD an emptied `EditText` reports its placeholder as
/// its text (`text="Search settings"`, and `uiautomator` exposes no separate hint attribute), so a
/// clear that worked reads as not applied. [`crate::GlassError::AxValueNotApplied`] names the
/// placeholder it read back; the other direction would have an agent believing an unchanged field
/// was cleared.
pub fn typed_clear_landed(read_back: Option<&str>) -> bool {
    read_back.unwrap_or("").is_empty()
}

/// What a write that took no effect looks like for a typed write: the tap that aims the keystrokes
/// is the part that can miss.
pub const TAP_MAY_HAVE_MISSED: &str = "this write is a tap and then keystrokes, so the tap may have missed the element — writing \
     again is worth one attempt";

/// The read-back's own failure, reported as an unconfirmed write.
///
/// For a read-back loop only, which is what makes it post-dispatch: the keystrokes have gone out by
/// the time a caller reaches this, so a verdict [`GlassError::set_value_failed_after_writing`]
/// rejects would leave the session holding the value it cached for a write that already landed, and
/// refuse the retry as drift.
pub fn read_back_failed(target: &AxTarget, e: &GlassError) -> GlassError {
    GlassError::AxWriteUnconfirmed(target.id.0, format!("reading the element back failed: {e}"))
}

/// Judge a typed `set_value` from a tree read back after it. `Ok(())` only when the field holds
/// exactly what was asked for, or — for a named field — reads back empty after a clear.
///
/// Exact match, not "changed from before": tap-and-type is not atomic, so a dropped key or an input
/// filter leaves the field holding something that is neither the request nor the old value, and
/// calling that success is the failure this check exists to prevent. [`typed_text_landed`] and
/// [`typed_clear_landed`] carry the rules and the cost of them.
///
/// The element is re-resolved by identity (role + name) via [`AxTarget::relocate`], not by its
/// pre-order id. Both tap-and-type backends need that, for different reasons. On the iOS Simulator
/// the focusing tap raises the soft keyboard and inserts nodes ahead of the field (glass#359), so
/// the id itself is not stable across the write. On Android the soft keyboard does not shift ids —
/// measured on the dogfood AVD with `mInputShown=true`, `uiautomator dump` emits the focused window
/// only and no IME window — but a tap that navigates does: Settings' search entry opens a different
/// window, and the field can turn up renumbered inside it rather than gone.
///
/// Bounds are deliberately no part of that identity, unlike a pre-write locate: the IME reflows the
/// layout under the field it is typing into, so a moved-but-correct element is the normal case here.
/// `relocate` does require a re-found node to still *overlap* where the target was, which is what
/// separates that reflow from a field on another screen entirely — and the Android reader needs it
/// most, because it names an editable from its resource-id leaf whenever `content-desc` is absent,
/// which is most of the time. A leaf like `search_src_text` names a *layout*: it repeats across
/// every instance of it, and across apps, so a navigating tap can find the next screen's field
/// matching uniquely. Overlap is checked here rather than left to [`Located::Moved`] because the
/// write's own tap renumbers the tree onto a neighbouring row, which [`Located::AtId`] accepts
/// without consulting bounds and which reads back empty like a landed clear.
///
/// The node must also still be editable: a non-editable node can carry its label in `value`, so a
/// label that changed inside the settle window would otherwise read as a successful write into a
/// Label. [`Located::AtId`] and [`Located::Moved`] both already carry the target's role and name, so
/// a node reached here that is not editable is not a neighbour that inherited the id — it is the
/// identified element having lost editability. Every refusal here is post-dispatch, hence
/// [`GlassError::AxWriteUnconfirmed`] throughout rather than [`AxTarget::drift_error`], which
/// answers for a locate made before the write went out.
///
/// A clear (`text` empty) is confirmed only for a target that carries a name. An empty read-back is
/// what any untouched same-role, same-name field would also show, so it is evidence of the write
/// only inasmuch as the identity that found the field is discriminating — a non-empty write's
/// exact-text check grounds itself, a clear has nothing to. A nameless target is refused however it
/// was reached, its own id included: [`AxTarget::matches`] compares `None` to `None`, so `AtId`
/// there accepts whatever nameless editable renumbering slid onto the id.
pub fn verify_typed_write(after_tree: &AxTree, target: &AxTarget, text: &str) -> Result<()> {
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
        // The mobile mappers drop an empty value, so an empty field arrives as `None` — which the
        // verdict reserves for a reading nobody took.
        let observed = node.value.as_deref().unwrap_or("");
        // Only where the field never moved: on a value the element transformed — an iOS field
        // autocapitalizing a typed lowercase letter (glass#363) — the tap landed, so "write again"
        // would contradict the sentence this clause hangs off.
        Err(
            if write_took_no_effect(observed, Some(target.value.as_deref().unwrap_or(""))) {
                GlassError::value_not_applied_because(
                    target.id.0,
                    text,
                    Some(observed),
                    TAP_MAY_HAVE_MISSED,
                )
            } else {
                GlassError::value_not_applied(target.id.0, text, Some(observed))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_still_holding_what_it_held_took_no_effect_from_the_write() {
        assert!(write_took_no_effect("hello", Some("hello")));
    }

    #[test]
    fn a_field_holding_anything_else_took_something_from_the_write() {
        // Including a transformation: an iOS field that autocapitalized the text it was given
        // received every keystroke, so a backend's "the tap may have missed" must not attach.
        assert!(!write_took_no_effect("Hello", Some("hello")));
    }

    #[test]
    fn a_read_back_with_no_baseline_cannot_show_a_write_took_no_effect() {
        // `None` is a pre-write read the backend could not take. Treating it as a match would
        // attach an explanation of a write that never landed to one nobody can place.
        assert!(!write_took_no_effect("hello", None));
        assert!(!write_took_no_effect("", None));
    }

    #[test]
    fn a_write_that_changed_nothing_did_not_take() {
        // A read-only editable that accepts the write and keeps its value.
        assert!(!set_value_took("#000000", "#000000", "#12AA34"));
    }

    #[test]
    fn a_read_back_equal_to_the_request_took() {
        assert!(set_value_took("#000000", "#12AA34", "#12AA34"));
    }

    #[test]
    fn a_reformatted_value_still_took() {
        // A slider set to "50" may read back "50.0" — changed from before, so it landed.
        assert!(set_value_took("0", "50.0", "50"));
    }

    #[test]
    fn writing_the_value_it_already_holds_counts_as_taken() {
        // Nothing can distinguish this from a no-op, and reporting failure for a write that asked
        // for the value already present would be the more misleading answer.
        assert!(set_value_took("50", "50", "50"));
    }

    #[test]
    fn a_typed_write_must_read_back_exactly() {
        assert!(typed_text_landed(Some("world"), "world"));
        // A dropped keystroke: differs from the old value, but the write did not land.
        assert!(!typed_text_landed(Some("worl"), "world"));
        assert!(!typed_text_landed(Some("hello"), "world"));
    }

    #[test]
    fn a_clear_is_confirmed_only_by_an_empty_field() {
        assert!(typed_clear_landed(None));
        assert!(typed_clear_landed(Some("")));
        // A clear that did not fire.
        assert!(!typed_clear_landed(Some("glass")));
        // The cost, pinned as a decision: a platform that reports an emptied field's placeholder
        // reads as not applied.
        assert!(!typed_clear_landed(Some("Search settings")));
    }

    #[test]
    fn a_typed_write_of_text_still_needs_an_exact_match() {
        assert!(typed_text_landed(None, ""));
        assert!(!typed_text_landed(None, "world"));
    }

    #[test]
    fn an_unchanged_value_does_not_confirm_an_atomic_write() {
        // The arm with no false case before: a read-back equal to the baseline and unequal to the
        // request is the no-op this predicate exists to catch.
        assert!(!read_back_confirms(Some("hello"), Some("hello"), "world"));
    }

    #[test]
    fn a_failed_post_read_never_confirms() {
        // Inconclusive is not success: the caller reports `AxValueNotApplied` rather than guess.
        assert!(!read_back_confirms(None, Some("hello"), "world"));
    }

    #[test]
    fn a_change_from_a_known_baseline_confirms() {
        assert!(read_back_confirms(Some("50.0"), Some("0"), "50"));
        assert!(read_back_confirms(Some("0.0"), Some("0"), "0"));
    }

    #[test]
    fn a_mere_difference_cannot_confirm_when_the_baseline_is_unknown() {
        // The regression this rule exists for: a failed pre-read defaulting to "" made a no-op that
        // reads back its real value look "changed", and the write was reported as successful.
        assert!(!read_back_confirms(Some("hello"), None, "world"));
    }

    #[test]
    fn an_exact_match_confirms_even_with_an_unknown_baseline() {
        assert!(read_back_confirms(Some("world"), None, "world"));
    }

    /// The tree-level verdict: which node the read-back found, and whether it holds the request.
    ///
    /// These are the tap-and-type backends' contract, tested here because the rule is theirs
    /// jointly — the Android `uiautomator` reader and the iOS reader each used to carry a copy of
    /// the function and of most of these cases.
    mod typed_write {
        use crate::Result;
        use crate::accessibility::{
            AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree, Located, Truncation,
            TruncationLimit,
        };
        use crate::error::GlassError;
        use crate::set_value::{TAP_MAY_HAVE_MISSED, read_back_failed, verify_typed_write};

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
                raw_role: String::new(),
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

        /// The same field and target with no name, the identity that nothing but position grounds.
        fn nameless(value: Option<&str>) -> (AxTree, AxTarget) {
            let mut field = leaf(0, AxRole::TextField, "Note", FIELD);
            field.name = None;
            field.value = value.map(Into::into);
            let mut target = matching_target();
            target.name = None;
            (tree_with(vec![field]), target)
        }

        /// Pin the whole verdict, not the variant: the two values it names are what a caller acts
        /// on (glass#363), and `why` attaches only to a field that never moved (glass#405).
        #[track_caller]
        fn assert_not_applied(
            outcome: Result<()>,
            id: u32,
            requested: &str,
            observed: Option<&str>,
            why: Option<&str>,
        ) {
            match outcome {
                Err(GlassError::AxValueNotApplied {
                    id: got_id,
                    requested: req,
                    observed: obs,
                    why: got_why,
                }) => assert_eq!(
                    (got_id, req.as_str(), obs.as_deref(), got_why),
                    (id, requested, observed, why)
                ),
                other => panic!("expected a not-applied verdict, got {other:?}"),
            }
        }
        #[test]
        fn a_write_that_landed_is_reported_as_success() {
            let after = tree_with_value(Some("world"));
            assert!(verify_typed_write(&after, &matching_target(), "world").is_ok());
        }

        #[test]
        fn clearing_a_field_is_a_write_that_can_succeed() {
            // An empty field reports no value at all, so a rule that read `None` as "unknown" made
            // `set_value(id, "")` fail every time.
            let after = tree_with_value(None);
            assert!(verify_typed_write(&after, &matching_target(), "").is_ok());
        }

        #[test]
        fn a_cleared_field_still_reporting_text_reads_as_not_applied() {
            // A clear is judged by "reads back empty", not by "the value changed": the tap that begins
            // the clear can itself change a field that reformats on focus, and accepting that would
            // confirm a clear whose delete never fired. The fixture is the measured Android case —
            // the AVD does empty the field and `uiautomator` then reports its hint as its text.
            let after = tree_with_value(Some("Search settings"));
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), ""),
                1,
                "",
                Some("Search settings"),
                None,
            );
        }

        #[test]
        fn a_truncated_read_back_says_so_rather_than_blaming_drift() {
            // The iOS tree carries the same truncation record Android's does; reporting "it moved" would
            // throw away the one fact that explains the absence. No node here carries the target's role
            // and name, so a complete tree would call this Gone — the cap is what keeps it Unproven.
            let mut after = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
            after.truncated = Some(Truncation {
                limit: TruncationLimit::Nodes,
                limit_value: 1,
                nodes_walked: 1,
            });
            let err = verify_typed_write(&after, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(err.to_string().contains("truncated at 1 nodes"), "{err}");
        }

        #[test]
        fn a_field_that_never_changed_is_not_a_successful_write() {
            let after = tree_with_value(Some("hello"));
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), "world"),
                1,
                "world",
                Some("hello"),
                None,
            );
        }

        #[test]
        fn an_autocapitalized_first_letter_is_not_a_successful_write() {
            // glass#363: iOS applies sentence autocapitalization to a value typed into a field it
            // considers empty, so every keystroke lands and the field holds the request with its first
            // letter uppercased. Do not relax the match to ignore case — that reports a write as landed
            // while the field holds different bytes.
            let after = tree_with_value(Some("Glasssmoke3"));
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), "glasssmoke3"),
                1,
                "glasssmoke3",
                Some("Glasssmoke3"),
                None,
            );
        }

        #[test]
        fn an_empty_field_reports_an_empty_read_back_not_a_missing_one() {
            // `axmap` drops an empty value, so an emptied field arrives as `None` — which must still
            // report as empty, not as a reading nobody took.
            let after = tree_with_value(None);
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), "world"),
                1,
                "world",
                Some(""),
                // The field held nothing before and holds nothing now, so the write took no effect and
                // the backend's own explanation of that is what closes the message.
                Some(TAP_MAY_HAVE_MISSED),
            );
        }

        #[test]
        fn a_partly_typed_write_is_not_a_successful_write() {
            // A dropped keystroke: differs from the old value, so "changed from before" would have
            // called it success.
            let after = tree_with_value(Some("worl"));
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), "world"),
                1,
                "world",
                Some("worl"),
                None,
            );
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
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), "world"),
                1,
                "world",
                Some("hello"),
                None,
            );
        }

        #[test]
        fn a_node_that_lost_editability_is_unconfirmed_not_changed() {
            // Not the `AxElementNotEditable` the pre-write `verify` raises: a caller reading "not
            // dispatched" off that would retype into whatever the element collapsed into.
            let mut after = tree_with_value(Some("world"));
            after.root.children[0].states.editable = false;
            let err = verify_typed_write(&after, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(err.to_string().contains("editable"), "{err}");
        }

        #[test]
        fn a_named_field_clears_after_being_re_found() {
            // The write's own focusing tap inserts nodes ahead of the field, so a clear into a field
            // that was not already focused ALWAYS relocates. Keying the refusal on that made it fail by
            // construction; the name is what identifies the field, and the name survives the move.
            let moved = leaf(0, AxRole::TextField, "Note", FIELD);
            let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
            assert!(verify_typed_write(&after, &matching_target(), "").is_ok());
        }

        #[test]
        fn a_nameless_field_cannot_confirm_a_clear_at_its_own_id() {
            // `AxTarget::matches` compares None to None, so the id alone accepts whatever nameless
            // editable renumbering slid onto it — and an empty read-back is exactly what that one shows
            // too. Nothing but the position says the keystrokes reached this field.
            let (after, target) = nameless(None);
            let err = verify_typed_write(&after, &target, "").unwrap_err();
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
            let err = verify_typed_write(&after, &target, "").unwrap_err();
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
            assert!(verify_typed_write(&after, &target, "world").is_ok());
        }

        #[test]
        fn losing_editability_outranks_the_clear_refusal() {
            // Both refusals fit a nameless non-editable node. Editability is the specific fact, and a
            // caller told only "an empty field cannot confirm a clear" would go on hunting for a field.
            let (mut after, target) = nameless(None);
            after.root.children[0].states.editable = false;
            let err = verify_typed_write(&after, &target, "").unwrap_err();
            assert!(err.to_string().contains("editable"), "{err}");
        }

        #[test]
        fn a_field_drawn_clear_of_the_target_does_not_confirm_the_write() {
            // Role and name pick out exactly one node, and it is nowhere near where the element was —
            // the shape of a tap that navigated, where the keystrokes went to another screen's field.
            let mut moved = leaf(0, AxRole::TextField, "Note", AxRect { y: 600, ..FIELD });
            moved.value = Some("world".into());
            let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
            let err = verify_typed_write(&after, &matching_target(), "world").unwrap_err();
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
            after.truncated = Some(Truncation {
                limit: TruncationLimit::Nodes,
                limit_value: 3,
                nodes_walked: 3,
            });
            let err = verify_typed_write(&after, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(err.to_string().contains("truncated at 3 nodes"), "{err}");
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
            let err = verify_typed_write(&after, &matching_target(), "").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(err.to_string().contains("drawn clear of where"), "{err}");
        }

        #[test]
        fn a_write_confirmed_at_a_new_id_is_a_success() {
            // glass#359, the measured case: the keyboard renumbered the field and the text landed.
            let mut moved = leaf(0, AxRole::TextField, "Note", FIELD);
            moved.value = Some("world".into());
            let after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
            assert!(verify_typed_write(&after, &matching_target(), "world").is_ok());
        }

        #[test]
        fn an_id_past_the_end_still_confirms_a_landed_write() {
            // The mirror image of the renumbering case: the keyboard dismissing after a landed write
            // can shrink the tree back down, leaving the field's old id past the end of it — nothing
            // resolves there, but the field itself never moved and still holds the text.
            let after = tree_with_value(Some("world"));
            let mut t = matching_target();
            t.id = AxNodeId(9);
            assert!(verify_typed_write(&after, &t, "world").is_ok());
        }

        #[test]
        fn a_write_that_replaced_the_screen_says_it_was_typed_but_unconfirmed() {
            // glass#323: a teardown can land between the write and the read-back, which is where
            // four of the nine refusals observed there came from.
            let replaced = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
            // A capped tree would answer Unproven, not Gone; pin the fixture so a future change to
            // `tree_with` cannot silently move this test onto the wrong arm and still pass.
            assert!(
                replaced.is_complete(),
                "a capped tree would answer Unproven, not Gone"
            );
            let err = verify_typed_write(&replaced, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            // "the write went out" is common to every AxWriteUnconfirmed arm (it's in the #[error]
            // template, not this arm's own clause) — assert the Gone arm's own sentence too, or a
            // change to just this arm's message would leave the suite green.
            assert!(err.to_string().contains("the write went out"), "{err}");
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
            let err = verify_typed_write(&after, &matching_target(), "world").unwrap_err();
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
            replaced.truncated = Some(Truncation {
                limit: TruncationLimit::Nodes,
                limit_value: 2,
                nodes_walked: 2,
            });
            let mut t = matching_target();
            t.id = AxNodeId(15);
            let err = verify_typed_write(&replaced, &t, "world").unwrap_err();
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
            replaced.truncated = Some(Truncation {
                limit: TruncationLimit::Nodes,
                limit_value: 2,
                nodes_walked: 2,
            });
            let err = verify_typed_write(&replaced, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(err.to_string().contains("truncated at 2 nodes"), "{err}");
        }

        #[test]
        fn an_incomplete_tree_does_not_claim_the_element_vanished() {
            let mut replaced = tree_with(vec![leaf(0, AxRole::Label, "No Results", FIELD)]);
            replaced.unreadable = 1;
            let err = verify_typed_write(&replaced, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(
                !err.to_string().contains("replaced"),
                "an incomplete read must not assert the screen changed: {err}"
            );
        }

        #[test]
        fn a_field_that_reformats_what_it_was_given_reports_not_applied() {
            // The cost of the strictness on the other backend, pinned so it is a decision rather
            // than a surprise: an Android mask that turns "1234567890" into "(123) 456-7890" reads
            // as not applied. An agent can re-read the tree and see the value; a false success
            // would have it asserting against the wrong text.
            let after = tree_with_value(Some("(123) 456-7890"));
            assert_not_applied(
                verify_typed_write(&after, &matching_target(), "1234567890"),
                1,
                "1234567890",
                Some("(123) 456-7890"),
                None,
            );
        }

        #[test]
        fn an_unreadable_subtree_cannot_confirm_a_write_against_its_one_match() {
            // An unreadable subtree is the other place a second match can hide, and its remedy is
            // not a raised cap, so its message must not collapse into the drifted-element one.
            let mut moved = leaf(0, AxRole::TextField, "Note", FIELD);
            moved.value = Some("world".into());
            let mut after = tree_with(vec![leaf(0, AxRole::Label, "Suggestions", FIELD), moved]);
            after.unreadable = 1;
            let err = verify_typed_write(&after, &matching_target(), "world").unwrap_err();
            assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
            assert!(err.to_string().contains("could not be read"), "{err}");
        }

        #[test]
        fn a_failed_read_back_is_an_unconfirmed_write_naming_what_failed() {
            // The read runs after the keystrokes went out, so a transport failure here must be a
            // verdict `set_value_failed_after_writing` accepts — otherwise the session keeps the
            // value it cached for a write that already landed and refuses the retry as drift.
            let e = GlassError::AccessibilityUnavailable("no window roots".into());
            let out = read_back_failed(&matching_target(), &e);
            assert!(matches!(out, GlassError::AxWriteUnconfirmed(1, _)), "{out}");
            assert!(out.set_value_failed_after_writing(), "{out}");
            assert!(out.to_string().contains("no window roots"), "{out}");
        }
    }
}
