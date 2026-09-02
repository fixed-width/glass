use super::*;
use crate::session::test_support::*;
use crate::{
    AxNode, AxNodeId, AxRect, AxRole, AxStateCoverage, AxStates, AxTree, ChangeSignal,
    SemanticSelector, SemanticState, Truncation, TruncationLimit, WalkLimits,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn selector(query: &str) -> SemanticSelector {
    SemanticSelector::new(Some(query.into()), None, Vec::new()).unwrap()
}

fn semantic_target(query: &str) -> SemanticTarget {
    SemanticTarget {
        target: selector(query),
        within: None,
    }
}

fn named_button_tree(name: &str) -> AxTree {
    let mut tree = fake_tree();
    tree.root.children[0].name = Some(name.into());
    tree
}

fn duplicate_button_tree(name: &str, count: usize) -> AxTree {
    let mut tree = named_button_tree(name);
    let button = tree.root.children[0].clone();
    tree.root.children = vec![button; count];
    tree
}

fn scoped_tree() -> AxTree {
    fn node(role: AxRole, name: &str, children: Vec<AxNode>) -> AxNode {
        AxNode {
            id: AxNodeId(0),
            role,
            raw_role: format!("{role:?}"),
            name: Some(name.into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 20,
                height: 20,
            }),
            children,
        }
    }

    AxTree::new(node(
        AxRole::Window,
        "App",
        vec![
            node(
                AxRole::Group,
                "Account panel",
                vec![node(AxRole::Button, "Save account", Vec::new())],
            ),
            node(
                AxRole::Group,
                "Account panel",
                vec![node(AxRole::Button, "Save other", Vec::new())],
            ),
        ],
    ))
}

fn signals_once() -> Box<dyn ChangeSignal> {
    Box::new(SignalsOnce(true))
}

fn never_signals() -> Box<dyn ChangeSignal> {
    Box::new(NeverSignals)
}

fn semantic_glass(
    trees: Vec<AxTree>,
    signal: Option<fn() -> Box<dyn ChangeSignal>>,
    behavior: InvokeBehavior,
) -> (
    Glass,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<AxTarget>>>,
) {
    let walks = Arc::new(AtomicUsize::new(0));
    let focus_calls = Arc::new(AtomicUsize::new(0));
    let invoke_log = Arc::new(Mutex::new(Vec::new()));
    let accessibility = SeqAccessibility::new(trees)
        .with_coverage(full_state_coverage())
        .with_signal(signal)
        .with_invoke_behavior(behavior)
        .with_invoke_log(invoke_log.clone())
        .with_walks(walks.clone())
        .with_focus_calls(focus_calls.clone());
    let glass = glass_with_backend(FakePlatform::new(100, 100), Box::new(accessibility));
    (glass, walks, focus_calls, invoke_log)
}

#[test]
fn public_semantic_action_enums_have_stable_snake_case_labels() {
    assert_eq!(ActionTarget::Id(AxNodeId(7)).as_str(), "id");
    assert_eq!(
        ActionTarget::Semantic(semantic_target("save")).as_str(),
        "semantic"
    );
    assert_eq!(ActionMode::Auto.as_str(), "auto");
    assert_eq!(ActionMode::Native.as_str(), "native");
    assert_eq!(ActionMode::Pointer.as_str(), "pointer");
    assert_eq!(
        ActionMethod::NativeAction { actuated: None }.as_str(),
        "native_action"
    );
    assert_eq!(
        ActionMethod::Pointer {
            native_fallback: None
        }
        .as_str(),
        "pointer"
    );
    assert_eq!(
        ActionMethod::AccessibilityValue.as_str(),
        "accessibility_value"
    );
    assert_eq!(ActionMethod::Keyboard.as_str(), "keyboard");
    assert_eq!(DispatchStatus::NotDispatched.as_str(), "not_dispatched");
    assert_eq!(DispatchStatus::Dispatched.as_str(), "dispatched");
    assert_eq!(
        DispatchStatus::MayHaveDispatched.as_str(),
        "may_have_dispatched"
    );
    assert_eq!(ConfirmationStatus::NotRequested.as_str(), "not_requested");
    assert_eq!(
        ConfirmationStatus::DispatchConfirmed.as_str(),
        "dispatch_confirmed"
    );
    assert_eq!(
        ConfirmationStatus::FocusConfirmed.as_str(),
        "focus_confirmed"
    );
    assert_eq!(
        ConfirmationStatus::ValueConfirmed.as_str(),
        "value_confirmed"
    );
    assert_eq!(ConfirmationStatus::Unconfirmed.as_str(), "unconfirmed");
    assert_eq!(SemanticActionFailureKind::NoMatch.as_str(), "no_match");
    assert_eq!(
        SemanticActionFailureKind::AmbiguousTarget.as_str(),
        "ambiguous_target"
    );
    assert_eq!(
        SemanticActionFailureKind::AmbiguousScope.as_str(),
        "ambiguous_scope"
    );
    assert_eq!(
        SemanticActionFailureKind::IncompleteTree.as_str(),
        "incomplete_tree"
    );
    assert_eq!(
        SemanticActionFailureKind::UnprovenSelectorState.as_str(),
        "unproven_selector_state"
    );
    assert_eq!(
        SemanticActionFailureKind::NotActionable.as_str(),
        "not_actionable"
    );
    assert_eq!(
        SemanticActionFailureKind::UnstableTarget.as_str(),
        "unstable_target"
    );
    assert_eq!(
        SemanticActionFailureKind::FocusUnconfirmed.as_str(),
        "focus_unconfirmed"
    );
    assert_eq!(
        SemanticActionFailureKind::UnsupportedMode.as_str(),
        "unsupported_mode"
    );
    assert_eq!(
        SemanticActionFailureKind::ActionDeadlineExceeded.as_str(),
        "action_deadline_exceeded"
    );
    assert_eq!(
        SemanticActionFailureKind::SequenceDeadlineExceeded.as_str(),
        "sequence_deadline_exceeded"
    );
    assert_eq!(
        SemanticActionFailureKind::ActionFailed.as_str(),
        "action_failed"
    );
    assert_eq!(RetryGuidance::CorrectRequest.as_str(), "correct_request");
    assert_eq!(RetryGuidance::WaitOrRefine.as_str(), "wait_or_refine");
    assert_eq!(RetryGuidance::Reobserve.as_str(), "reobserve");
    assert_eq!(RetryGuidance::SafeToRetry.as_str(), "safe_to_retry");
    assert_eq!(RetryGuidance::DoNotRetry.as_str(), "do_not_retry");
}

#[test]
fn semantic_action_error_display_never_exposes_retained_payloads() {
    let error = SemanticActionError {
        kind: SemanticActionFailureKind::ActionFailed,
        summary: "semantic action failed",
        resolution: None,
        actionability: ActionabilityReport::default(),
        focus: Some(MutationReport {
            method: ActionMethod::Pointer {
                native_fallback: Some("mutation-secret".into()),
            },
            dispatch: DispatchStatus::MayHaveDispatched,
            confirmation: ConfirmationStatus::Unconfirmed,
        }),
        action_dispatch: DispatchStatus::MayHaveDispatched,
        candidates: Vec::new(),
        bound: ActionDeadline {
            deadline: Deadline::UNBOUNDED,
            owner: None,
            allow_wait: true,
        },
        retry: RetryGuidance::DoNotRetry,
        source: Some(GlassError::Backend("source-secret".into())),
    };

    assert_eq!(error.to_string(), "semantic action failed");
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn selector_resolution_reads_fresh_and_caches_the_unique_complete_tree() {
    let stale = named_button_tree("Save old");
    let fresh = named_button_tree("Save account");
    let (mut glass, walks, _, _) =
        semantic_glass(vec![stale, fresh], None, InvokeBehavior::Unsupported);
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(None).unwrap();

    let resolved = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 2);
    assert_eq!(resolved.element.id, AxNodeId(1));
    assert_eq!(resolved.element.name.as_deref(), Some("Save account"));
    assert_eq!(resolved.target.id, AxNodeId(1));
    assert_eq!(resolved.target.name.as_deref(), Some("Save account"));
    assert_eq!(
        glass
            .active
            .as_ref()
            .unwrap()
            .last_ax
            .as_ref()
            .unwrap()
            .root
            .children[0]
            .name
            .as_deref(),
        Some("Save account")
    );
}

#[test]
fn selector_resolution_waits_for_delayed_publication() {
    let first = named_button_tree("Save old");
    let second = named_button_tree("Save account");
    let (mut glass, walks, _, _) = semantic_glass(
        vec![first, second],
        Some(signals_once),
        InvokeBehavior::Unsupported,
    );
    glass.start(&spec()).unwrap();

    let resolved = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            500,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 2);
    assert_eq!(resolved.element.name.as_deref(), Some("Save account"));
    assert!(resolved.bound.allow_wait);
}

#[test]
fn duplicate_targets_never_resolve_to_the_first_ranked_match() {
    let tree = duplicate_button_tree("Save account", 7);
    let (mut glass, walks, mutation_calls, _) =
        semantic_glass(vec![tree], Some(never_signals), InvokeBehavior::Unsupported);
    glass.start(&spec()).unwrap();

    let error = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::AmbiguousTarget);
    assert_eq!(error.resolution.as_ref().unwrap().matches_in_walk, 7);
    assert_eq!(error.candidates.len(), SEMANTIC_ACTION_CANDIDATE_LIMIT);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(mutation_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn ambiguous_scope_never_falls_back_to_the_whole_window() {
    let target = SemanticTarget {
        target: selector("Save account"),
        within: Some(selector("Account panel")),
    };
    let (mut glass, walks, mutation_calls, _) =
        semantic_glass(vec![scoped_tree()], None, InvokeBehavior::Unsupported);
    glass.start(&spec()).unwrap();

    let error = glass
        .resolve_semantic_target(&target, None, 0, Deadline::UNBOUNDED, |_, _| true)
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::AmbiguousScope);
    assert!(error.candidates.is_empty());
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(mutation_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn one_match_in_an_incomplete_tree_does_not_prove_uniqueness() {
    let mut tree = named_button_tree("Save account");
    tree.truncated = Some(Truncation {
        limit: TruncationLimit::Nodes,
        limit_value: 2,
        nodes_walked: 2,
    });
    let (mut glass, walks, mutation_calls, _) =
        semantic_glass(vec![tree], None, InvokeBehavior::Unsupported);
    glass.start(&spec()).unwrap();

    let error = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::IncompleteTree);
    let report = error.resolution.unwrap();
    assert_eq!(report.matches_in_walk, 1);
    assert!(!report.search_complete);
    assert!(report.tree_truncated);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(mutation_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn uncovered_requested_state_fails_before_the_first_tree_read() {
    let walks = Arc::new(AtomicUsize::new(0));
    let accessibility = SeqAccessibility::new(vec![named_button_tree("Save account")])
        .with_coverage(AxStateCoverage::NONE)
        .with_walks(walks.clone());
    let mut glass = glass_with_backend(FakePlatform::new(100, 100), Box::new(accessibility));
    glass.start(&spec()).unwrap();
    let target = SemanticTarget {
        target: SemanticSelector::new(
            Some("Save account".into()),
            None,
            vec![SemanticState::Visible],
        )
        .unwrap(),
        within: None,
    };

    let error = glass
        .resolve_semantic_target(&target, None, 0, Deadline::UNBOUNDED, |_, _| true)
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::UnprovenSelectorState);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(walks.load(Ordering::Relaxed), 0);
}

#[test]
fn zero_timeout_performs_exactly_one_fresh_read() {
    let (mut glass, walks, _, _) = semantic_glass(
        vec![
            named_button_tree("Save old"),
            named_button_tree("Save account"),
        ],
        Some(signals_once),
        InvokeBehavior::Unsupported,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .resolve_semantic_target(
            &semantic_target("missing"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NoMatch);
    assert!(!error.bound.allow_wait);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
}

#[test]
fn zero_timeout_native_click_can_dispatch_without_starting_a_wait() {
    let (mut glass, walks, _, invoke_log) = semantic_glass(
        vec![named_button_tree("Save account")],
        Some(never_signals),
        InvokeBehavior::Succeed,
    );
    glass.start(&spec()).unwrap();

    let resolved = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap();
    assert!(!resolved.bound.allow_wait);
    assert_eq!(walks.load(Ordering::Relaxed), 1);

    glass.click_element(resolved.target.id).unwrap();

    assert_eq!(invoke_log.lock().unwrap().len(), 1);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
}

#[test]
fn omitted_max_nodes_installs_walk_limits_default_instead_of_reusing_the_session_cap() {
    let (mut glass, walks, _, _) = semantic_glass(
        vec![
            named_button_tree("Save old"),
            named_button_tree("Save account"),
        ],
        None,
        InvokeBehavior::Unsupported,
    );
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(Some(1)).unwrap();
    assert_eq!(
        glass.active.as_ref().unwrap().a11y_limits,
        WalkLimits::from_max_nodes(Some(1))
    );

    glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap();

    assert_eq!(
        glass.active.as_ref().unwrap().a11y_limits,
        WalkLimits::DEFAULT
    );
    assert_eq!(walks.load(Ordering::Relaxed), 2);
}

#[test]
fn known_ineligible_target_is_classified_after_unique_complete_resolution() {
    let (mut glass, walks, _, _) = semantic_glass(
        vec![named_button_tree("Save account")],
        None,
        InvokeBehavior::Unsupported,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            0,
            Deadline::UNBOUNDED,
            |_, _| false,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.candidates.len(), 1);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
}

#[test]
fn target_deadlines_validate_id_options_and_selector_timeout_ceiling() {
    let id_error = target_deadline(
        &ActionTarget::Id(AxNodeId(1)),
        Some(1),
        None,
        Deadline::UNBOUNDED,
    )
    .unwrap_err();
    assert_eq!(id_error.kind, SemanticActionFailureKind::UnsupportedMode);
    assert_eq!(id_error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(id_error.retry, RetryGuidance::CorrectRequest);

    let selector_error = target_deadline(
        &ActionTarget::Semantic(semantic_target("save")),
        Some(SEMANTIC_ACTION_MAX_TIMEOUT_MS + 1),
        None,
        Deadline::UNBOUNDED,
    )
    .unwrap_err();
    assert_eq!(
        selector_error.kind,
        SemanticActionFailureKind::UnsupportedMode
    );
    assert_eq!(
        selector_error.action_dispatch,
        DispatchStatus::NotDispatched
    );
    assert_eq!(selector_error.retry, RetryGuidance::CorrectRequest);
}

#[test]
fn target_deadlines_preserve_id_and_selector_bound_ownership() {
    let standalone = target_deadline(
        &ActionTarget::Id(AxNodeId(1)),
        None,
        None,
        Deadline::UNBOUNDED,
    )
    .unwrap();
    assert_eq!(standalone.deadline, Deadline::UNBOUNDED);
    assert_eq!(standalone.owner, None);
    assert!(standalone.allow_wait);

    let sequence = Deadline::from_millis(500);
    let batched = target_deadline(&ActionTarget::Id(AxNodeId(1)), None, None, sequence).unwrap();
    assert_eq!(batched.deadline, sequence);
    assert_eq!(batched.owner, Some(Whose::Caller));
    assert!(batched.allow_wait);

    let defaulted = target_deadline(
        &ActionTarget::Semantic(semantic_target("save")),
        None,
        None,
        Deadline::UNBOUNDED,
    )
    .unwrap();
    let remaining = defaulted.deadline.remaining().unwrap();
    assert!(remaining <= std::time::Duration::from_millis(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS));
    assert!(remaining > std::time::Duration::from_millis(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS - 100));
    assert_eq!(defaulted.owner, Some(Whose::Callee));
    assert!(defaulted.allow_wait);

    let zero = target_deadline(
        &ActionTarget::Semantic(semantic_target("save")),
        Some(0),
        None,
        Deadline::UNBOUNDED,
    )
    .unwrap();
    assert_eq!(zero.deadline, Deadline::UNBOUNDED);
    assert_eq!(zero.owner, None);
    assert!(!zero.allow_wait);

    let sequence = Deadline::from_millis(500);
    let sequence_limited = target_deadline(
        &ActionTarget::Semantic(semantic_target("save")),
        Some(1_000),
        None,
        sequence,
    )
    .unwrap();
    assert_eq!(sequence_limited.deadline, sequence);
    assert_eq!(sequence_limited.owner, Some(Whose::Caller));
    assert!(sequence_limited.allow_wait);
}
