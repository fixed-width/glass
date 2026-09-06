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

type SemanticSetValueFixture = (
    Glass,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<(AxNodeId, String)>>>,
    AxDeadlineLog,
);
type PointerFixture = (
    Glass,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<(i32, i32)>>>,
);
type SemanticFixture = (
    Glass,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<AxTarget>>>,
);
type DeadlineRecordingFixture = (
    Glass,
    Arc<Mutex<Option<std::time::Instant>>>,
    Arc<Mutex<Vec<Deadline>>>,
    Arc<Mutex<Vec<Deadline>>>,
);
type ClickModeFixture = (
    Glass,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<(i32, i32)>>>,
    AxDeadlineLog,
);

fn selector(query: &str) -> SemanticSelector {
    SemanticSelector::new(Some(query.into()), None, Vec::new()).unwrap()
}

fn semantic_target(query: &str) -> SemanticTarget {
    SemanticTarget {
        target: selector(query),
        within: None,
    }
}

fn value_control_tree(name: &str, role: AxRole, value: Option<&str>, states: AxStates) -> AxTree {
    AxTree::new(AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "window".into(),
        name: Some("App".into()),
        description: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: 320,
            height: 240,
        }),
        children: vec![AxNode {
            id: AxNodeId(0),
            role,
            raw_role: format!("{role:?}"),
            name: Some(name.into()),
            description: None,
            value: value.map(str::to_owned),
            states,
            bounds: Some(AxRect {
                x: 20,
                y: 20,
                width: 160,
                height: 30,
            }),
            children: Vec::new(),
        }],
    })
}

fn editable_field_tree(name: &str, value: Option<&str>, enabled: bool) -> AxTree {
    value_control_tree(
        name,
        AxRole::TextField,
        value,
        AxStates {
            enabled,
            visible: true,
            focusable: true,
            editable: true,
            ..AxStates::default()
        },
    )
}

fn semantic_set_value_params(query: &str, timeout_ms: u64) -> SetValueTargetParams {
    SetValueTargetParams {
        target: ActionTarget::Semantic(semantic_target(query)),
        timeout_ms: Some(timeout_ms),
        max_nodes: None,
    }
}

fn semantic_set_value_glass(
    trees: Vec<AxTree>,
    set_results: Vec<crate::Result<()>>,
) -> SemanticSetValueFixture {
    semantic_set_value_glass_with_coverage(trees, set_results, full_state_coverage())
}

fn semantic_set_value_glass_with_coverage(
    trees: Vec<AxTree>,
    set_results: Vec<crate::Result<()>>,
    coverage: AxStateCoverage,
) -> SemanticSetValueFixture {
    let walks = Arc::new(AtomicUsize::new(0));
    let set_log = Arc::new(Mutex::new(Vec::new()));
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let accessibility = SeqAccessibility::new(trees)
        .with_coverage(coverage)
        .with_walks(walks.clone())
        .with_deadlines(deadlines.clone())
        .with_set_log(set_log.clone())
        .with_set_results(set_results);
    (
        glass_with_backend(FakePlatform::new(320, 240), Box::new(accessibility)),
        walks,
        set_log,
        deadlines,
    )
}

fn targeted_type_params(query: &str, focus_mode: ActionMode, timeout_ms: u64) -> TypeTargetParams {
    TypeTargetParams {
        target: semantic_target(query),
        focus_mode,
        timeout_ms,
        max_nodes: None,
    }
}

fn targeted_type_field_tree(name: &str, focused: bool) -> AxTree {
    let mut tree = editable_field_tree(name, Some("old"), true);
    tree.root.children[0].id = AxNodeId(1);
    tree.root.children[0].states.focused = focused;
    tree
}

fn renumbered_targeted_type_field_tree(name: &str) -> AxTree {
    let mut tree = targeted_type_field_tree(name, true);
    tree.root.children.insert(
        0,
        AxNode {
            id: AxNodeId(1),
            role: AxRole::Label,
            raw_role: "label".into(),
            name: Some("Inserted helper".into()),
            description: None,
            value: None,
            states: AxStates {
                visible: true,
                ..AxStates::default()
            },
            bounds: Some(AxRect {
                x: 20,
                y: 60,
                width: 160,
                height: 20,
            }),
            children: Vec::new(),
        },
    );
    tree.root.children[1].id = AxNodeId(2);
    AxTree::new(tree.root)
}

struct TargetedTypeFixture {
    glass: Glass,
    focus_calls: Arc<AtomicUsize>,
    clicks: Arc<Mutex<Vec<(i32, i32)>>>,
    key_log: Arc<Mutex<Vec<KeyEvent>>>,
    walks: Arc<AtomicUsize>,
    ax_deadlines: AxDeadlineLog,
    key_deadlines: InputDeadlineLog,
    hit_calls: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<&'static str>>>,
}

fn targeted_type_glass(
    trees: Vec<AxTree>,
    focus_behavior: InvokeBehavior,
    coverage: AxStateCoverage,
    fail_key: bool,
) -> TargetedTypeFixture {
    let focus_calls = Arc::new(AtomicUsize::new(0));
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let key_log = Arc::new(Mutex::new(Vec::new()));
    let walks = Arc::new(AtomicUsize::new(0));
    let ax_deadlines = Arc::new(Mutex::new(Vec::new()));
    let key_deadlines = Arc::new(Mutex::new(Vec::new()));
    let hit_calls = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut platform = FakePlatform::new(320, 240)
        .with_click_log(clicks.clone())
        .with_key_log(key_log.clone())
        .with_key_deadline_log(key_deadlines.clone())
        .with_event_log(events.clone());
    if fail_key {
        platform = platform.with_failing_key();
    }
    let accessibility = SeqAccessibility::new(trees)
        .with_coverage(coverage)
        .with_focus_behavior(focus_behavior)
        .with_focus_calls(focus_calls.clone())
        .with_hit(PointerHit::Target)
        .with_hit_calls(hit_calls.clone())
        .with_walks(walks.clone())
        .with_deadlines(ax_deadlines.clone())
        .with_event_log(events.clone());
    TargetedTypeFixture {
        glass: glass_with_backend(platform, Box::new(accessibility)),
        focus_calls,
        clicks,
        key_log,
        walks,
        ax_deadlines,
        key_deadlines,
        hit_calls,
        events,
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
) -> PointerFixture {
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

fn test_element(role: AxRole, states: AxStates, bounds: AxRect) -> ElementInfo {
    ElementInfo {
        id: AxNodeId(7),
        role,
        name: Some("Target".into()),
        description: Some("test target".into()),
        value: None,
        bounds: Some(bounds),
        states,
    }
}

fn unbounded_action_deadline(owner: Option<Whose>) -> ActionDeadline {
    ActionDeadline {
        deadline: Deadline::UNBOUNDED,
        owner,
        allow_wait: true,
    }
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
) -> SemanticFixture {
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
    coverage_started: Arc<Mutex<Option<std::time::Instant>>>,
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
        *self.coverage_started.lock().unwrap() = Some(std::time::Instant::now());
        std::thread::sleep(self.coverage_delay);
        full_state_coverage()
    }
}

fn deadline_recording_glass(coverage_delay: std::time::Duration) -> DeadlineRecordingFixture {
    let coverage_started = Arc::new(Mutex::new(None));
    let subscription_deadlines = Arc::new(Mutex::new(Vec::new()));
    let snapshot_deadlines = Arc::new(Mutex::new(Vec::new()));
    let accessibility = DeadlineRecordingAccessibility {
        tree: named_button_tree("Save account"),
        coverage_delay,
        coverage_started: coverage_started.clone(),
        subscription_deadlines: subscription_deadlines.clone(),
        snapshot_deadlines: snapshot_deadlines.clone(),
    };
    (
        glass_with_backend(FakePlatform::new(100, 100), Box::new(accessibility)),
        coverage_started,
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
fn semantic_outcomes_and_errors_combine_both_mutation_phases() {
    use DispatchStatus::{Dispatched, MayHaveDispatched, NotDispatched};

    for (focus_dispatch, action_dispatch, expected) in [
        (None, NotDispatched, false),
        (Some(NotDispatched), NotDispatched, false),
        (Some(Dispatched), NotDispatched, true),
        (Some(MayHaveDispatched), NotDispatched, true),
        (None, Dispatched, true),
        (Some(NotDispatched), Dispatched, true),
        (Some(Dispatched), Dispatched, true),
        (Some(MayHaveDispatched), Dispatched, true),
        (None, MayHaveDispatched, true),
        (Some(NotDispatched), MayHaveDispatched, true),
        (Some(Dispatched), MayHaveDispatched, true),
        (Some(MayHaveDispatched), MayHaveDispatched, true),
    ] {
        let focus = focus_dispatch.map(|dispatch| MutationReport {
            method: ActionMethod::NativeAction { actuated: None },
            dispatch,
            confirmation: ConfirmationStatus::Unconfirmed,
        });
        let outcome = SemanticActionOutcome {
            target: ElementInfo::from_node(&fake_tree().root),
            resolution: None,
            actionability: ActionabilityReport::default(),
            focus: focus.clone(),
            action: MutationReport {
                method: ActionMethod::Keyboard,
                dispatch: action_dispatch,
                confirmation: ConfirmationStatus::Unconfirmed,
            },
            bound: unbounded_action_deadline(None),
        };
        let mut error = empty_error(
            SemanticActionFailureKind::ActionFailed,
            "semantic action failed",
            outcome.bound,
            RetryGuidance::Reobserve,
            None,
        );
        error.focus = focus;
        error.action_dispatch = action_dispatch;

        assert_eq!(
            outcome.side_effects_may_have_occurred(),
            expected,
            "outcome: focus={focus_dispatch:?}, action={action_dispatch:?}"
        );
        assert_eq!(
            error.side_effects_may_have_occurred(),
            expected,
            "error: focus={focus_dispatch:?}, action={action_dispatch:?}"
        );
    }
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
        action_method: Some(ActionMethod::Keyboard),
        action_dispatch: DispatchStatus::MayHaveDispatched,
        candidates: Vec::new(),
        target: None,
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
fn source_errors_preserve_deadline_owner_in_kind_and_summary() {
    let cases = [
        (
            GlassError::caller_deadline_elapsed("resolve"),
            unbounded_action_deadline(None),
            SemanticActionFailureKind::SequenceDeadlineExceeded,
            "semantic action sequence deadline exceeded",
        ),
        (
            GlassError::Bounded {
                kind: crate::BoundKind::TimedOut,
                whose: Whose::Callee,
                dispatch: crate::BoundDispatch::NotDispatched,
                message: "resolver ceiling elapsed".into(),
            },
            unbounded_action_deadline(None),
            SemanticActionFailureKind::ActionDeadlineExceeded,
            "semantic action deadline exceeded",
        ),
        (
            GlassError::Backend("reader failed".into()),
            unbounded_action_deadline(None),
            SemanticActionFailureKind::ActionFailed,
            "semantic action target resolution failed",
        ),
    ];

    for (source, bound, kind, summary) in cases {
        let error = source_error(source, bound);
        assert_eq!(error.kind, kind);
        assert_eq!(error.summary, summary);
        assert!(error.source.is_some());
    }
}

#[test]
fn pointer_plan_requires_every_trailing_toggle_fact_and_uses_the_segment_midpoint() {
    let bounds = AxRect {
        x: 11,
        y: 7,
        width: 81,
        height: 15,
    };
    let mut element = test_element(
        AxRole::CheckBox,
        AxStates {
            checkable: true,
            ..AxStates::default()
        },
        bounds,
    );
    assert_eq!(
        pointer_plan(&element, (120, 80), true),
        Some(PlannedPointerInput::TrailingToggle {
            segment: Segment {
                from_x: 66,
                from_y: 14,
                to_x: 88,
                to_y: 14,
            },
            probe_point: (77, 14),
        })
    );

    element.bounds = Some(AxRect {
        x: 10,
        y: 0,
        width: 20,
        height: 1,
    });
    assert_eq!(
        pointer_plan(&element, (120, 80), true),
        Some(PlannedPointerInput::TrailingToggle {
            segment: Segment {
                from_x: 28,
                from_y: 0,
                to_x: 30,
                to_y: 0,
            },
            probe_point: (29, 0),
        })
    );
    element.bounds = Some(bounds);

    element.states.checkable = false;
    assert!(matches!(
        pointer_plan(&element, (120, 80), true),
        Some(PlannedPointerInput::Click { .. })
    ));
    element.states.checkable = true;
    assert!(matches!(
        pointer_plan(&element, (120, 80), false),
        Some(PlannedPointerInput::Click { .. })
    ));
    element.bounds = Some(AxRect {
        width: 60,
        ..bounds
    });
    assert!(matches!(
        pointer_plan(&element, (120, 80), true),
        Some(PlannedPointerInput::Click { .. })
    ));
}

#[test]
fn pointer_candidate_requires_one_retained_match_from_a_complete_resolved_query() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let query = SemanticQuery::new(selector("Save"), None, SEMANTIC_ACTION_CANDIDATE_LIMIT)
        .expect("valid query");
    let complete = tree.semantic_query(&query);
    assert!(complete_unique_pointer_result(&complete));

    let mut unresolved_scope = complete.clone();
    unresolved_scope.scope = ScopeResolution::NotFound;
    let mut duplicate_walk = complete.clone();
    duplicate_walk.matches_in_walk = 2;
    let mut incomplete = complete.clone();
    incomplete.search_complete = false;
    let mut omitted_match = complete;
    omitted_match.matches.clear();

    for result in [unresolved_scope, duplicate_walk, incomplete, omitted_match] {
        assert!(!complete_unique_pointer_result(&result), "{result:?}");
    }
}

#[test]
fn pointer_resolution_reports_partial_query_evidence_without_hit_testing_or_dispatch() {
    let bounds = AxRect {
        x: 10,
        y: 10,
        width: 20,
        height: 20,
    };
    let mut incomplete = actionable_button_tree("Save", bounds);
    incomplete.truncated = Some(Truncation {
        limit: TruncationLimit::Nodes,
        limit_value: 2,
        nodes_walked: 2,
    });
    let cases = [
        (
            duplicate_button_tree("Save", 2),
            SemanticTarget {
                target: selector("Save"),
                within: None,
            },
            SemanticActionFailureKind::AmbiguousTarget,
        ),
        (
            incomplete,
            SemanticTarget {
                target: selector("Save"),
                within: None,
            },
            SemanticActionFailureKind::IncompleteTree,
        ),
        (
            scoped_tree(),
            SemanticTarget {
                target: selector("Save account"),
                within: Some(selector("Account panel")),
            },
            SemanticActionFailureKind::AmbiguousScope,
        ),
    ];

    for (tree, target, expected_kind) in cases {
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
        let (mut glass, walks, hit_calls, _) =
            pointer_glass(platform, vec![tree], PointerHit::Target, false);
        glass.start(&spec()).unwrap();

        let error = glass
            .click_target_inner(
                pointer_params(ActionTarget::Semantic(target), Some(0)),
                Deadline::UNBOUNDED,
            )
            .expect_err("partial query evidence cannot choose a pointer target");

        assert_eq!(error.kind, expected_kind);
        assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
        assert_eq!(walks.load(Ordering::Relaxed), 1);
        assert_eq!(hit_calls.load(Ordering::Relaxed), 0);
        assert!(clicks.lock().unwrap().is_empty());
    }
}

#[test]
fn planned_pointer_input_requires_every_coordinate_to_be_strictly_inside_the_window() {
    for point in [(0, 0), (99, 99)] {
        assert!(PlannedPointerInput::Click { point }.is_inside_window((100, 100)));
    }
    for point in [(-1, 0), (0, -1), (100, 0), (0, 100)] {
        assert!(!PlannedPointerInput::Click { point }.is_inside_window((100, 100)));
    }

    let segment = crate::Segment {
        from_x: 1,
        from_y: 2,
        to_x: 98,
        to_y: 97,
    };
    assert!(
        PlannedPointerInput::TrailingToggle {
            segment,
            probe_point: (50, 50),
        }
        .is_inside_window((100, 100))
    );
    for plan in [
        PlannedPointerInput::TrailingToggle {
            segment: crate::Segment {
                from_x: 100,
                ..segment
            },
            probe_point: (50, 50),
        },
        PlannedPointerInput::TrailingToggle {
            segment: crate::Segment {
                to_y: 100,
                ..segment
            },
            probe_point: (50, 50),
        },
        PlannedPointerInput::TrailingToggle {
            segment,
            probe_point: (-1, 50),
        },
    ] {
        assert!(!plan.is_inside_window((100, 100)), "{plan:?}");
    }
}

#[test]
fn dispatch_and_retry_provenance_distinguish_pre_dispatch_from_uncertain_effects() {
    let may_have = GlassError::Backend("focus transport failed".into());
    assert_eq!(
        focus_dispatch(&may_have, true),
        DispatchStatus::MayHaveDispatched
    );
    assert_eq!(
        focus_dispatch(&may_have, false),
        DispatchStatus::NotDispatched
    );
    assert_eq!(
        focus_dispatch(&GlassError::AxUnsupported, true),
        DispatchStatus::NotDispatched
    );
    assert_eq!(
        focus_dispatch(&GlassError::NoAxSnapshot, true),
        DispatchStatus::NotDispatched
    );
    assert_eq!(
        focus_dispatch(
            &GlassError::Backend("pre-dispatch".into()).before_dispatch(),
            true
        ),
        DispatchStatus::NotDispatched
    );

    let uncertain = action_source_error(
        GlassError::Backend("click transport failed".into()),
        None,
        None,
        ActionabilityReport::default(),
        unbounded_action_deadline(None),
        true,
    );
    assert_eq!(uncertain.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(uncertain.retry, RetryGuidance::DoNotRetry);
    let pre_dispatch = action_source_error(
        GlassError::NoAxSnapshot,
        None,
        None,
        ActionabilityReport::default(),
        unbounded_action_deadline(None),
        true,
    );
    assert_eq!(pre_dispatch.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(pre_dispatch.retry, RetryGuidance::SafeToRetry);
}

#[test]
fn set_value_error_provenance_requires_proof_before_reporting_safe_non_dispatch() {
    let cases = [
        (
            GlassError::NoAxSnapshot,
            DispatchStatus::NotDispatched,
            RetryGuidance::Reobserve,
        ),
        (
            GlassError::AxUnsupported,
            DispatchStatus::NotDispatched,
            RetryGuidance::CorrectRequest,
        ),
        (
            GlassError::Backend("refused before write".into()).before_dispatch(),
            DispatchStatus::NotDispatched,
            RetryGuidance::SafeToRetry,
        ),
        (
            GlassError::Backend("unknown transport outcome".into()),
            DispatchStatus::MayHaveDispatched,
            RetryGuidance::DoNotRetry,
        ),
        (
            GlassError::AxElementGone(7).after_dispatch(),
            DispatchStatus::MayHaveDispatched,
            RetryGuidance::DoNotRetry,
        ),
        (
            GlassError::AxWriteUnconfirmed(7, "read-back failed".into()),
            DispatchStatus::MayHaveDispatched,
            RetryGuidance::DoNotRetry,
        ),
    ];

    for (source, dispatch, retry) in cases {
        let error = set_value_source_error(
            source,
            None,
            None,
            ActionabilityReport::default(),
            unbounded_action_deadline(None),
        );
        assert_eq!(error.action_dispatch, dispatch);
        assert_eq!(error.retry, retry);
        assert_eq!(error.summary, "semantic set-value action failed");
    }
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
    let timeout = std::time::Duration::from_millis(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS);
    let (mut glass, coverage_started, subscription_deadlines, snapshot_deadlines) =
        deadline_recording_glass(std::time::Duration::from_millis(10));
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

    let coverage_started = coverage_started.lock().unwrap().unwrap();
    assert!(
        resolved.bound.deadline.instant().unwrap() <= coverage_started + timeout,
        "the action deadline must start before pre-poll setup"
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

    let exact_maximum = target_deadline(
        &ActionTarget::Semantic(semantic_target("save")),
        Some(SEMANTIC_ACTION_MAX_TIMEOUT_MS),
        None,
        Deadline::UNBOUNDED,
    )
    .expect("the documented maximum remains a valid action timeout");
    assert_eq!(exact_maximum.owner, Some(Whose::Callee));
    assert!(exact_maximum.allow_wait);
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

    let before_default = std::time::Instant::now();
    let defaulted = target_deadline(
        &ActionTarget::Semantic(semantic_target("save")),
        None,
        None,
        Deadline::UNBOUNDED,
    )
    .unwrap();
    let after_default = std::time::Instant::now();
    let duration = std::time::Duration::from_millis(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS);
    assert!(defaulted.deadline.instant().unwrap() >= before_default + duration);
    assert!(defaulted.deadline.instant().unwrap() <= after_default + duration);
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 3);
    assert_eq!(clicks.lock().unwrap().len(), 1);
}

#[test]
fn pointer_plan_change_resets_stability_when_identity_and_bounds_are_equal() {
    let bounds = AxRect {
        x: 10,
        y: 10,
        width: 81,
        height: 15,
    };
    let first = actionable_button_tree("Save", bounds);
    let mut final_tree = first.clone();
    final_tree.root.children[0].states.checkable = true;
    let drags = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(120, 80)
        .with_drag_log(drags.clone())
        .with_trailing_toggle_backend();
    let (mut glass, walks, _, _) = pointer_glass(
        platform,
        vec![first, final_tree.clone(), final_tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 3);
    assert_eq!(drags.lock().unwrap().len(), 1);
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
        pointer_glass(platform, vec![tree.clone()], PointerHit::Target, false);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_inner(
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), Some(0)),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::UnstableTarget);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    let target = error
        .target
        .as_ref()
        .expect("unstable known target retained");
    assert_eq!(target.id, AxNodeId(1));
    assert_eq!(target.name.as_deref(), Some("Save"));
    assert_eq!(target.bounds, tree.root.children[0].bounds);
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        error.action_method,
        Some(ActionMethod::Pointer {
            native_fallback: None
        })
    );
    assert!(!error.side_effects_may_have_occurred());
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Wi-Fi")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Wi-Fi")), None),
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
            pointer_params(ActionTarget::Semantic(semantic_target("Globex")), None),
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
fn legacy_id_forced_pointer_dispatches_without_a_fresh_read_or_stability_poll() {
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

    let outcome = glass
        .click_target_inner(
            pointer_params(ActionTarget::Id(AxNodeId(1)), None),
            Deadline::UNBOUNDED,
        )
        .unwrap();

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
            pointer_params(ActionTarget::Semantic(semantic_target("Save")), None),
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
    assert_eq!(
        error.action_method,
        Some(ActionMethod::NativeAction { actuated: None })
    );
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

fn click_mode_glass(trees: Vec<AxTree>, behavior: InvokeBehavior) -> ClickModeFixture {
    let native_calls = Arc::new(AtomicUsize::new(0));
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let accessibility = SeqAccessibility::new(trees)
        .with_coverage(full_state_coverage())
        .with_invoke_behavior(behavior)
        .with_invoke_calls(native_calls.clone())
        .with_deadlines(deadlines.clone())
        .with_hit(PointerHit::Target);
    let platform = FakePlatform::new(100, 100).with_click_log(clicks.clone());
    (
        glass_with_backend(platform, Box::new(accessibility)),
        native_calls,
        clicks,
        deadlines,
    )
}

fn semantic_click_params(mode: ActionMode, timeout_ms: u64) -> ClickTargetParams {
    ClickTargetParams {
        target: ActionTarget::Semantic(semantic_target("Save")),
        mode,
        timeout_ms: Some(timeout_ms),
        max_nodes: None,
    }
}

#[derive(Debug)]
struct GeneratedTreeCase {
    tree: AxTree,
    match_count: usize,
    complete: bool,
    summary: String,
}

struct FixedLcg(u64);

impl FixedLcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, upper: usize) -> usize {
        (self.next() % upper as u64) as usize
    }
}

fn generated_trees(count: usize) -> Vec<GeneratedTreeCase> {
    let mut rng = FixedLcg(0x676c_6173_735f_7077);
    (0..count)
        .map(|case_index| {
            let match_count = rng.below(7);
            let sibling_count = rng.below(13);
            let mut children = Vec::with_capacity(match_count + sibling_count);
            for index in 0..match_count {
                children.push(AxNode {
                    id: AxNodeId(0),
                    role: AxRole::Button,
                    raw_role: "button".into(),
                    name: Some("Generated target".into()),
                    description: None,
                    value: None,
                    states: AxStates {
                        enabled: true,
                        visible: true,
                        focusable: true,
                        ..AxStates::default()
                    },
                    bounds: Some(AxRect {
                        x: 10,
                        y: 10 + index as i32 * 20,
                        width: 80,
                        height: 18,
                    }),
                    children: Vec::new(),
                });
            }
            for index in 0..sibling_count {
                children.push(AxNode {
                    id: AxNodeId(0),
                    role: if rng.below(2) == 0 {
                        AxRole::Button
                    } else {
                        AxRole::Label
                    },
                    raw_role: "generated sibling".into(),
                    name: Some(format!("Unrelated sibling {case_index}-{index}")),
                    description: None,
                    value: None,
                    states: AxStates {
                        enabled: true,
                        visible: true,
                        ..AxStates::default()
                    },
                    bounds: Some(AxRect {
                        x: 110,
                        y: 10 + index as i32 * 12,
                        width: 80,
                        height: 10,
                    }),
                    children: Vec::new(),
                });
            }
            let mut tree = AxTree::new(AxNode {
                id: AxNodeId(0),
                role: AxRole::Window,
                raw_role: "window".into(),
                name: Some("Generated app".into()),
                description: None,
                value: None,
                states: AxStates::default(),
                bounds: Some(AxRect {
                    x: 0,
                    y: 0,
                    width: 320,
                    height: 240,
                }),
                children,
            });
            if rng.below(4) == 0 {
                tree.truncated = Some(Truncation {
                    limit: match rng.below(3) {
                        0 => TruncationLimit::Nodes,
                        1 => TruncationLimit::Depth,
                        _ => TruncationLimit::Siblings,
                    },
                    limit_value: 32,
                    nodes_walked: tree.root.children.len() + 1,
                });
            }
            tree.unexposed = rng.below(4);
            let complete = tree.can_prove_absence();
            let summary = format!(
                "index={case_index} matches={match_count} siblings={sibling_count} \
                 truncated={:?} withheld={} complete={complete}",
                tree.truncated.map(|value| value.limit),
                tree.unexposed,
            );
            GeneratedTreeCase {
                tree,
                match_count,
                complete,
                summary,
            }
        })
        .collect()
}

fn run_generated_click(
    tree: AxTree,
    dispatches: Arc<AtomicUsize>,
) -> SemanticActionResult<SemanticActionOutcome> {
    let accessibility = SeqAccessibility::new(vec![tree])
        .with_coverage(full_state_coverage())
        .with_invoke_behavior(InvokeBehavior::Succeed)
        .with_invoke_calls(dispatches);
    let mut glass = glass_with_backend(FakePlatform::new(320, 240), Box::new(accessibility));
    glass.start(&spec()).unwrap();
    glass.click_target(&ClickTargetParams {
        target: ActionTarget::Semantic(SemanticTarget {
            target: SemanticSelector::new(
                Some("Generated target".into()),
                Some(AxRole::Button),
                vec![SemanticState::Enabled, SemanticState::Visible],
            )
            .unwrap(),
            within: None,
        }),
        mode: ActionMode::Native,
        timeout_ms: Some(0),
        max_nodes: None,
    })
}

#[test]
fn generated_selector_actions_dispatch_only_for_one_match_in_a_complete_tree() {
    let mut seen = [[false; 2]; 7];
    for case in generated_trees(512) {
        seen[case.match_count][usize::from(case.complete)] = true;
        let expected = case.match_count == 1 && case.complete;
        let dispatches = Arc::new(AtomicUsize::new(0));
        let result = run_generated_click(case.tree, Arc::clone(&dispatches));
        assert_eq!(result.is_ok(), expected, "case={:?}", case.summary);
        assert_eq!(dispatches.load(Ordering::SeqCst), usize::from(expected));
        if let Err(error) = result {
            assert_eq!(
                error.action_dispatch,
                DispatchStatus::NotDispatched,
                "case={:?}",
                case.summary
            );
        }
    }
    for (match_count, completeness) in seen.into_iter().enumerate() {
        assert_eq!(
            completeness,
            [true, true],
            "generator did not cover complete and incomplete trees for match_count={match_count}"
        );
    }
}

#[test]
fn semantic_auto_click_uses_native_action_first() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, native_calls, clicks, _) =
        click_mode_glass(vec![tree], InvokeBehavior::Succeed);
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Auto, 500),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert!(clicks.lock().unwrap().is_empty());
    assert!(matches!(
        outcome.action.method,
        ActionMethod::NativeAction { actuated: None }
    ));
    assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::DispatchConfirmed
    );
}

#[test]
fn semantic_auto_click_falls_back_only_after_proven_pre_dispatch_unavailable() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, native_calls, clicks, deadlines) =
        click_mode_glass(vec![tree.clone(), tree], InvokeBehavior::NoAction);
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Auto, 500),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert_eq!(clicks.lock().unwrap().len(), 1);
    assert!(matches!(
        outcome.action.method,
        ActionMethod::Pointer {
            native_fallback: Some(ref reason)
        } if reason == "element exposes no activation action"
    ));
    assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
    assert!(
        deadlines
            .lock()
            .unwrap()
            .iter()
            .all(|deadline| *deadline == outcome.bound.deadline),
        "auto fallback must keep the original action deadline"
    );
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::DispatchConfirmed
    );
}

#[test]
fn semantic_auto_click_never_falls_back_after_possible_native_dispatch() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, native_calls, clicks, _) =
        click_mode_glass(vec![tree], InvokeBehavior::MayHaveDispatched);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Auto, 500),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert_eq!(native_calls.load(Ordering::Relaxed).saturating_sub(1), 0);
    assert!(clicks.lock().unwrap().is_empty());
    assert_eq!(
        error.action_method,
        Some(ActionMethod::NativeAction { actuated: None })
    );
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(error.retry, RetryGuidance::DoNotRetry);
}

#[test]
fn semantic_native_mode_never_dispatches_pointer() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, native_calls, clicks, _) =
        click_mode_glass(vec![tree], InvokeBehavior::Unsupported);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Native, 500),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert!(clicks.lock().unwrap().is_empty());
    assert_eq!(
        error.action_method,
        Some(ActionMethod::NativeAction { actuated: None })
    );
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
}

#[test]
fn semantic_pointer_mode_never_attempts_native_invoke() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, native_calls, clicks, _) =
        click_mode_glass(vec![tree.clone(), tree], InvokeBehavior::Succeed);
    let sink = RecordingSink::default();
    glass.set_audit_sink(Box::new(sink.clone()));
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Pointer, 500),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(native_calls.load(Ordering::Relaxed), 0);
    assert_eq!(clicks.lock().unwrap().len(), 1);
    assert_eq!(
        outcome.action.method,
        ActionMethod::Pointer {
            native_fallback: None
        }
    );
    let audits = sink.1.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].mode, "pointer");
    assert_eq!(audits[0].method.as_deref(), Some("pointer"));
    assert_eq!(audits[0].native_fallback, None);
    assert_eq!(audits[0].dispatch, "dispatched");
    assert_eq!(audits[0].confirmation, "dispatch_confirmed");
}

#[test]
fn semantic_native_click_can_actuate_a_known_hidden_target_and_discloses_visibility_failure_as_optional()
 {
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
    let (mut glass, native_calls, clicks, _) =
        click_mode_glass(vec![tree], InvokeBehavior::Succeed);
    glass.start(&spec()).unwrap();

    let outcome = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Native, 500),
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert!(clicks.lock().unwrap().is_empty());
    let visible = outcome
        .actionability
        .checks
        .iter()
        .find(|check| check.name == ActionabilityCheckName::Visible)
        .unwrap();
    assert_eq!(visible.verdict, ActionabilityVerdict::Failed);
    assert!(!visible.required);
}

#[test]
fn semantic_pointer_click_refuses_the_same_known_hidden_target() {
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
    let (mut glass, native_calls, clicks, _) =
        click_mode_glass(vec![tree], InvokeBehavior::Succeed);
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target_by(
            &semantic_click_params(ActionMode::Pointer, 0),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

    assert_eq!(native_calls.load(Ordering::Relaxed), 0);
    assert!(clicks.lock().unwrap().is_empty());
    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(
        error.actionability.blocking().unwrap().name,
        ActionabilityCheckName::Visible
    );
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
}

#[test]
fn duplicate_semantic_click_dispatches_neither_native_nor_pointer() {
    for mode in [ActionMode::Auto, ActionMode::Native, ActionMode::Pointer] {
        let tree = duplicate_button_tree("Save", 2);
        let (mut glass, native_calls, clicks, _) =
            click_mode_glass(vec![tree], InvokeBehavior::Succeed);
        glass.start(&spec()).unwrap();

        let error = glass
            .click_target_by(&semantic_click_params(mode, 0), Deadline::UNBOUNDED)
            .unwrap_err();

        assert_eq!(native_calls.load(Ordering::Relaxed), 0);
        assert!(clicks.lock().unwrap().is_empty());
        assert_eq!(error.kind, SemanticActionFailureKind::AmbiguousTarget);
        assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
        assert_eq!(error.action_method, None);
        assert!(!error.side_effects_may_have_occurred());
    }
}

#[test]
fn legacy_click_element_fields_and_native_first_behavior_are_unchanged() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let id = tree.root.children[0].id;
    let (mut legacy, legacy_native_calls, legacy_clicks, _) =
        click_mode_glass(vec![tree.clone()], InvokeBehavior::SucceedOnAnother(7));
    let legacy_sink = RecordingSink::default();
    legacy.set_audit_sink(Box::new(legacy_sink.clone()));
    legacy.start(&spec()).unwrap();
    legacy.a11y_snapshot(None).unwrap();

    let legacy_method = legacy.click_element(id).unwrap();

    assert_eq!(legacy_method.label(), "native-action");
    assert_eq!(legacy_method.native_fallback(), None);
    assert_eq!(legacy_method.actuated(), Some(AxNodeId(7)));
    assert_eq!(legacy_native_calls.load(Ordering::Relaxed), 1);
    assert!(legacy_clicks.lock().unwrap().is_empty());
    let legacy_audits = legacy_sink.1.lock().unwrap();
    assert_eq!(legacy_audits.len(), 1);
    assert_eq!(legacy_audits[0].mode, "auto");
    assert_eq!(legacy_audits[0].method.as_deref(), Some("native-action"));
    assert_eq!(legacy_audits[0].actuated_id, Some(7));
    drop(legacy_audits);

    let (mut semantic, semantic_native_calls, semantic_clicks, _) =
        click_mode_glass(vec![tree], InvokeBehavior::NoAction);
    let semantic_sink = RecordingSink::default();
    semantic.set_audit_sink(Box::new(semantic_sink.clone()));
    semantic.start(&spec()).unwrap();
    semantic.a11y_snapshot(None).unwrap();
    let outcome = semantic
        .click_target_by(
            &ClickTargetParams {
                target: ActionTarget::Id(id),
                mode: ActionMode::Auto,
                timeout_ms: None,
                max_nodes: None,
            },
            Deadline::UNBOUNDED,
        )
        .unwrap();
    let legacy_pointer = semantic.click_element(id).unwrap();

    assert!(matches!(
        outcome.action.method,
        ActionMethod::Pointer {
            native_fallback: Some(_)
        }
    ));
    assert!(!outcome.actionability.checks.is_empty());
    assert!(
        outcome
            .actionability
            .checks
            .iter()
            .all(|check| !check.required && check.source == ActionabilitySource::LegacyCache)
    );
    assert_eq!(
        report_verdict(&outcome, ActionabilityCheckName::BackendFingerprint),
        ActionabilityVerdict::Unproven,
        "a legacy pointer fallback cannot claim native fingerprint validation"
    );
    let native_report = legacy.legacy_click_actionability(id, false);
    assert!(!native_report.checks.is_empty());
    assert!(
        native_report
            .checks
            .iter()
            .all(|check| !check.required && check.source == ActionabilitySource::LegacyCache)
    );
    let native_outcome = legacy.legacy_click_outcome(
        id,
        ActionMethod::NativeAction { actuated: None },
        false,
        unbounded_action_deadline(None),
    );
    assert_eq!(
        report_verdict(&native_outcome, ActionabilityCheckName::BackendFingerprint),
        ActionabilityVerdict::Passed
    );
    assert_eq!(legacy_pointer.label(), "pointer");
    assert_eq!(
        legacy_pointer.native_fallback(),
        Some("element exposes no activation action")
    );
    assert_eq!(legacy_pointer.actuated(), None);
    assert_eq!(semantic_native_calls.load(Ordering::Relaxed), 2);
    assert_eq!(semantic_clicks.lock().unwrap().len(), 2);
    let semantic_audits = semantic_sink.1.lock().unwrap();
    assert_eq!(semantic_audits.len(), 2);
    assert!(semantic_audits.iter().all(|audit| {
        audit.mode == "auto"
            && audit.method.as_deref() == Some("pointer")
            && audit.native_fallback.as_deref() == Some("element exposes no activation action")
            && audit.actuated_id.is_none()
    }));
}

#[test]
fn semantic_public_click_audit_emits_exactly_one_record_on_native_fallback_and_failure() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let cases = [
        (
            vec![tree.clone()],
            InvokeBehavior::Succeed,
            500,
            true,
            Some("native-action"),
            None,
            "dispatched",
            "dispatch_confirmed",
        ),
        (
            vec![tree.clone(), tree.clone()],
            InvokeBehavior::Unsupported,
            500,
            true,
            Some("pointer"),
            Some("backend has no native action path"),
            "dispatched",
            "dispatch_confirmed",
        ),
        (
            vec![duplicate_button_tree("Save", 2)],
            InvokeBehavior::Succeed,
            0,
            false,
            None,
            None,
            "not_dispatched",
            "unconfirmed",
        ),
    ];

    for (
        trees,
        behavior,
        timeout_ms,
        succeeds,
        expected_method,
        expected_fallback,
        expected_dispatch,
        expected_confirmation,
    ) in cases
    {
        let (mut glass, _, _, _) = click_mode_glass(trees, behavior);
        let sink = RecordingSink::default();
        glass.set_audit_sink(Box::new(sink.clone()));
        glass.start(&spec()).unwrap();

        let result = glass.click_target_by(
            &semantic_click_params(ActionMode::Auto, timeout_ms),
            Deadline::UNBOUNDED,
        );

        assert_eq!(result.is_ok(), succeeds);
        let records = sink.0.lock().unwrap();
        let click_records = records
            .iter()
            .filter(|record| record.starts_with("click_element:"))
            .collect::<Vec<_>>();
        assert_eq!(click_records.len(), 1, "records: {records:?}");
        assert_eq!(
            click_records[0].as_str(),
            if succeeds {
                "click_element:true"
            } else {
                "click_element:false"
            }
        );
        drop(records);
        let audits = sink.1.lock().unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].element.name.as_deref(), Some("Save"));
        assert_eq!(audits[0].mode, "auto");
        assert_eq!(audits[0].method.as_deref(), expected_method);
        assert_eq!(audits[0].native_fallback.as_deref(), expected_fallback);
        assert_eq!(audits[0].dispatch, expected_dispatch);
        assert_eq!(audits[0].confirmation, expected_confirmation);
        assert_eq!(audits[0].ok, succeeds);
    }
}

#[test]
fn auto_fallback_resolution_failure_retains_pointer_selection_without_dispatch() {
    let tree = actionable_button_tree(
        "Save",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let (mut glass, native_calls, clicks, _) = click_mode_glass(
        vec![tree, duplicate_button_tree("Save", 2)],
        InvokeBehavior::Unsupported,
    );
    let sink = RecordingSink::default();
    glass.set_audit_sink(Box::new(sink.clone()));
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target(&semantic_click_params(ActionMode::Auto, 0))
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::AmbiguousTarget);
    assert_eq!(
        error.action_method,
        Some(ActionMethod::Pointer {
            native_fallback: Some("backend has no native action path".into()),
        })
    );
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert!(!error.side_effects_may_have_occurred());
    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert!(clicks.lock().unwrap().is_empty());
    let audits = sink.1.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].method.as_deref(), Some("pointer"));
    assert_eq!(
        audits[0].native_fallback.as_deref(),
        Some("backend has no native action path")
    );
    assert_eq!(audits[0].dispatch, "not_dispatched");
    assert_eq!(audits[0].confirmation, "unconfirmed");
    assert_eq!(audits[0].actuated_id, None);
}

#[test]
fn failure_audit_auto_pointer_fallback_retains_the_resolved_target_and_selected_path() {
    let tree = actionable_button_tree(
        "Save account",
        AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        },
    );
    let native_calls = Arc::new(AtomicUsize::new(0));
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let accessibility = SeqAccessibility::new(vec![tree.clone(), tree.clone(), tree])
        .with_coverage(full_state_coverage())
        .with_invoke_behavior(InvokeBehavior::Unsupported)
        .with_invoke_calls(native_calls.clone())
        .with_hit(PointerHit::Target);
    let platform = FakePlatform::new(100, 100)
        .with_click_log(clicks.clone())
        .with_failing_pointer();
    let mut glass = glass_with_backend(platform, Box::new(accessibility));
    let sink = RecordingSink::default();
    glass.set_audit_sink(Box::new(sink.clone()));
    glass.start(&spec()).unwrap();

    let error = glass
        .click_target(&semantic_click_params(ActionMode::Auto, 500))
        .unwrap_err();

    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert_eq!(clicks.lock().unwrap().len(), 1);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(
        error.action_method,
        Some(ActionMethod::Pointer {
            native_fallback: Some("backend has no native action path".into()),
        })
    );
    assert!(error.side_effects_may_have_occurred());
    assert_eq!(
        error.target.as_ref().map(|target| target.id),
        Some(AxNodeId(1))
    );
    let audits = sink.1.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(
        (
            audits[0].element.id,
            audits[0].method.as_deref(),
            audits[0].native_fallback.as_deref(),
        ),
        (
            1,
            Some("pointer"),
            Some("backend has no native action path"),
        )
    );
    assert_eq!(audits[0].element.role.as_deref(), Some("Button"));
    assert_eq!(audits[0].element.name.as_deref(), Some("Save account"));
    assert_eq!(audits[0].actuated_id, None);
    assert_eq!(audits[0].dispatch, "may_have_dispatched");
    assert_eq!(audits[0].confirmation, "unconfirmed");
    assert!(!audits[0].ok);
}

#[test]
fn semantic_set_value_resolves_fresh_and_calls_the_backend_once() {
    let cached = editable_field_tree("Former account name", Some("old"), true);
    let fresh = editable_field_tree("Account name", Some("old"), true);
    let (mut glass, walks, set_log, deadlines) =
        semantic_set_value_glass(vec![cached, fresh], Vec::new());
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(None).unwrap();

    let outcome = glass
        .set_value_target_by(
            &semantic_set_value_params("Account name", 500),
            "updated",
            Deadline::UNBOUNDED,
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 2);
    assert_eq!(
        set_log.lock().unwrap().as_slice(),
        &[(outcome.target.id, "updated".into())]
    );
    assert_eq!(outcome.target.name.as_deref(), Some("Account name"));
    assert_eq!(outcome.action.method, ActionMethod::AccessibilityValue);
    assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::ValueConfirmed
    );
    let deadlines = deadlines.lock().unwrap();
    assert_eq!(deadlines.len(), 3);
    assert!(
        deadlines[1..]
            .iter()
            .all(|deadline| *deadline == outcome.bound.deadline)
    );
}

#[test]
fn semantic_set_value_waits_for_a_known_disabled_field_to_become_enabled() {
    let disabled = editable_field_tree("Account name", Some("old"), false);
    let enabled = editable_field_tree("Account name", Some("old"), true);
    let (mut glass, walks, set_log, _) =
        semantic_set_value_glass(vec![disabled, enabled], Vec::new());
    glass.start(&spec()).unwrap();

    let outcome = glass
        .set_value_target(&semantic_set_value_params("Account name", 500), "updated")
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 2);
    assert_eq!(set_log.lock().unwrap().len(), 1);
    assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::ValueConfirmed
    );
}

#[test]
fn semantic_set_value_refuses_a_known_non_value_role_before_backend_dispatch() {
    let tree = value_control_tree(
        "Save",
        AxRole::Button,
        None,
        AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            ..AxStates::default()
        },
    );
    let (mut glass, walks, set_log, _) = semantic_set_value_glass(vec![tree], Vec::new());
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("Save", 0), "not-a-value")
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.retry, RetryGuidance::Reobserve);
    assert_eq!(
        error.actionability.blocking().unwrap().name,
        ActionabilityCheckName::FocusEligible
    );
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert!(set_log.lock().unwrap().is_empty());
}

#[test]
fn semantic_set_value_refuses_uncovered_editable_bit_on_unsupported_role() {
    let tree = value_control_tree(
        "Save",
        AxRole::Button,
        None,
        AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            editable: true,
            ..AxStates::default()
        },
    );
    let mut coverage = full_state_coverage();
    coverage.editable = false;
    let (mut glass, walks, set_log, _) =
        semantic_set_value_glass_with_coverage(vec![tree], Vec::new(), coverage);
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("Save", 0), "not-a-value")
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.retry, RetryGuidance::Reobserve);
    let eligibility = error
        .actionability
        .checks
        .iter()
        .find(|check| check.name == ActionabilityCheckName::FocusEligible)
        .unwrap();
    assert_eq!(eligibility.verdict, ActionabilityVerdict::Unproven);
    assert!(eligibility.required);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert!(set_log.lock().unwrap().is_empty());
}

#[test]
fn semantic_set_value_refuses_uncovered_checkable_bit_on_unsupported_role() {
    let tree = value_control_tree(
        "Save",
        AxRole::Button,
        None,
        AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            checkable: true,
            ..AxStates::default()
        },
    );
    let (mut glass, walks, set_log, _) =
        semantic_set_value_glass_with_coverage(vec![tree], Vec::new(), AxStateCoverage::NONE);
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("Save", 0), "not-a-value")
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.retry, RetryGuidance::Reobserve);
    let eligibility = error
        .actionability
        .checks
        .iter()
        .find(|check| check.name == ActionabilityCheckName::FocusEligible)
        .unwrap();
    assert_eq!(eligibility.verdict, ActionabilityVerdict::Unproven);
    assert!(eligibility.required);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert!(set_log.lock().unwrap().is_empty());
}

#[test]
fn semantic_set_value_refuses_uncovered_editable_support_on_text_field() {
    let tree = editable_field_tree("Account name", Some("old"), true);
    let mut coverage = full_state_coverage();
    coverage.editable = false;
    let (mut glass, walks, set_log, _) =
        semantic_set_value_glass_with_coverage(vec![tree], Vec::new(), coverage);
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("Account name", 0), "updated")
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.retry, RetryGuidance::Reobserve);
    let eligibility = error
        .actionability
        .checks
        .iter()
        .find(|check| check.name == ActionabilityCheckName::FocusEligible)
        .unwrap();
    assert_eq!(eligibility.verdict, ActionabilityVerdict::Unproven);
    assert!(eligibility.required);
    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert!(set_log.lock().unwrap().is_empty());
}

#[test]
fn semantic_set_value_preserves_backend_value_confirmation() {
    let tree = editable_field_tree("Account name", Some("old"), true);
    let (mut glass, walks, set_log, _) = semantic_set_value_glass(vec![tree], Vec::new());
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(None).unwrap();
    let id = glass
        .active
        .as_ref()
        .unwrap()
        .last_ax
        .as_ref()
        .unwrap()
        .root
        .children[0]
        .id;

    let outcome = glass
        .set_value_target(
            &SetValueTargetParams {
                target: ActionTarget::Id(id),
                timeout_ms: None,
                max_nodes: None,
            },
            "updated",
        )
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(
        set_log.lock().unwrap().as_slice(),
        &[(id, "updated".into())]
    );
    assert_eq!(outcome.action.method, ActionMethod::AccessibilityValue);
    assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::ValueConfirmed
    );
}

#[test]
fn unconfirmed_semantic_write_is_terminal_and_clears_the_cached_value() {
    let secret = "requested secret text";
    let tree = editable_field_tree("Account name", Some("old"), true);
    let (mut glass, _, set_log, _) = semantic_set_value_glass(
        vec![tree],
        vec![Err(GlassError::AxWriteUnconfirmed(1, secret.into()))],
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("Account name", 0), secret)
        .unwrap_err();

    let calls = set_log.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, secret);
    let id = calls[0].0;
    drop(calls);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(error.retry, RetryGuidance::DoNotRetry);
    assert!(!error.to_string().contains(secret));
    assert!(!error.summary.contains(secret));
    assert!(!format!("{:?}", error.candidates).contains(secret));
    assert!(!format!("{:?}", error.actionability).contains(secret));
    assert_eq!(
        glass
            .active
            .as_ref()
            .and_then(|active| active.last_ax.as_ref())
            .and_then(|tree| tree.find(id))
            .and_then(|node| node.value.as_deref()),
        None
    );
}

#[test]
fn semantic_set_value_does_not_retry_after_a_may_have_dispatched_transport_failure() {
    let tree = editable_field_tree("Account name", Some("old"), true);
    let (mut glass, walks, set_log, _) = semantic_set_value_glass(
        vec![tree],
        vec![Err(GlassError::AccessibilityUnavailable(
            "scripted transport failure".into(),
        ))],
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("Account name", 500), "updated")
        .unwrap_err();

    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(set_log.lock().unwrap().len(), 1);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(error.action_method, Some(ActionMethod::AccessibilityValue));
    assert!(error.side_effects_may_have_occurred());
    assert_eq!(error.retry, RetryGuidance::DoNotRetry);
}

#[test]
fn legacy_set_value_uses_the_cached_id_without_a_fresh_read() {
    let cached = editable_field_tree("Cached account name", Some("old"), true);
    let fresh = editable_field_tree("Different field", Some("other"), true);
    let (mut glass, walks, set_log, _) = semantic_set_value_glass(vec![cached, fresh], Vec::new());
    glass.start(&spec()).unwrap();
    glass.a11y_snapshot(None).unwrap();
    let cached_id = glass
        .active
        .as_ref()
        .unwrap()
        .last_ax
        .as_ref()
        .unwrap()
        .root
        .children[0]
        .id;

    glass.set_value(cached_id, "updated").unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(
        set_log.lock().unwrap().as_slice(),
        &[(cached_id, "updated".into())]
    );
}

#[test]
fn semantic_set_value_already_applied_reports_a_truthful_no_op() {
    let tree = value_control_tree(
        "Beta",
        AxRole::ComboBox,
        Some("Beta"),
        AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            ..AxStates::default()
        },
    );
    let (mut glass, walks, set_log, _) = semantic_set_value_glass(vec![tree], Vec::new());
    glass.start(&spec()).unwrap();

    let outcome = glass
        .set_value_target(&semantic_set_value_params("Beta", 0), "Beta")
        .unwrap();

    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert!(set_log.lock().unwrap().is_empty());
    assert_eq!(outcome.action.method, ActionMethod::AccessibilityValue);
    assert_eq!(outcome.action.dispatch, DispatchStatus::NotDispatched);
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::ValueConfirmed
    );
}

#[test]
fn semantic_public_set_value_audit_emits_one_safe_high_level_record() {
    let secret = "audit secret text";
    let tree = editable_field_tree("Account name", Some("old"), true);
    let (mut glass, _, set_log, _) = semantic_set_value_glass(
        vec![tree],
        vec![Err(GlassError::AxWriteUnconfirmed(1, secret.into()))],
    );
    let sink = RecordingSink::default();
    glass.set_audit_sink(Box::new(sink.clone()));
    glass.start(&spec()).unwrap();

    let result = glass.set_value_target(&semantic_set_value_params("Account name", 0), secret);

    assert!(result.is_err());
    assert_eq!(set_log.lock().unwrap().len(), 1);
    let records = sink.0.lock().unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.starts_with("set_value:"))
            .count(),
        1,
        "records: {records:?}"
    );
    drop(records);
    let audits = sink.2.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].element.name.as_deref(), Some("Account name"));
    assert_eq!(audits[0].text, secret);
    assert_eq!(audits[0].dispatch, "may_have_dispatched");
    assert_eq!(audits[0].confirmation, "unconfirmed");
    assert!(!audits[0].ok);
    assert!(!audits[0].error.as_deref().unwrap().contains(secret));
}

#[test]
fn failure_audit_set_value_retains_the_resolved_target_instead_of_the_selector_query() {
    let secret = "audit secret text";
    let tree = editable_field_tree("Primary account name", Some("old"), true);
    let (mut glass, walks, set_log, _) = semantic_set_value_glass(
        vec![tree],
        vec![Err(GlassError::AxWriteUnconfirmed(1, secret.into()))],
    );
    let sink = RecordingSink::default();
    glass.set_audit_sink(Box::new(sink.clone()));
    glass.start(&spec()).unwrap();

    let error = glass
        .set_value_target(&semantic_set_value_params("account", 0), secret)
        .unwrap_err();

    assert_eq!(walks.load(Ordering::Relaxed), 1);
    assert_eq!(set_log.lock().unwrap().len(), 1);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(
        error.target.as_ref().map(|target| target.id),
        Some(AxNodeId(1))
    );
    let audits = sink.2.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].element.id, 1);
    assert_eq!(audits[0].element.role.as_deref(), Some("TextField"));
    assert_eq!(
        audits[0].element.name.as_deref(),
        Some("Primary account name")
    );
    assert_eq!(audits[0].dispatch, "may_have_dispatched");
    assert_eq!(audits[0].confirmation, "unconfirmed");
    assert!(!audits[0].ok);
    assert!(!audits[0].error.as_deref().unwrap().contains(secret));
}

#[test]
fn targeted_type_zero_timeout_types_only_if_the_first_confirmation_read_is_focused() {
    for (focused, succeeds) in [(true, true), (false, false)] {
        let mut fixture = targeted_type_glass(
            vec![
                targeted_type_field_tree("Account name", false),
                targeted_type_field_tree("Account name", focused),
                targeted_type_field_tree("Account name", true),
            ],
            InvokeBehavior::Succeed,
            full_state_coverage(),
            false,
        );
        fixture.glass.start(&spec()).unwrap();

        let result = fixture.glass.type_target_by(
            &targeted_type_params("Account name", ActionMode::Native, 0),
            "typed once",
            Deadline::UNBOUNDED,
        );

        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(fixture.walks.load(Ordering::Relaxed), 2);
        assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.key_log.lock().unwrap().len(), usize::from(succeeds));
    }
}

#[test]
fn targeted_type_native_focuses_confirms_then_types_once() {
    let secret = "private account text";
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", true),
        ],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    let sink = RecordingSink::default();
    fixture.glass.set_audit_sink(Box::new(sink.clone()));
    fixture.glass.start(&spec()).unwrap();
    sink.0.lock().unwrap().clear();
    sink.3.lock().unwrap().clear();

    let outcome = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 500),
            secret,
        )
        .unwrap();

    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert_eq!(
        fixture.key_log.lock().unwrap().as_slice(),
        &[KeyEvent::Text(secret.into())]
    );
    assert_eq!(
        fixture.events.lock().unwrap().as_slice(),
        &["focus", "snapshot focused", "key"]
    );
    assert_eq!(
        outcome.focus,
        Some(MutationReport {
            method: ActionMethod::NativeAction { actuated: None },
            dispatch: DispatchStatus::Dispatched,
            confirmation: ConfirmationStatus::FocusConfirmed,
        })
    );
    assert_eq!(outcome.action.method, ActionMethod::Keyboard);
    assert_eq!(outcome.action.dispatch, DispatchStatus::Dispatched);
    assert_eq!(
        outcome.action.confirmation,
        ConfirmationStatus::DispatchConfirmed
    );
    assert_eq!(sink.0.lock().unwrap().as_slice(), &["type_target:true"]);
    let audits = sink.3.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].text, secret);
    assert_eq!(audits[0].focus_mode, "native");
    assert_eq!(audits[0].focus_method.as_deref(), Some("native_action"));
    assert_eq!(audits[0].focus_dispatch, "dispatched");
    assert_eq!(audits[0].focus_confirmation, "focus_confirmed");
    assert_eq!(audits[0].type_dispatch, "dispatched");
}

#[test]
fn targeted_type_auto_falls_back_to_pointer_only_after_pre_dispatch_focus_unsupported() {
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", true),
        ],
        InvokeBehavior::Unsupported,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let outcome = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Auto, 500),
            "typed once",
        )
        .unwrap();

    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.clicks.lock().unwrap().len(), 1);
    assert_eq!(fixture.hit_calls.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.key_log.lock().unwrap().len(), 1);
    assert!(matches!(
        outcome.focus,
        Some(MutationReport {
            method: ActionMethod::Pointer {
                native_fallback: Some(_)
            },
            dispatch: DispatchStatus::Dispatched,
            confirmation: ConfirmationStatus::FocusConfirmed,
        })
    ));
}

#[test]
fn targeted_type_does_not_try_pointer_after_native_focus_may_have_dispatched() {
    let mut fixture = targeted_type_glass(
        vec![targeted_type_field_tree("Account name", false)],
        InvokeBehavior::MayHaveDispatched,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Auto, 0),
            "must not type",
        )
        .unwrap_err();

    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert!(fixture.key_log.lock().unwrap().is_empty());
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.retry, RetryGuidance::DoNotRetry);
    assert_eq!(
        error.focus.as_ref().map(|focus| focus.dispatch),
        Some(DispatchStatus::MayHaveDispatched)
    );
}

#[test]
fn targeted_type_keeps_retry_safe_when_pointer_focus_proves_no_dispatch() {
    let tree = value_control_tree(
        "Account name",
        AxRole::TextField,
        Some("old"),
        AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            editable: true,
            ..AxStates::default()
        },
    );
    let mut tree = tree;
    tree.root.children[0].bounds = Some(AxRect {
        x: 70,
        y: 248,
        width: 80,
        height: 27,
    });
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
    let keys = Arc::new(Mutex::new(Vec::new()));
    let platform = FakePlatform::new(340, 300)
        .with_windows(vec![active, popover])
        .with_key_log(keys.clone());
    let (mut glass, _, _, _) = pointer_glass(
        platform,
        vec![tree.clone(), tree.clone(), tree],
        PointerHit::Target,
        false,
    );
    glass.start(&spec()).unwrap();

    let error = glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Auto, 600),
            "must not type",
        )
        .expect_err("an unmappable popover target cannot receive pointer focus");

    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.retry, RetryGuidance::SafeToRetry);
    assert_eq!(
        error.focus.as_ref().map(|focus| focus.dispatch),
        Some(DispatchStatus::NotDispatched)
    );
    assert!(keys.lock().unwrap().is_empty());
}

#[test]
fn focus_confirmation_without_state_coverage_reads_once_even_with_a_wait_budget() {
    let tree = targeted_type_field_tree("Account name", true);
    let target = ax_target(&ElementInfo::from_node(&tree.root.children[0]));
    let mut fixture = targeted_type_glass(
        vec![tree],
        InvokeBehavior::Succeed,
        AxStateCoverage {
            focused: false,
            ..full_state_coverage()
        },
        false,
    );
    fixture.glass.start(&spec()).unwrap();
    let bound = ActionDeadline {
        deadline: Deadline::from_millis(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS),
        owner: Some(Whose::Callee),
        allow_wait: true,
    };

    let error = fixture
        .glass
        .confirm_focused_target(&target, bound)
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::FocusUnconfirmed);
    assert_eq!(fixture.walks.load(Ordering::Relaxed), 1);
}

#[test]
fn focus_confirmation_with_state_coverage_waits_for_a_focused_read() {
    let tree = targeted_type_field_tree("Account name", false);
    let target = ax_target(&ElementInfo::from_node(&tree.root.children[0]));
    let mut fixture = targeted_type_glass(
        vec![tree, targeted_type_field_tree("Account name", true)],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();
    let bound = ActionDeadline {
        deadline: Deadline::from_millis(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS),
        owner: Some(Whose::Callee),
        allow_wait: true,
    };

    let confirmed = fixture
        .glass
        .confirm_focused_target(&target, bound)
        .unwrap();

    assert!(confirmed.states.focused);
    assert_eq!(fixture.walks.load(Ordering::Relaxed), 2);
}

#[test]
fn targeted_type_never_types_when_focus_cannot_be_confirmed() {
    let secret = "never expose this text";
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", true),
        ],
        InvokeBehavior::Succeed,
        AxStateCoverage {
            focused: false,
            ..full_state_coverage()
        },
        false,
    );
    let sink = RecordingSink::default();
    fixture.glass.set_audit_sink(Box::new(sink.clone()));
    fixture.glass.start(&spec()).unwrap();
    sink.0.lock().unwrap().clear();
    sink.3.lock().unwrap().clear();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 0),
            secret,
        )
        .unwrap_err();

    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
    assert!(fixture.key_log.lock().unwrap().is_empty());
    assert_eq!(error.kind, SemanticActionFailureKind::FocusUnconfirmed);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    assert_eq!(error.action_method, None);
    assert!(error.side_effects_may_have_occurred());
    assert_eq!(error.retry, RetryGuidance::Reobserve);
    assert_eq!(
        error.focus,
        Some(MutationReport {
            method: ActionMethod::NativeAction { actuated: None },
            dispatch: DispatchStatus::Dispatched,
            confirmation: ConfirmationStatus::Unconfirmed,
        })
    );
    let target = error
        .target
        .as_ref()
        .expect("focus-unconfirmed target retained");
    assert_eq!(target.id, AxNodeId(1));
    assert_eq!(target.name.as_deref(), Some("Account name"));
    assert_eq!(target.role, AxRole::TextField);
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{:?}", error.source).contains(secret));
    assert!(!format!("{:?}", error.candidates).contains(secret));
    assert!(!format!("{:?}", error.actionability).contains(secret));
    assert_eq!(sink.0.lock().unwrap().as_slice(), &["type_target:false"]);
    let audits = sink.3.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].text, secret);
    assert!(!audits[0].error.as_deref().unwrap().contains(secret));
}

#[test]
fn targeted_type_never_dispatches_a_second_text_batch_after_possible_key_delivery() {
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", true),
        ],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        true,
    );
    let sink = RecordingSink::default();
    fixture.glass.set_audit_sink(Box::new(sink.clone()));
    fixture.glass.start(&spec()).unwrap();
    sink.0.lock().unwrap().clear();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 500),
            "one batch",
        )
        .unwrap_err();

    assert_eq!(fixture.key_log.lock().unwrap().len(), 1);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(error.action_method, Some(ActionMethod::Keyboard));
    assert!(error.side_effects_may_have_occurred());
    assert_eq!(error.retry, RetryGuidance::DoNotRetry);
    assert_eq!(
        error.focus.as_ref().map(|focus| focus.confirmation),
        Some(ConfirmationStatus::FocusConfirmed)
    );
    assert_eq!(sink.0.lock().unwrap().as_slice(), &["type_target:false"]);
}

#[test]
fn failure_audit_post_focus_type_retains_the_resolved_target_and_dispatch_evidence() {
    let secret = "one private batch";
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Primary account name", false),
            targeted_type_field_tree("Primary account name", true),
        ],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        true,
    );
    let sink = RecordingSink::default();
    fixture.glass.set_audit_sink(Box::new(sink.clone()));
    fixture.glass.start(&spec()).unwrap();
    sink.0.lock().unwrap().clear();
    sink.3.lock().unwrap().clear();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("account", ActionMode::Native, 500),
            secret,
        )
        .unwrap_err();

    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert_eq!(fixture.key_log.lock().unwrap().len(), 1);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(
        error.target.as_ref().map(|target| target.id),
        Some(AxNodeId(1))
    );
    assert_eq!(sink.0.lock().unwrap().as_slice(), &["type_target:false"]);
    let audits = sink.3.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].element.id, 1);
    assert_eq!(audits[0].element.role.as_deref(), Some("TextField"));
    assert_eq!(
        audits[0].element.name.as_deref(),
        Some("Primary account name")
    );
    assert_eq!(audits[0].focus_method.as_deref(), Some("native_action"));
    assert_eq!(audits[0].focus_dispatch, "dispatched");
    assert_eq!(audits[0].focus_confirmation, "focus_confirmed");
    assert_eq!(audits[0].type_dispatch, "may_have_dispatched");
    assert!(!audits[0].ok);
    assert!(!audits[0].error.as_deref().unwrap().contains(secret));
}

#[test]
fn failure_audit_ambiguity_stays_unresolved_and_dispatches_nothing() {
    let mut tree = targeted_type_field_tree("Primary account name", false);
    let mut duplicate = tree.root.children[0].clone();
    duplicate.id = AxNodeId(2);
    duplicate.name = Some("Backup account name".into());
    tree.root.children.push(duplicate);
    let tree = AxTree::new(tree.root);
    let mut fixture = targeted_type_glass(
        vec![tree],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    let sink = RecordingSink::default();
    fixture.glass.set_audit_sink(Box::new(sink.clone()));
    fixture.glass.start(&spec()).unwrap();
    sink.0.lock().unwrap().clear();
    sink.3.lock().unwrap().clear();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("account", ActionMode::Native, 0),
            "must not type",
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::AmbiguousTarget);
    assert_eq!(error.candidates.len(), 2);
    assert!(error.target.is_none());
    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 0);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert!(fixture.key_log.lock().unwrap().is_empty());
    assert_eq!(sink.0.lock().unwrap().as_slice(), &["type_target:false"]);
    let audits = sink.3.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].element.id, 0);
    assert_eq!(audits[0].element.role, None);
    assert_eq!(audits[0].element.name.as_deref(), Some("account"));
    assert_eq!(audits[0].focus_method, None);
    assert_eq!(audits[0].focus_dispatch, "not_dispatched");
    assert_eq!(audits[0].type_dispatch, "not_dispatched");
    assert!(!audits[0].ok);
}

#[test]
fn targeted_type_reacquires_a_renumbered_focused_field_by_identity() {
    let initial = targeted_type_field_tree("Account name", false);
    let initial_id = initial.root.children[0].id;
    let focused = renumbered_targeted_type_field_tree("Account name");
    let focused_id = focused.root.children[1].id;
    assert_ne!(initial_id, focused_id);
    let mut fixture = targeted_type_glass(
        vec![initial, focused],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let outcome = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 500),
            "typed after renumber",
        )
        .unwrap();

    assert_eq!(outcome.target.id, focused_id);
    assert_eq!(fixture.key_log.lock().unwrap().len(), 1);
    assert!(
        fixture
            .glass
            .active
            .as_ref()
            .and_then(|active| active.last_ax.as_ref())
            .and_then(|tree| tree.find(focused_id))
            .is_some_and(|node| node.states.focused)
    );
}

#[test]
fn targeted_type_preserves_text_role_eligibility_when_editable_is_reported_false() {
    for role in [AxRole::TextField, AxRole::TextArea] {
        let trees = [false, true]
            .into_iter()
            .map(|focused| {
                let mut tree = targeted_type_field_tree("Account name", focused);
                let field = &mut tree.root.children[0];
                field.role = role;
                field.states.editable = false;
                field.states.focusable = false;
                tree
            })
            .collect();
        let mut fixture =
            targeted_type_glass(trees, InvokeBehavior::Succeed, full_state_coverage(), false);
        fixture.glass.start(&spec()).unwrap();

        let outcome = fixture
            .glass
            .type_target(
                &targeted_type_params("Account name", ActionMode::Native, 0),
                "typed once",
            )
            .unwrap();

        assert_eq!(outcome.target.role, role);
        assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            fixture.key_log.lock().unwrap().as_slice(),
            &[KeyEvent::Text("typed once".into())]
        );
    }
}

#[test]
fn targeted_type_rejects_a_button_even_when_the_backend_can_focus_it() {
    let tree = value_control_tree(
        "Save",
        AxRole::Button,
        None,
        AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            ..AxStates::default()
        },
    );
    let mut fixture = targeted_type_glass(
        vec![tree],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Save", ActionMode::Native, 0),
            "must not type",
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    assert_eq!(error.action_dispatch, DispatchStatus::NotDispatched);
    let eligibility = error_report_check(&error, ActionabilityCheckName::FocusEligible);
    assert_eq!(eligibility.verdict, ActionabilityVerdict::Failed);
    assert!(eligibility.required);
    assert_eq!(eligibility.source, ActionabilitySource::NormalizedState);
    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 0);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert!(fixture.key_log.lock().unwrap().is_empty());
}

#[test]
fn targeted_type_rejects_a_disabled_field_before_native_focus_dispatch() {
    let mut tree = targeted_type_field_tree("Account name", false);
    tree.root.children[0].states.enabled = false;
    let mut fixture = targeted_type_glass(
        vec![tree],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 0),
            "must not type",
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::NotActionable);
    let enabled = error_report_check(&error, ActionabilityCheckName::Enabled);
    assert_eq!(enabled.verdict, ActionabilityVerdict::Failed);
    assert!(enabled.required);
    assert_eq!(enabled.source, ActionabilitySource::NormalizedState);
    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 0);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert!(fixture.key_log.lock().unwrap().is_empty());
}

#[test]
fn targeted_type_incomplete_query_keeps_resolution_error_actionability_unselected() {
    let mut tree = targeted_type_field_tree("Account name", false);
    tree.truncated = Some(Truncation {
        limit: TruncationLimit::Nodes,
        limit_value: 2,
        nodes_walked: 2,
    });
    let mut fixture = targeted_type_glass(
        vec![tree],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 0),
            "must not type",
        )
        .unwrap_err();

    assert_eq!(error.kind, SemanticActionFailureKind::IncompleteTree);
    assert_eq!(error.candidates.len(), 1);
    assert!(error.actionability.checks.is_empty());
    assert!(error.target.is_none());
    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 0);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert!(fixture.key_log.lock().unwrap().is_empty());
}

#[test]
fn targeted_type_uses_the_same_absolute_deadline_for_focus_confirmation_and_typing() {
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", true),
        ],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let outcome = fixture
        .glass
        .type_target_by(
            &targeted_type_params("Account name", ActionMode::Native, 500),
            "one deadline",
            Deadline::from_millis(1_000),
        )
        .unwrap();

    let ax_deadlines = fixture.ax_deadlines.lock().unwrap();
    assert_eq!(ax_deadlines.len(), 3);
    assert!(
        ax_deadlines
            .iter()
            .all(|deadline| *deadline == outcome.bound.deadline)
    );
    assert_eq!(
        fixture.key_deadlines.lock().unwrap().as_slice(),
        &[outcome.bound.deadline]
    );
}

#[test]
fn targeted_type_sanitizes_a_secret_bearing_key_backend_error_after_classification() {
    let secret = "scripted key failure";
    let mut fixture = targeted_type_glass(
        vec![
            targeted_type_field_tree("Account name", false),
            targeted_type_field_tree("Account name", true),
        ],
        InvokeBehavior::Succeed,
        full_state_coverage(),
        true,
    );
    let sink = RecordingSink::default();
    fixture.glass.set_audit_sink(Box::new(sink.clone()));
    fixture.glass.start(&spec()).unwrap();
    sink.0.lock().unwrap().clear();
    sink.3.lock().unwrap().clear();

    let error = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 500),
            secret,
        )
        .unwrap_err();

    assert_eq!(fixture.key_log.lock().unwrap().len(), 1);
    assert_eq!(error.action_dispatch, DispatchStatus::MayHaveDispatched);
    assert_eq!(error.retry, RetryGuidance::DoNotRetry);
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
    assert!(!format!("{:?}", error.source).contains(secret));
    assert!(error.source.is_none());
    assert!(!format!("{:?}", error.candidates).contains(secret));
    assert!(!format!("{:?}", error.actionability).contains(secret));
    assert_eq!(sink.0.lock().unwrap().as_slice(), &["type_target:false"]);
    let audits = sink.3.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert!(!audits[0].error.as_deref().unwrap().contains(secret));
}

#[test]
fn targeted_type_confirms_and_reports_a_substituted_native_focus_target() {
    let mut before = targeted_type_field_tree("Account name", false);
    before.root.children.push(AxNode {
        id: AxNodeId(2),
        role: AxRole::TextArea,
        raw_role: "text area".into(),
        name: Some("Actual editor".into()),
        description: None,
        value: Some("old".into()),
        states: AxStates {
            enabled: true,
            visible: true,
            focusable: true,
            editable: true,
            ..AxStates::default()
        },
        bounds: Some(AxRect {
            x: 20,
            y: 70,
            width: 160,
            height: 60,
        }),
        children: Vec::new(),
    });
    let mut after = before.clone();
    after.root.children[1].states.focused = true;
    let mut fixture = targeted_type_glass(
        vec![before, after],
        InvokeBehavior::SucceedOnAnother(2),
        full_state_coverage(),
        false,
    );
    fixture.glass.start(&spec()).unwrap();

    let outcome = fixture
        .glass
        .type_target(
            &targeted_type_params("Account name", ActionMode::Native, 0),
            "typed once",
        )
        .unwrap();

    assert_eq!(fixture.focus_calls.load(Ordering::Relaxed), 1);
    assert!(fixture.clicks.lock().unwrap().is_empty());
    assert_eq!(fixture.key_log.lock().unwrap().len(), 1);
    assert_eq!(outcome.target.id, AxNodeId(2));
    assert_eq!(outcome.target.role, AxRole::TextArea);
    assert_eq!(outcome.target.name.as_deref(), Some("Actual editor"));
    assert_eq!(
        outcome.focus,
        Some(MutationReport {
            method: ActionMethod::NativeAction {
                actuated: Some(AxNodeId(2)),
            },
            dispatch: DispatchStatus::Dispatched,
            confirmation: ConfirmationStatus::FocusConfirmed,
        })
    );
    let deadlines = fixture.ax_deadlines.lock().unwrap();
    assert_eq!(deadlines.len(), 3);
    assert!(
        deadlines
            .iter()
            .all(|deadline| *deadline == outcome.bound.deadline)
    );
    assert_eq!(
        fixture.key_deadlines.lock().unwrap().as_slice(),
        &[outcome.bound.deadline]
    );
}
