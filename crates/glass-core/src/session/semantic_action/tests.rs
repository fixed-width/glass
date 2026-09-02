use super::*;
use crate::session::test_support::*;
use crate::{
    Accessibility, ActionabilityCheck, ActionabilityCheckName, ActionabilitySource,
    ActionabilityVerdict, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStateCoverage, AxStates,
    AxTree, ChangeSignal, PointerHit, SemanticSelector, SemanticState, Truncation, TruncationLimit,
    WalkLimits,
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

fn actionable_button_tree(name: &str, bounds: AxRect) -> AxTree {
    let mut tree = named_button_tree(name);
    let button = &mut tree.root.children[0];
    button.bounds = Some(bounds);
    button.states.enabled = true;
    button.states.visible = true;
    tree
}

fn pointer_params(target: ActionTarget, timeout_ms: Option<u64>) -> ClickTargetParams {
    ClickTargetParams {
        target,
        mode: ActionMode::Pointer,
        timeout_ms,
        max_nodes: None,
    }
}

fn pointer_glass(
    platform: FakePlatform,
    trees: Vec<AxTree>,
    hit: PointerHit,
    hit_error: bool,
) -> (
    Glass,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<(i32, i32)>>>,
) {
    let walks = Arc::new(AtomicUsize::new(0));
    let hit_calls = Arc::new(AtomicUsize::new(0));
    let hit_points = Arc::new(Mutex::new(Vec::new()));
    let mut accessibility = SeqAccessibility::new(trees)
        .with_coverage(full_state_coverage())
        .with_hit(hit)
        .with_walks(walks.clone())
        .with_hit_calls(hit_calls.clone())
        .with_hit_points(hit_points.clone());
    if hit_error {
        accessibility = accessibility.with_hit_error();
    }
    (
        glass_with_backend(platform, Box::new(accessibility)),
        walks,
        hit_calls,
        hit_points,
    )
}

fn report_verdict(
    outcome: &SemanticActionOutcome,
    name: ActionabilityCheckName,
) -> ActionabilityVerdict {
    outcome
        .actionability
        .checks
        .iter()
        .find(|check| check.name == name)
        .expect("missing actionability check")
        .verdict
}

fn error_report_check(
    error: &SemanticActionError,
    name: ActionabilityCheckName,
) -> ActionabilityCheck {
    error
        .actionability
        .checks
        .iter()
        .copied()
        .find(|check| check.name == name)
        .expect("missing actionability check")
}

fn assert_pointer_report_order(error: &SemanticActionError) {
    assert_eq!(
        error
            .actionability
            .checks
            .iter()
            .map(|check| check.name)
            .collect::<Vec<_>>(),
        vec![
            ActionabilityCheckName::Unique,
            ActionabilityCheckName::Enabled,
            ActionabilityCheckName::Visible,
            ActionabilityCheckName::Stable,
            ActionabilityCheckName::InWindow,
            ActionabilityCheckName::NonOccluded,
            ActionabilityCheckName::BackendFingerprint,
        ]
    );
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

struct DeadlineRecordingAccessibility {
    tree: AxTree,
    coverage_delay: std::time::Duration,
    coverage_finished: Arc<Mutex<Option<std::time::Instant>>>,
    subscription_deadlines: Arc<Mutex<Vec<Deadline>>>,
    snapshot_deadlines: Arc<Mutex<Vec<Deadline>>>,
}

impl Accessibility for DeadlineRecordingAccessibility {
    fn snapshot(&mut self, ctx: &AxContext) -> crate::Result<AxTree> {
        self.snapshot_deadlines.lock().unwrap().push(ctx.deadline);
        Ok(self.tree.clone())
    }

    fn subscribe_changes(&mut self, ctx: &AxContext) -> Option<Box<dyn ChangeSignal>> {
        self.subscription_deadlines
            .lock()
            .unwrap()
            .push(ctx.deadline);
        None
    }

    fn state_coverage(&self) -> AxStateCoverage {
        std::thread::sleep(self.coverage_delay);
        *self.coverage_finished.lock().unwrap() = Some(std::time::Instant::now());
        full_state_coverage()
    }
}

fn deadline_recording_glass(
    coverage_delay: std::time::Duration,
) -> (
    Glass,
    Arc<Mutex<Option<std::time::Instant>>>,
    Arc<Mutex<Vec<Deadline>>>,
    Arc<Mutex<Vec<Deadline>>>,
) {
    let coverage_finished = Arc::new(Mutex::new(None));
    let subscription_deadlines = Arc::new(Mutex::new(Vec::new()));
    let snapshot_deadlines = Arc::new(Mutex::new(Vec::new()));
    let accessibility = DeadlineRecordingAccessibility {
        tree: named_button_tree("Save account"),
        coverage_delay,
        coverage_finished: coverage_finished.clone(),
        subscription_deadlines: subscription_deadlines.clone(),
        snapshot_deadlines: snapshot_deadlines.clone(),
    };
    (
        glass_with_backend(FakePlatform::new(100, 100), Box::new(accessibility)),
        coverage_finished,
        subscription_deadlines,
        snapshot_deadlines,
    )
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
fn selector_resolution_uses_the_reported_deadline_for_subscription_and_snapshot() {
    let (mut glass, _, subscription_deadlines, snapshot_deadlines) =
        deadline_recording_glass(std::time::Duration::ZERO);
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

    assert_eq!(
        subscription_deadlines.lock().unwrap().as_slice(),
        &[resolved.bound.deadline]
    );
    assert_eq!(
        snapshot_deadlines.lock().unwrap().as_slice(),
        &[resolved.bound.deadline]
    );
}

#[test]
fn selector_resolution_setup_consumes_the_reported_action_budget() {
    let timeout = std::time::Duration::from_millis(300);
    let (mut glass, coverage_finished, subscription_deadlines, snapshot_deadlines) =
        deadline_recording_glass(std::time::Duration::from_millis(150));
    glass.start(&spec()).unwrap();

    let resolved = glass
        .resolve_semantic_target(
            &semantic_target("Save account"),
            None,
            timeout.as_millis() as u64,
            Deadline::UNBOUNDED,
            |_, _| true,
        )
        .unwrap();

    let coverage_finished = coverage_finished.lock().unwrap().unwrap();
    let remaining_after_setup = resolved
        .bound
        .deadline
        .instant()
        .unwrap()
        .saturating_duration_since(coverage_finished);
    assert!(
        remaining_after_setup < std::time::Duration::from_millis(225),
        "pre-poll setup did not consume the action budget: {remaining_after_setup:?} remained"
    );
    assert_eq!(
        subscription_deadlines.lock().unwrap().as_slice(),
        &[resolved.bound.deadline]
    );
    assert_eq!(
        snapshot_deadlines.lock().unwrap().as_slice(),
        &[resolved.bound.deadline]
    );
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

#[test]
fn selector_pointer_waits_for_two_identical_samples_at_least_one_hundred_ms_apart() {
    let bounds = AxRect {
        x: 10,
        y: 10,
        width: 20,
        height: 20,
    };
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, walks, hit_calls, _) = pointer_glass(
        platform,
        vec![
            actionable_button_tree("Save", bounds),
            actionable_button_tree("Save", bounds),
        ],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let started = std::time::Instant::now();
    let outcome = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    assert_eq!(walks.load(Ordering::Relaxed), 2);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 1);
    assert_eq!(clicks.lock().unwrap().as_slice(), &[(20, 20)]);
    assert_eq!(
        report_verdict(&outcome, ActionabilityCheckName::Stable),
        ActionabilityVerdict::Passed
    );
}

#[test]
fn moving_bounds_reset_the_stability_sample_until_the_target_stops() {
    let first = AxRect {
        x: 10,
        y: 10,
        width: 20,
        height: 20,
    };
    let final_bounds = AxRect { x: 40, ..first };
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, walks, _, _) = pointer_glass(
        platform,
        vec![
            actionable_button_tree("Save", first),
            actionable_button_tree("Save", final_bounds),
            actionable_button_tree("Save", final_bounds),
        ],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let started = std::time::Instant::now();
    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(600)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert!(started.elapsed() >= std::time::Duration::from_millis(200));
    assert_eq!(walks.load(Ordering::Relaxed), 3);
    assert_eq!(clicks.lock().unwrap().as_slice(), &[(50, 20)]);
}

#[test]
fn identity_change_resets_stability_even_when_bounds_are_equal() {
    let bounds = AxRect {
        x: 10,
        y: 10,
        width: 20,
        height: 20,
    };
    let mut first = actionable_button_tree("Save", bounds);
    first.root.children[0].description = Some("old identity".into());
    let mut final_tree = actionable_button_tree("Save", bounds);
    final_tree.root.children[0].description = Some("new identity".into());
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, walks, _, _) = pointer_glass(
        platform,
        vec![first, final_tree.clone(), final_tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(600)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 3);
    assert_eq!(clicks.lock().unwrap().len(), 1);
}

#[test]
fn selector_pointer_builds_each_stability_sample_from_its_refreshed_window() {
    let bounds = AxRect {
        x: 10,
        y: 10,
        width: 20,
        height: 20,
    };
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let narrow = WindowGeometry {
        x: 0,
        y: 0,
        width: 15,
        height: 100,
    };
    let full = WindowGeometry {
        width: 100,
        ..narrow.clone()
    };
    let platform = FakePlatform::new(100, 100)
        .with_click_log(clicks.clone())
        .resized_to(narrow)
        .resized_to(full.clone())
        .resized_to(full);
    let tree = actionable_button_tree("Save", bounds);
    let (mut glass, walks, _, hit_points) = pointer_glass(
        platform,
        vec![tree.clone(), tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(600)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 3);
    assert_eq!(hit_points.lock().unwrap().as_slice(), &[(20, 20)]);
    assert_eq!(clicks.lock().unwrap().as_slice(), &[(20, 20)]);
}

#[test]
fn selector_pointer_revalidates_the_exact_plan_against_the_dispatch_window() {
    let bounds = AxRect {
        x: 80,
        y: 10,
        width: 20,
        height: 20,
    };
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let full = WindowGeometry {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let narrow = WindowGeometry {
        width: 15,
        ..full.clone()
    };
    let platform = FakePlatform::new(100, 100)
        .with_click_log(clicks.clone())
        .resized_to(full.clone())
        .resized_to(full)
        .resized_to(narrow);
    let tree = actionable_button_tree("Save", bounds);
    let (mut glass, _, hit_calls, _) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        error_report_check(&error, ActionabilityCheckName::InWindow).verdict,
        ActionabilityVerdict::Failed
    );
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
    assert!(clicks.lock().unwrap().is_empty());
}

#[test]
fn auto_native_discloses_the_post_resolution_window_geometry() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 80,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let platform = FakePlatform::new(100, 100).resized_to(WindowGeometry {
        x: 0,
        y: 0,
        width: 15,
        height: 100,
    });
    let invoke_log = Arc::new(Mutex::new(Vec::new()));
    let accessibility = SeqAccessibility::new(vec![tree])
        .with_coverage(full_state_coverage())
        .with_invoke_behavior(InvokeBehavior::Succeed)
        .with_invoke_log(invoke_log.clone());
    let mut glass = glass_with_backend(platform, Box::new(accessibility));
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_inner(
            ClickTargetParams {
                target: ActionTarget::Semantic(semantic_target("Save")),
                mode: ActionMode::Auto,
                timeout_ms: Some(0),
                max_nodes: None,
            },
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(invoke_log.lock().unwrap().len(), 1);
    let in_window = outcome
        .actionability
        .checks
        .iter()
        .find(|check| check.name == ActionabilityCheckName::InWindow)
        .unwrap();
    assert_eq!(in_window.verdict, ActionabilityVerdict::Failed);
    assert!(!in_window.required);
}

#[test]
fn selector_pointer_disabled_target_preserves_the_ordered_blocking_report() {
    let mut tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    tree.root.children[0].states.enabled = false;
    let (mut glass, _, hit_calls, _) = pointer_glass(
        FakePlatform::new(100, 100),
        vec![tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(0)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_pointer_report_order(&error);
    assert_eq!(
        error.actionability.blocking().unwrap().name,
        ActionabilityCheckName::Enabled
    );
    let stable = error_report_check(&error, ActionabilityCheckName::Stable);
    assert_eq!(stable.verdict, ActionabilityVerdict::Unproven);
    assert!(stable.required);
    assert_eq!(stable.source, ActionabilitySource::GeometrySamples);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn selector_pointer_hidden_target_preserves_the_ordered_blocking_report() {
    let mut tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    tree.root.children[0].states.visible = false;
    let (mut glass, _, hit_calls, _) = pointer_glass(
        FakePlatform::new(100, 100),
        vec![tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(0)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_pointer_report_order(&error);
    assert_eq!(
        error.actionability.blocking().unwrap().name,
        ActionabilityCheckName::Visible
    );
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn selector_pointer_off_window_target_preserves_the_ordered_blocking_report() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 100,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, _, hit_calls, _) = pointer_glass(
        FakePlatform::new(100, 100),
        vec![tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(0)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_pointer_report_order(&error);
    assert_eq!(
        error.actionability.blocking().unwrap().name,
        ActionabilityCheckName::InWindow
    );
    let in_window = error_report_check(&error, ActionabilityCheckName::InWindow);
    assert_eq!(in_window.verdict, ActionabilityVerdict::Failed);
    assert!(in_window.required);
    assert_eq!(in_window.source, ActionabilitySource::WindowGeometry);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn zero_timeout_native_action_dispatches_after_one_fresh_read() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, walks, _, invoke_log) =
        semantic_glass(vec![tree], None, InvokeBehavior::Succeed);
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_inner(
            ClickTargetParams {
                target: ActionTarget::Semantic(semantic_target("Save")),
                mode: ActionMode::Native,
                timeout_ms: Some(0),
                max_nodes: None,
            },
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(invoke_log.lock().unwrap().len(), 1);
    assert!(matches!(
        outcome.action.method,
        ActionMethod::NativeAction { .. }
    ));
}

#[test]
fn zero_timeout_pointer_action_returns_unstable_without_dispatch() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, walks, hit_calls, _) =
        pointer_glass(platform, vec![tree], PointerHit::Target, false);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(0)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::UnstableTarget);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
    assert!(clicks.lock().unwrap().is_empty());
}

#[test]
fn selector_pointer_known_occlusion_blocks_before_pointer_dispatch() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, _, hit_calls, _) =
        pointer_glass(platform, vec![tree.clone(), tree], PointerHit::Other, false);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 1);
    assert!(clicks.lock().unwrap().is_empty());
}

#[test]
fn selector_pointer_inconclusive_occlusion_dispatches_once_and_discloses_unproven() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, _, hit_calls, _) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Inconclusive,
        false,
    );
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(hit_calls.load(Ordering::Relaxed), 1);
    assert_eq!(clicks.lock().unwrap().len(), 1);
    assert_eq!(
        report_verdict(&outcome, ActionabilityCheckName::NonOccluded),
        ActionabilityVerdict::Unproven
    );
}

#[test]
fn selector_pointer_hit_probe_and_dispatch_use_the_same_planned_point() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: -10,
            y: 10,
            width: 30,
            height: 20,
        },
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, _, _, hit_points) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(hit_points.lock().unwrap().as_slice(), &[(10, 20)]);
    assert_eq!(clicks.lock().unwrap().as_slice(), &[(10, 20)]);
}

#[test]
fn selector_pointer_row_toggle_probes_and_dispatches_the_same_trailing_control() {
    let bounds = AxRect {
        x: 10,
        y: 10,
        width: 80,
        height: 15,
    };
    let mut tree = actionable_button_tree("Wi-Fi", bounds);
    let toggle = &mut tree.root.children[0];
    toggle.role = AxRole::CheckBox;
    toggle.states.checkable = true;
    let drags = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100)
        .with_drag_log(drags.clone())
        .with_trailing_toggle_backend();
    let (mut glass, _, _, hit_points) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Wi-Fi")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    let segment = bounds.trailing_toggle_swipe(100, 100).unwrap();
    let midpoint = (
        (segment.from_x + segment.to_x) / 2,
        (segment.from_y + segment.to_y) / 2,
    );
    assert_eq!(hit_points.lock().unwrap().as_slice(), &[midpoint]);
    let drags = drags.lock().unwrap();
    assert_eq!(drags.len(), 1);
    assert!(matches!(
        drags[0],
        PointerEvent::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            ..
        } if (from_x, from_y, to_x, to_y)
            == (segment.from_x, segment.from_y, segment.to_x, segment.to_y)
    ));
}

#[test]
fn selector_pointer_rejects_a_trailing_toggle_with_a_boundary_endpoint() {
    let bounds = AxRect {
        x: 99,
        y: 99,
        width: 80,
        height: 15,
    };
    let mut tree = actionable_button_tree("Wi-Fi", bounds);
    let toggle = &mut tree.root.children[0];
    toggle.role = AxRole::CheckBox;
    toggle.states.checkable = true;
    let drags = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100)
        .with_drag_log(drags.clone())
        .with_trailing_toggle_backend();
    let (mut glass, _, hit_calls, _) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Wi-Fi")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        error.actionability.blocking().unwrap().name,
        ActionabilityCheckName::InWindow
    );
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
    assert!(drags.lock().unwrap().is_empty());
}

#[test]
fn exact_trailing_toggle_plan_validates_the_stored_probe_point() {
    let segment = crate::Segment {
        from_x: 10,
        from_y: 20,
        to_x: 30,
        to_y: 20,
    };
    let valid = PlannedPointerInput::TrailingToggle {
        segment,
        probe_point: (20, 20),
    };
    let invalid = PlannedPointerInput::TrailingToggle {
        segment,
        probe_point: (100, 100),
    };

    assert!(valid.is_inside_window((100, 100)));
    assert!(!invalid.is_inside_window((100, 100)));
}

#[test]
fn selector_pointer_popover_row_toggle_dispatches_the_translated_planned_segment() {
    let mut tree = fake_tree_with_popover_option();
    let toggle = &mut tree.root.children[0].children[0];
    toggle.role = AxRole::CheckBox;
    toggle.states.enabled = true;
    toggle.states.visible = true;
    toggle.states.checkable = true;
    toggle.bounds.as_mut().unwrap().width = 200;
    let bounds = toggle.bounds.unwrap();
    let active = window_info(
        1,
        WindowGeometry {
            x: 0,
            y: 0,
            width: 340,
            height: 300,
        },
        true,
    );
    let popover = window_info(
        2,
        WindowGeometry {
            x: -3,
            y: 220,
            width: 326,
            height: 135,
        },
        false,
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let drags = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(340, 300)
        .with_windows(vec![active, popover])
        .with_click_log(clicks.clone())
        .with_drag_log(drags.clone())
        .with_trailing_toggle_backend();
    let (mut glass, _, _, hit_points) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Globex")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    let segment = bounds.trailing_toggle_swipe(340, 300).unwrap();
    let probe_point = (
        (segment.from_x + segment.to_x) / 2,
        (segment.from_y + segment.to_y) / 2,
    );
    assert_eq!(hit_points.lock().unwrap().as_slice(), &[probe_point]);
    assert!(clicks.lock().unwrap().is_empty());
    let drags = drags.lock().unwrap();
    assert_eq!(drags.len(), 1);
    assert!(matches!(
        drags[0],
        PointerEvent::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            ..
        } if (from_x, from_y, to_x, to_y)
            == (
                segment.from_x,
                segment.from_y - 194,
                segment.to_x,
                segment.to_y - 194,
            )
    ));
}

#[test]
fn legacy_id_forced_pointer_dispatches_without_a_fresh_read_or_stability_sleep() {
    let tree = fake_tree();
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, walks, hit_calls, _) =
        pointer_glass(platform, vec![tree], PointerHit::Other, false);
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(None).unwrap();
    walks.store(0, Ordering::Relaxed);
    hit_calls.store(0, Ordering::Relaxed);
    clicks.lock().unwrap().clear();

    let started = std::time::Instant::now();
    let outcome = glass
        .click_target_inner(
            pointer_params(ActionTarget::Id(AxNodeId(1)), None),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert!(started.elapsed() < std::time::Duration::from_millis(75));
    assert_eq!(walks.load(Ordering::Relaxed), 0);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
    assert_eq!(clicks.lock().unwrap().len(), 1);
    for name in [
        ActionabilityCheckName::Unique,
        ActionabilityCheckName::Stable,
        ActionabilityCheckName::NonOccluded,
    ] {
        let check = outcome
            .actionability
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap();
        assert_eq!(check.verdict, ActionabilityVerdict::Unproven);
        assert!(!check.required);
    }
}

#[test]
fn selector_pointer_hit_probe_errors_are_action_failed_before_dispatch() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, _, hit_calls, _) = pointer_glass(
        platform,
        vec![tree.clone(), tree],
        PointerHit::Inconclusive,
        true,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(500)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::ActionFailed);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(hit_calls.load(Ordering::Relaxed), 1);
    assert!(clicks.lock().unwrap().is_empty());
}

#[test]
fn native_mode_unsupported_is_proven_not_dispatched() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    let (mut glass, _, _, _) = pointer_glass(platform, vec![tree], PointerHit::Inconclusive, false);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            ClickTargetParams {
                target: ActionTarget::Semantic(semantic_target("Save")),
                mode: ActionMode::Native,
                timeout_ms: Some(0),
                max_nodes: None,
            },
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert!(matches!(error.source, Some(GlassError::AxUnsupported)));
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert!(clicks.lock().unwrap().is_empty());
}

#[test]
fn legacy_id_missing_bounds_preserves_the_primitive_error_before_dispatch() {
    let mut tree = fake_tree();
    tree.root.children[0].bounds = None;
    let (mut glass, _, _, _) = pointer_glass(
        FakePlatform::new(100, 100),
        vec![tree],
        PointerHit::Other,
        false,
    );
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(None).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Id(AxNodeId(1)), None),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert!(matches!(
        error.source,
        Some(GlassError::AxElementNotClickable(1))
    ));
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
}
