#![allow(dead_code)]

use super::a11y::SetValueExecution;
use super::*;
use crate::{
    ActionabilityCheck, ActionabilityCheckName, ActionabilityReport, ActionabilitySource,
    ActionabilityVerdict, AxStateCoverage, ScopeResolution, SemanticMatch, SemanticQuery,
    SemanticQueryResult, SemanticSelector, SemanticState, Whose,
};

pub const SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const SEMANTIC_ACTION_MAX_TIMEOUT_MS: u64 = 120_000;
pub const SEMANTIC_ACTION_STABILITY_MS: u64 = 100;
pub const SEMANTIC_ACTION_CANDIDATE_LIMIT: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticTarget {
    pub target: SemanticSelector,
    pub within: Option<SemanticSelector>,
}

impl SemanticTarget {
    pub fn uncovered_states(&self, coverage: AxStateCoverage) -> Vec<SemanticState> {
        self.target
            .states()
            .iter()
            .chain(self.within.iter().flat_map(SemanticSelector::states))
            .copied()
            .filter(|state| !coverage.covers_selector_state(*state))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionTarget {
    Id(AxNodeId),
    Semantic(SemanticTarget),
}

impl ActionTarget {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Id(_) => "id",
            Self::Semantic(_) => "semantic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionMode {
    Auto,
    Native,
    Pointer,
}

impl ActionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Native => "native",
            Self::Pointer => "pointer",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionMethod {
    NativeAction { actuated: Option<AxNodeId> },
    Pointer { native_fallback: Option<String> },
    AccessibilityValue,
    Keyboard,
}

impl ActionMethod {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NativeAction { .. } => "native_action",
            Self::Pointer { .. } => "pointer",
            Self::AccessibilityValue => "accessibility_value",
            Self::Keyboard => "keyboard",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchStatus {
    NotDispatched,
    Dispatched,
    MayHaveDispatched,
}

impl DispatchStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotDispatched => "not_dispatched",
            Self::Dispatched => "dispatched",
            Self::MayHaveDispatched => "may_have_dispatched",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationStatus {
    NotRequested,
    DispatchConfirmed,
    FocusConfirmed,
    ValueConfirmed,
    Unconfirmed,
}

impl ConfirmationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::DispatchConfirmed => "dispatch_confirmed",
            Self::FocusConfirmed => "focus_confirmed",
            Self::ValueConfirmed => "value_confirmed",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationReport {
    pub method: ActionMethod,
    pub dispatch: DispatchStatus,
    pub confirmation: ConfirmationStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionDeadline {
    pub deadline: Deadline,
    pub owner: Option<Whose>,
    pub allow_wait: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionReport {
    pub elapsed_ms: u64,
    pub scope: ScopeResolution,
    pub matches_in_walk: usize,
    pub search_complete: bool,
    pub timed_out_by: Option<Whose>,
    pub tree_truncated: bool,
    pub unreadable_subtrees: usize,
    pub unexposed_placeholders: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticActionOutcome {
    pub target: ElementInfo,
    pub resolution: Option<ResolutionReport>,
    pub actionability: ActionabilityReport,
    pub focus: Option<MutationReport>,
    pub action: MutationReport,
    pub bound: ActionDeadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticActionFailureKind {
    NoMatch,
    AmbiguousTarget,
    AmbiguousScope,
    IncompleteTree,
    UnprovenSelectorState,
    NotActionable,
    UnstableTarget,
    FocusUnconfirmed,
    UnsupportedMode,
    ActionDeadlineExceeded,
    SequenceDeadlineExceeded,
    ActionFailed,
}

impl SemanticActionFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoMatch => "no_match",
            Self::AmbiguousTarget => "ambiguous_target",
            Self::AmbiguousScope => "ambiguous_scope",
            Self::IncompleteTree => "incomplete_tree",
            Self::UnprovenSelectorState => "unproven_selector_state",
            Self::NotActionable => "not_actionable",
            Self::UnstableTarget => "unstable_target",
            Self::FocusUnconfirmed => "focus_unconfirmed",
            Self::UnsupportedMode => "unsupported_mode",
            Self::ActionDeadlineExceeded => "action_deadline_exceeded",
            Self::SequenceDeadlineExceeded => "sequence_deadline_exceeded",
            Self::ActionFailed => "action_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryGuidance {
    CorrectRequest,
    WaitOrRefine,
    Reobserve,
    SafeToRetry,
    DoNotRetry,
}

impl RetryGuidance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CorrectRequest => "correct_request",
            Self::WaitOrRefine => "wait_or_refine",
            Self::Reobserve => "reobserve",
            Self::SafeToRetry => "safe_to_retry",
            Self::DoNotRetry => "do_not_retry",
        }
    }
}

#[derive(Debug)]
pub struct SemanticActionError {
    pub kind: SemanticActionFailureKind,
    pub summary: &'static str,
    pub resolution: Option<ResolutionReport>,
    pub actionability: ActionabilityReport,
    pub focus: Option<MutationReport>,
    pub action_dispatch: DispatchStatus,
    pub candidates: Vec<SemanticMatch>,
    /// The resolved element for failures that occur after unique target resolution.
    pub target: Option<Box<ElementInfo>>,
    pub bound: ActionDeadline,
    pub retry: RetryGuidance,
    pub source: Option<GlassError>,
}

type SemanticActionResult<T> = std::result::Result<T, Box<SemanticActionError>>;

impl std::fmt::Display for SemanticActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.summary)
    }
}

impl std::error::Error for SemanticActionError {}

impl SemanticActionError {
    fn with_target(mut self: Box<Self>, target: ElementInfo) -> Box<Self> {
        self.target = Some(Box::new(target));
        self
    }

    fn proves_pre_dispatch_native_unavailable(&self) -> bool {
        self.action_dispatch == DispatchStatus::NotDispatched
            && self
                .source
                .as_ref()
                .is_some_and(GlassError::invoke_fallback_eligible)
    }

    fn safe_fallback_reason(&self) -> String {
        native_fallback_reason(
            self.source
                .as_ref()
                .expect("native fallback proof always retains its source"),
        )
    }
}

#[derive(Clone, Debug)]
pub struct ClickTargetParams {
    pub target: ActionTarget,
    pub mode: ActionMode,
    pub timeout_ms: Option<u64>,
    pub max_nodes: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct SetValueTargetParams {
    pub target: ActionTarget,
    pub timeout_ms: Option<u64>,
    pub max_nodes: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct TypeTargetParams {
    pub target: SemanticTarget,
    pub focus_mode: ActionMode,
    pub timeout_ms: u64,
    pub max_nodes: Option<usize>,
}

#[derive(Debug)]
pub(super) struct ResolvedSemanticTarget {
    pub(super) element: ElementInfo,
    pub(super) resolution: ResolutionReport,
    pub(super) target: AxTarget,
    pub(super) coverage: AxStateCoverage,
    pub(super) bound: ActionDeadline,
}

#[derive(Debug)]
struct ResolutionObservation {
    result: SemanticQueryResult,
    eligible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SemanticIdentity {
    role: AxRole,
    name: Option<String>,
    description: Option<String>,
}

impl From<&ElementInfo> for SemanticIdentity {
    fn from(element: &ElementInfo) -> Self {
        Self {
            role: element.role,
            name: element.name.clone(),
            description: element.description.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct StabilitySample {
    observed_at: std::time::Instant,
    identity: SemanticIdentity,
    bounds: AxRect,
    pointer: PlannedPointerInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlannedPointerInput {
    Click {
        point: (i32, i32),
    },
    TrailingToggle {
        segment: crate::Segment,
        probe_point: (i32, i32),
    },
}

impl PlannedPointerInput {
    fn is_inside_window(&self, window: (u32, u32)) -> bool {
        let inside =
            |(x, y): (i32, i32)| x >= 0 && y >= 0 && (x as u32) < window.0 && (y as u32) < window.1;
        match self {
            Self::Click { point } => inside(*point),
            Self::TrailingToggle {
                segment,
                probe_point,
            } => {
                inside((segment.from_x, segment.from_y))
                    && inside((segment.to_x, segment.to_y))
                    && inside(*probe_point)
            }
        }
    }
}

#[derive(Clone, Debug)]
struct PointerCandidate {
    element: ElementInfo,
    target: AxTarget,
    plan: PlannedPointerInput,
    window: (u32, u32),
}

#[derive(Debug)]
struct PointerResolutionObservation {
    resolution: ResolutionObservation,
    candidate: Option<PointerCandidate>,
    actionability: Option<ActionabilityReport>,
    stable: bool,
}

#[derive(Debug)]
struct ResolvedPointerTarget {
    candidate: PointerCandidate,
    resolution: ResolutionReport,
    coverage: AxStateCoverage,
    bound: ActionDeadline,
}

fn empty_error(
    kind: SemanticActionFailureKind,
    summary: &'static str,
    bound: ActionDeadline,
    retry: RetryGuidance,
    source: Option<GlassError>,
) -> Box<SemanticActionError> {
    Box::new(SemanticActionError {
        kind,
        summary,
        resolution: None,
        actionability: ActionabilityReport::default(),
        focus: None,
        action_dispatch: DispatchStatus::NotDispatched,
        candidates: Vec::new(),
        target: None,
        bound,
        retry,
        source,
    })
}

fn request_error(summary: &'static str, sequence_deadline: Deadline) -> Box<SemanticActionError> {
    empty_error(
        SemanticActionFailureKind::UnsupportedMode,
        summary,
        ActionDeadline {
            deadline: sequence_deadline,
            owner: sequence_deadline.instant().map(|_| Whose::Caller),
            allow_wait: false,
        },
        RetryGuidance::CorrectRequest,
        None,
    )
}

fn target_deadline(
    source: &ActionTarget,
    timeout_ms: Option<u64>,
    max_nodes: Option<usize>,
    sequence_deadline: Deadline,
) -> SemanticActionResult<ActionDeadline> {
    let now = std::time::Instant::now();
    match source {
        ActionTarget::Id(_) => {
            if timeout_ms.is_some() || max_nodes.is_some() {
                return Err(request_error(
                    "id targets do not accept timeout_ms or max_nodes",
                    sequence_deadline,
                ));
            }
            if sequence_deadline.instant().is_some() {
                Ok(ActionDeadline {
                    deadline: sequence_deadline,
                    owner: Some(Whose::Caller),
                    allow_wait: true,
                })
            } else {
                Ok(ActionDeadline {
                    deadline: Deadline::UNBOUNDED,
                    owner: None,
                    allow_wait: true,
                })
            }
        }
        ActionTarget::Semantic(_) => {
            let timeout_ms = timeout_ms.unwrap_or(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS);
            if timeout_ms > SEMANTIC_ACTION_MAX_TIMEOUT_MS {
                return Err(request_error(
                    "timeout_ms exceeds the semantic action maximum",
                    sequence_deadline,
                ));
            }
            if timeout_ms == 0 {
                return Ok(ActionDeadline {
                    deadline: sequence_deadline,
                    owner: sequence_deadline.instant().map(|_| Whose::Caller),
                    allow_wait: false,
                });
            }
            let (duration, owner) =
                sequence_deadline.budget(std::time::Duration::from_millis(timeout_ms), now);
            Ok(ActionDeadline {
                deadline: Deadline::at(now + duration),
                owner: Some(owner),
                allow_wait: true,
            })
        }
    }
}

fn resolution_report(
    result: &SemanticQueryResult,
    elapsed_ms: u64,
    timed_out_by: Option<Whose>,
) -> ResolutionReport {
    ResolutionReport {
        elapsed_ms,
        scope: result.scope,
        matches_in_walk: result.matches_in_walk,
        search_complete: result.search_complete,
        timed_out_by,
        tree_truncated: result.tree_truncated.is_some(),
        unreadable_subtrees: result.unreadable_subtrees,
        unexposed_placeholders: result.unexposed_placeholders,
    }
}

fn classified_resolution_error(
    observation: ResolutionObservation,
    report: ResolutionReport,
    bound: ActionDeadline,
) -> Box<SemanticActionError> {
    let (kind, summary, retry) =
        if matches!(observation.result.scope, ScopeResolution::Ambiguous { .. }) {
            (
                SemanticActionFailureKind::AmbiguousScope,
                "semantic scope matched more than one element",
                RetryGuidance::CorrectRequest,
            )
        } else if observation.result.matches_in_walk >= 2 {
            (
                SemanticActionFailureKind::AmbiguousTarget,
                "semantic target matched more than one element",
                RetryGuidance::WaitOrRefine,
            )
        } else if !observation.result.search_complete {
            (
                SemanticActionFailureKind::IncompleteTree,
                "accessibility tree could not prove a unique semantic target",
                RetryGuidance::Reobserve,
            )
        } else if observation.result.matches_in_walk == 0 {
            (
                SemanticActionFailureKind::NoMatch,
                "semantic target did not match any element",
                RetryGuidance::WaitOrRefine,
            )
        } else {
            debug_assert!(!observation.eligible);
            (
                SemanticActionFailureKind::NotActionable,
                "semantic target is not actionable",
                RetryGuidance::Reobserve,
            )
        };
    Box::new(SemanticActionError {
        kind,
        summary,
        resolution: Some(report),
        actionability: ActionabilityReport::default(),
        focus: None,
        action_dispatch: DispatchStatus::NotDispatched,
        candidates: observation.result.matches,
        target: None,
        bound,
        retry,
        source: None,
    })
}

fn source_error(source: GlassError, bound: ActionDeadline) -> Box<SemanticActionError> {
    let owner = source.bound_owner().or_else(|| {
        matches!(&source, GlassError::AccessibilityNotReady(_))
            .then_some(bound.owner)
            .flatten()
    });
    let kind = match owner {
        Some(Whose::Caller) => SemanticActionFailureKind::SequenceDeadlineExceeded,
        Some(Whose::Callee) => SemanticActionFailureKind::ActionDeadlineExceeded,
        None => SemanticActionFailureKind::ActionFailed,
    };
    let summary = match kind {
        SemanticActionFailureKind::SequenceDeadlineExceeded => {
            "semantic action sequence deadline exceeded"
        }
        SemanticActionFailureKind::ActionDeadlineExceeded => "semantic action deadline exceeded",
        _ => "semantic action target resolution failed",
    };
    empty_error(
        kind,
        summary,
        bound,
        RetryGuidance::SafeToRetry,
        Some(source),
    )
}

fn pointer_plan(
    element: &ElementInfo,
    window: (u32, u32),
    trailing_toggle_backend: bool,
) -> Option<PlannedPointerInput> {
    const ROW_ASPECT: u32 = 4;
    let bounds = element.bounds?;
    let plan = if element.states.checkable
        && trailing_toggle_backend
        && bounds.width > bounds.height.saturating_mul(ROW_ASPECT)
    {
        let segment = bounds.trailing_toggle_swipe(window.0, window.1)?;
        let probe_point = (
            segment.from_x + (segment.to_x - segment.from_x) / 2,
            segment.from_y + (segment.to_y - segment.from_y) / 2,
        );
        PlannedPointerInput::TrailingToggle {
            segment,
            probe_point,
        }
    } else {
        PlannedPointerInput::Click {
            point: bounds.clamped_center(window.0, window.1)?,
        }
    };
    plan.is_inside_window(window).then_some(plan)
}

fn ax_target(element: &ElementInfo) -> AxTarget {
    AxTarget {
        id: element.id,
        role: element.role,
        name: element.name.clone(),
        bounds: element.bounds,
        value: element.value.clone(),
    }
}

fn role_supports_set_value(element: &ElementInfo, coverage: AxStateCoverage) -> bool {
    set_value_eligibility(element, coverage) == ActionabilityVerdict::Passed
}

fn set_value_eligibility(element: &ElementInfo, coverage: AxStateCoverage) -> ActionabilityVerdict {
    if matches!(
        element.role,
        AxRole::ComboBox
            | AxRole::Slider
            | AxRole::SpinButton
            | AxRole::ToggleButton
            | AxRole::RadioButton
            | AxRole::CheckBox
    ) || (coverage.editable && element.states.editable)
        || (coverage.checkable && element.states.checkable)
    {
        ActionabilityVerdict::Passed
    } else if coverage.editable && coverage.checkable {
        ActionabilityVerdict::Failed
    } else {
        ActionabilityVerdict::Unproven
    }
}

fn set_value_actionability(
    element: &ElementInfo,
    coverage: AxStateCoverage,
    window: (u32, u32),
    legacy_id: bool,
) -> ActionabilityReport {
    let mut report = ActionabilityReport::evaluate_click(
        element,
        coverage,
        None,
        window,
        crate::PointerHit::Inconclusive,
        legacy_id,
        false,
    );
    let source = if legacy_id {
        ActionabilitySource::LegacyCache
    } else {
        ActionabilitySource::NormalizedState
    };
    let check = ActionabilityCheck::new(
        ActionabilityCheckName::FocusEligible,
        set_value_eligibility(element, coverage),
        !legacy_id,
        source,
    );
    let insert_at = report
        .checks
        .iter()
        .position(|existing| existing.name == ActionabilityCheckName::Stable)
        .unwrap_or(report.checks.len());
    report.checks.insert(insert_at, check);
    report
}

fn targeted_type_eligibility(
    element: &ElementInfo,
    coverage: AxStateCoverage,
) -> ActionabilityVerdict {
    if matches!(element.role, AxRole::TextField | AxRole::TextArea)
        || (coverage.editable && element.states.editable)
    {
        ActionabilityVerdict::Passed
    } else if coverage.editable {
        ActionabilityVerdict::Failed
    } else {
        ActionabilityVerdict::Unproven
    }
}

fn targeted_type_actionability(
    element: &ElementInfo,
    coverage: AxStateCoverage,
    stable: Option<bool>,
    window: (u32, u32),
    pointer: bool,
) -> ActionabilityReport {
    let mut report = ActionabilityReport::evaluate_click(
        element,
        coverage,
        stable,
        window,
        crate::PointerHit::Inconclusive,
        false,
        pointer,
    );
    insert_targeted_type_eligibility(&mut report, element, coverage);
    report
}

fn insert_targeted_type_eligibility(
    report: &mut ActionabilityReport,
    element: &ElementInfo,
    coverage: AxStateCoverage,
) {
    let insert_at = report
        .checks
        .iter()
        .position(|check| check.name == ActionabilityCheckName::Stable)
        .unwrap_or(report.checks.len());
    report.checks.insert(
        insert_at,
        ActionabilityCheck::new(
            ActionabilityCheckName::FocusEligible,
            targeted_type_eligibility(element, coverage),
            true,
            ActionabilitySource::NormalizedState,
        ),
    );
}

fn record_focus_confirmation(
    report: &mut ActionabilityReport,
    coverage: AxStateCoverage,
    confirmed: bool,
) {
    let verdict = if !coverage.focused {
        ActionabilityVerdict::Unproven
    } else if confirmed {
        ActionabilityVerdict::Passed
    } else {
        ActionabilityVerdict::Failed
    };
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::Focused,
        verdict,
        true,
        ActionabilitySource::ConfirmationPoll,
    ));
}

#[derive(Debug)]
struct ConfirmedFocus {
    element: ElementInfo,
    resolution: ResolutionReport,
    actionability: ActionabilityReport,
    focus: MutationReport,
    bound: ActionDeadline,
}

fn focus_dispatch(source: &GlassError, dispatch_started: bool) -> DispatchStatus {
    if !dispatch_started
        || source.invoke_fallback_eligible()
        || source.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched)
        || matches!(
            source.cause(),
            GlassError::NoActiveSession
                | GlassError::NoAxSnapshot
                | GlassError::AxElementNotFound(_)
                | GlassError::AxElementNotClickable(_)
                | GlassError::AxElementInUnmappedPopover(_)
                | GlassError::WindowNotFound
        )
    {
        DispatchStatus::NotDispatched
    } else {
        DispatchStatus::MayHaveDispatched
    }
}

fn focus_source_error(
    source: GlassError,
    target: ElementInfo,
    resolution: Option<ResolutionReport>,
    actionability: ActionabilityReport,
    method: ActionMethod,
    bound: ActionDeadline,
    dispatch_started: bool,
) -> Box<SemanticActionError> {
    let dispatch = focus_dispatch(&source, dispatch_started);
    let mut error = source_error(source, bound);
    error.summary = "semantic target focus failed";
    error.target = Some(Box::new(target));
    error.resolution = resolution;
    error.actionability = actionability;
    error.focus = Some(MutationReport {
        method,
        dispatch,
        confirmation: ConfirmationStatus::Unconfirmed,
    });
    error.action_dispatch = DispatchStatus::NotDispatched;
    if dispatch != DispatchStatus::NotDispatched {
        error.retry = RetryGuidance::DoNotRetry;
    }
    error
}

fn focus_unconfirmed_error(
    source: Option<GlassError>,
    target: ElementInfo,
    resolution: ResolutionReport,
    mut actionability: ActionabilityReport,
    coverage: AxStateCoverage,
    method: ActionMethod,
    bound: ActionDeadline,
) -> Box<SemanticActionError> {
    record_focus_confirmation(&mut actionability, coverage, false);
    let mut error = actionability_error(
        FailureSummary {
            kind: SemanticActionFailureKind::FocusUnconfirmed,
            summary: "semantic target focus could not be confirmed",
        },
        Some(resolution),
        actionability,
        bound,
        RetryGuidance::Reobserve,
        source,
        DispatchStatus::NotDispatched,
    )
    .with_target(target);
    error.focus = Some(MutationReport {
        method,
        dispatch: DispatchStatus::Dispatched,
        confirmation: ConfirmationStatus::Unconfirmed,
    });
    error
}

fn key_source_error(source: GlassError, focused: ConfirmedFocus) -> Box<SemanticActionError> {
    let proven_not_dispatched =
        source.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched);
    let mut error = source_error(source, focused.bound);
    error.summary = "semantic targeted typing failed";
    error.target = Some(Box::new(focused.element.clone()));
    error.resolution = Some(focused.resolution);
    error.actionability = focused.actionability;
    error.focus = Some(focused.focus);
    error.action_dispatch = if proven_not_dispatched {
        DispatchStatus::NotDispatched
    } else {
        DispatchStatus::MayHaveDispatched
    };
    error.retry = if proven_not_dispatched {
        RetryGuidance::SafeToRetry
    } else {
        RetryGuidance::DoNotRetry
    };
    error.source = None;
    error
}

struct FailureSummary {
    kind: SemanticActionFailureKind,
    summary: &'static str,
}

fn actionability_error(
    failure: FailureSummary,
    resolution: Option<ResolutionReport>,
    actionability: ActionabilityReport,
    bound: ActionDeadline,
    retry: RetryGuidance,
    source: Option<GlassError>,
    dispatch: DispatchStatus,
) -> Box<SemanticActionError> {
    Box::new(SemanticActionError {
        kind: failure.kind,
        summary: failure.summary,
        resolution,
        actionability,
        focus: None,
        action_dispatch: dispatch,
        candidates: Vec::new(),
        target: None,
        bound,
        retry,
        source,
    })
}

fn action_source_error(
    source: GlassError,
    target: Option<ElementInfo>,
    resolution: Option<ResolutionReport>,
    actionability: ActionabilityReport,
    bound: ActionDeadline,
    dispatch_started: bool,
) -> Box<SemanticActionError> {
    let proves_not_dispatched = !dispatch_started
        || source.invoke_fallback_eligible()
        || source.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched)
        || matches!(
            &source,
            GlassError::NoAxSnapshot
                | GlassError::AxElementNotFound(_)
                | GlassError::AxElementNotClickable(_)
                | GlassError::AxElementInUnmappedPopover(_)
                | GlassError::WindowNotFound
        );
    let mut error = source_error(source, bound);
    error.summary = "semantic action failed";
    error.target = target.map(Box::new);
    error.resolution = resolution;
    error.actionability = actionability;
    error.action_dispatch = if proves_not_dispatched {
        DispatchStatus::NotDispatched
    } else if dispatch_started {
        error
            .source
            .as_ref()
            .and_then(GlassError::bound_dispatch)
            .map_or(
                DispatchStatus::MayHaveDispatched,
                |dispatch| match dispatch {
                    crate::BoundDispatch::NotDispatched => DispatchStatus::NotDispatched,
                    crate::BoundDispatch::MayHaveDispatched => DispatchStatus::MayHaveDispatched,
                },
            )
    } else {
        unreachable!("non-dispatching failures are classified above")
    };
    if dispatch_started && error.action_dispatch != DispatchStatus::NotDispatched {
        error.retry = RetryGuidance::DoNotRetry;
    }
    error
}

fn set_value_source_error(
    source: GlassError,
    target: Option<ElementInfo>,
    resolution: Option<ResolutionReport>,
    actionability: ActionabilityReport,
    bound: ActionDeadline,
) -> Box<SemanticActionError> {
    let possible_dispatch = source.set_value_failed_after_writing()
        || source.bound_dispatch() == Some(crate::BoundDispatch::MayHaveDispatched);
    let cause = source.cause();
    let proven_pre_dispatch = source.bound_dispatch() == Some(crate::BoundDispatch::NotDispatched)
        || matches!(
            cause,
            GlassError::NoActiveSession
                | GlassError::NoAxSnapshot
                | GlassError::AxUnsupported
                | GlassError::AxElementChanged(_)
                | GlassError::AxElementGone(_)
                | GlassError::AxElementNotFound(_)
                | GlassError::AxElementNotEditable(_)
                | GlassError::AxValueNotBoolean(_, _)
        );
    let retry = if possible_dispatch || !proven_pre_dispatch {
        RetryGuidance::DoNotRetry
    } else if matches!(
        cause,
        GlassError::NoActiveSession
            | GlassError::NoAxSnapshot
            | GlassError::AxElementChanged(_)
            | GlassError::AxElementGone(_)
            | GlassError::AxElementNotFound(_)
    ) {
        RetryGuidance::Reobserve
    } else if matches!(
        cause,
        GlassError::AxUnsupported
            | GlassError::AxElementNotEditable(_)
            | GlassError::AxValueNotBoolean(_, _)
    ) {
        RetryGuidance::CorrectRequest
    } else {
        RetryGuidance::SafeToRetry
    };
    let mut error = source_error(source, bound);
    error.summary = "semantic set-value action failed";
    error.target = target.map(Box::new);
    error.resolution = resolution;
    error.actionability = actionability;
    error.action_dispatch = if possible_dispatch || !proven_pre_dispatch {
        DispatchStatus::MayHaveDispatched
    } else {
        DispatchStatus::NotDispatched
    };
    error.retry = retry;
    error
}

fn native_fallback_reason(source: &GlassError) -> String {
    match source {
        GlassError::AxUnsupported => "backend has no native action path".into(),
        GlassError::AxActionUnavailable(_) => "element exposes no activation action".into(),
        _ => "native accessibility action did not dispatch".into(),
    }
}

impl Glass {
    pub(super) fn resolve_semantic_target(
        &mut self,
        target: &SemanticTarget,
        max_nodes: Option<usize>,
        timeout_ms: u64,
        sequence_deadline: Deadline,
        eligibility: impl Fn(&ElementInfo, AxStateCoverage) -> bool,
    ) -> SemanticActionResult<ResolvedSemanticTarget> {
        let bound = target_deadline(
            &ActionTarget::Semantic(target.clone()),
            Some(timeout_ms),
            max_nodes,
            sequence_deadline,
        )?;
        self.resolve_semantic_target_by_bound(
            target,
            max_nodes,
            sequence_deadline,
            bound,
            eligibility,
        )
    }

    fn resolve_semantic_target_by_bound(
        &mut self,
        target: &SemanticTarget,
        max_nodes: Option<usize>,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
        eligibility: impl Fn(&ElementInfo, AxStateCoverage) -> bool,
    ) -> SemanticActionResult<ResolvedSemanticTarget> {
        let coverage = {
            let active = self
                .active_mut()
                .map_err(|source| source_error(source, bound))?;
            active
                .accessibility
                .as_ref()
                .ok_or_else(|| source_error(GlassError::AxUnsupported, bound))?
                .state_coverage()
        };
        if !target.uncovered_states(coverage).is_empty() {
            return Err(empty_error(
                SemanticActionFailureKind::UnprovenSelectorState,
                "accessibility backend cannot prove a requested selector state",
                bound,
                RetryGuidance::CorrectRequest,
                None,
            ));
        }
        self.set_a11y_limits(max_nodes)
            .map_err(|source| source_error(source, bound))?;
        let query = SemanticQuery::new(
            target.target.clone(),
            target.within.clone(),
            SEMANTIC_ACTION_CANDIDATE_LIMIT,
        )
        .expect("the fixed candidate limit is valid");
        let poll = self
            .poll_accessibility_until_by_deadline(
                a11y_poll::A11yPollCadence {
                    interval_ms: SEMANTIC_ACTION_STABILITY_MS,
                    reread_after: std::time::Duration::from_secs(1),
                },
                a11y_poll::A11yPollBound {
                    action_deadline: bound.deadline,
                    whose: bound.owner.unwrap_or(Whose::Callee),
                    allow_wait: bound.allow_wait,
                    sequence_deadline,
                },
                "resolve semantic action target",
                |tree| {
                    let result = tree.semantic_query(&query);
                    let eligible = result.matches_in_walk == 1
                        && result
                            .matches
                            .first()
                            .is_some_and(|candidate| eligibility(&candidate.element, coverage));
                    ResolutionObservation { result, eligible }
                },
                |observation| {
                    matches!(
                        observation.result.scope,
                        ScopeResolution::Unscoped | ScopeResolution::Resolved(_)
                    ) && observation.result.matches_in_walk == 1
                        && observation.result.search_complete
                        && observation.result.matches.len() == 1
                        && observation.eligible
                },
            )
            .map_err(|source| source_error(source, bound))?;
        let report =
            resolution_report(&poll.observation.result, poll.elapsed_ms, poll.timed_out_by);
        if !poll.satisfied {
            return Err(classified_resolution_error(poll.observation, report, bound));
        }

        let id = poll.observation.result.matches[0].element.id;
        let node = self
            .active
            .as_ref()
            .and_then(|active| active.last_ax.as_ref())
            .and_then(|tree| tree.find(id))
            .ok_or_else(|| source_error(GlassError::AxElementNotFound(id.0), bound))?;
        let element = ElementInfo::from_node(node);
        let target = AxTarget {
            id: node.id,
            role: node.role,
            name: node.name.clone(),
            bounds: node.bounds,
            value: node.value.clone(),
        };
        Ok(ResolvedSemanticTarget {
            element,
            resolution: report,
            target,
            coverage,
            bound,
        })
    }

    fn resolve_stable_pointer_target(
        &mut self,
        target: &SemanticTarget,
        max_nodes: Option<usize>,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
    ) -> SemanticActionResult<ResolvedPointerTarget> {
        let coverage = {
            let active = self
                .active_mut()
                .map_err(|source| source_error(source, bound))?;
            active
                .accessibility
                .as_ref()
                .ok_or_else(|| source_error(GlassError::AxUnsupported, bound))?
                .state_coverage()
        };
        if !target.uncovered_states(coverage).is_empty() {
            return Err(empty_error(
                SemanticActionFailureKind::UnprovenSelectorState,
                "accessibility backend cannot prove a requested selector state",
                bound,
                RetryGuidance::CorrectRequest,
                None,
            ));
        }
        self.set_a11y_limits(max_nodes)
            .map_err(|source| source_error(source, bound))?;
        let query = SemanticQuery::new(
            target.target.clone(),
            target.within.clone(),
            SEMANTIC_ACTION_CANDIDATE_LIMIT,
        )
        .expect("the fixed candidate limit is valid");
        let trailing_toggle_backend = {
            let active = self
                .active
                .as_ref()
                .ok_or_else(|| source_error(GlassError::NoActiveSession, bound))?;
            active.platform.a11y_toggle_control_at_trailing_edge()
        };
        let mut sample: Option<StabilitySample> = None;
        let poll = self
            .poll_accessibility_until_by_deadline_with_window(
                a11y_poll::A11yPollCadence {
                    interval_ms: SEMANTIC_ACTION_STABILITY_MS,
                    reread_after: std::time::Duration::from_millis(SEMANTIC_ACTION_STABILITY_MS),
                },
                a11y_poll::A11yPollBound {
                    action_deadline: bound.deadline,
                    whose: bound.owner.unwrap_or(Whose::Callee),
                    allow_wait: bound.allow_wait,
                    sequence_deadline,
                },
                "stabilize semantic pointer target",
                |tree, window| {
                    let result = tree.semantic_query(&query);
                    let complete_unique = matches!(
                        result.scope,
                        ScopeResolution::Unscoped | ScopeResolution::Resolved(_)
                    ) && result.matches_in_walk == 1
                        && result.search_complete
                        && result.matches.len() == 1;
                    let mut actionability = None;
                    let candidate = if complete_unique {
                        let element = result.matches[0].element.clone();
                        let plan = pointer_plan(&element, window, trailing_toggle_backend);
                        let mut report = ActionabilityReport::evaluate_click(
                            &element,
                            coverage,
                            None,
                            window,
                            crate::PointerHit::Inconclusive,
                            false,
                            true,
                        );
                        if plan.is_none() {
                            report.fail_in_window();
                        }
                        let candidate = if report.blocking().is_none() {
                            plan.map(|plan| PointerCandidate {
                                target: ax_target(&element),
                                element,
                                plan,
                                window,
                            })
                        } else {
                            None
                        };
                        actionability = Some(report);
                        candidate
                    } else {
                        None
                    };
                    let now = std::time::Instant::now();
                    let mut stable = false;
                    if let Some(candidate) = &candidate {
                        let bounds = candidate
                            .element
                            .bounds
                            .expect("a pointer candidate always has bounds");
                        let identity = SemanticIdentity::from(&candidate.element);
                        match &sample {
                            Some(previous)
                                if previous.identity == identity
                                    && previous.bounds == bounds
                                    && previous.pointer == candidate.plan =>
                            {
                                stable = now.duration_since(previous.observed_at)
                                    >= std::time::Duration::from_millis(
                                        SEMANTIC_ACTION_STABILITY_MS,
                                    );
                            }
                            _ => {
                                sample = Some(StabilitySample {
                                    observed_at: now,
                                    identity,
                                    bounds,
                                    pointer: candidate.plan.clone(),
                                });
                            }
                        }
                        actionability = Some(ActionabilityReport::evaluate_click(
                            &candidate.element,
                            coverage,
                            Some(stable),
                            candidate.window,
                            crate::PointerHit::Inconclusive,
                            false,
                            true,
                        ));
                    } else {
                        sample = None;
                    }
                    PointerResolutionObservation {
                        resolution: ResolutionObservation {
                            result,
                            eligible: candidate.is_some(),
                        },
                        candidate,
                        actionability,
                        stable,
                    }
                },
                |observation| observation.stable,
            )
            .map_err(|source| source_error(source, bound))?;
        let report = resolution_report(
            &poll.observation.resolution.result,
            poll.elapsed_ms,
            poll.timed_out_by,
        );
        if !poll.satisfied {
            if let Some(candidate) = &poll.observation.candidate {
                let target = candidate.element.clone();
                return Err(actionability_error(
                    FailureSummary {
                        kind: SemanticActionFailureKind::UnstableTarget,
                        summary: "semantic pointer target did not remain stable",
                    },
                    Some(report),
                    poll.observation
                        .actionability
                        .expect("a pointer candidate has an actionability report"),
                    bound,
                    RetryGuidance::WaitOrRefine,
                    None,
                    DispatchStatus::NotDispatched,
                )
                .with_target(target));
            }
            let mut error = classified_resolution_error(poll.observation.resolution, report, bound);
            error.actionability = poll.observation.actionability.unwrap_or_default();
            return Err(error);
        }
        Ok(ResolvedPointerTarget {
            candidate: poll
                .observation
                .candidate
                .expect("a satisfied stability observation has a candidate"),
            resolution: report,
            coverage,
            bound,
        })
    }

    fn accessibility_context_for_action(&mut self, deadline: Deadline) -> Result<AxContext> {
        let active = self.active_mut()?;
        let pids = active.platform.app_pids_by(deadline)?;
        Ok(AxContext {
            pids,
            window: active.geometry.clone(),
            window_handle: active.platform.active_window_handle(),
            a11y_bus_addr: active.platform.a11y_bus_addr(),
            limits: active.a11y_limits,
            deadline,
        })
    }

    fn refresh_action_window(&mut self, deadline: Deadline) -> Result<(u32, u32)> {
        let active = self.active_mut()?;
        let window = active.platform.window_by(&WindowOp::Geometry, deadline)?;
        let dimensions = (window.width, window.height);
        active.geometry = window;
        Ok(dimensions)
    }

    fn probe_semantic_pointer(
        &mut self,
        target: &AxTarget,
        point: (i32, i32),
        deadline: Deadline,
    ) -> Result<crate::PointerHit> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("pointer hit probe"));
        }
        let ctx = self.accessibility_context_for_action(deadline)?;
        let active = self.active_mut()?;
        let hit = active
            .accessibility
            .as_mut()
            .ok_or(GlassError::AxUnsupported)?
            .pointer_target_at(&ctx, target, point)?;
        active.pump();
        Ok(hit)
    }

    fn dispatch_native_click(
        &mut self,
        resolved: ResolvedSemanticTarget,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let window = {
            let active = self.active.as_ref().ok_or_else(|| {
                let mut error = source_error(GlassError::NoActiveSession, resolved.bound);
                error.target = Some(Box::new(resolved.element.clone()));
                error.resolution = Some(resolved.resolution.clone());
                error
            })?;
            (active.geometry.width, active.geometry.height)
        };
        let mut actionability = ActionabilityReport::evaluate_click(
            &resolved.element,
            resolved.coverage,
            None,
            window,
            crate::PointerHit::Inconclusive,
            false,
            false,
        );
        if actionability.blocking().is_some() {
            return Err(actionability_error(
                FailureSummary {
                    kind: SemanticActionFailureKind::NotActionable,
                    summary: "semantic target is not actionable",
                },
                Some(resolved.resolution),
                actionability,
                resolved.bound,
                RetryGuidance::Reobserve,
                None,
                DispatchStatus::NotDispatched,
            )
            .with_target(resolved.element.clone()));
        }
        let actuated = self
            .try_native_invoke(resolved.element.id, resolved.bound.deadline)
            .map_err(|source| {
                action_source_error(
                    source,
                    Some(resolved.element.clone()),
                    Some(resolved.resolution.clone()),
                    actionability.clone(),
                    resolved.bound,
                    true,
                )
            })?;
        actionability.pass_backend_fingerprint();
        Ok(SemanticActionOutcome {
            target: resolved.element,
            resolution: Some(resolved.resolution),
            actionability,
            focus: None,
            action: MutationReport {
                method: ActionMethod::NativeAction { actuated },
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::DispatchConfirmed,
            },
            bound: resolved.bound,
        })
    }

    fn dispatch_pointer_click(
        &mut self,
        resolved: ResolvedPointerTarget,
        native_fallback: Option<String>,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let sample_actionability = ActionabilityReport::evaluate_click(
            &resolved.candidate.element,
            resolved.coverage,
            Some(true),
            resolved.candidate.window,
            crate::PointerHit::Inconclusive,
            false,
            true,
        );
        let window = self
            .refresh_action_window(resolved.bound.deadline)
            .map_err(|source| {
                action_source_error(
                    source,
                    Some(resolved.candidate.element.clone()),
                    Some(resolved.resolution.clone()),
                    sample_actionability,
                    resolved.bound,
                    false,
                )
            })?;
        let mut actionability = ActionabilityReport::evaluate_click(
            &resolved.candidate.element,
            resolved.coverage,
            Some(true),
            window,
            crate::PointerHit::Inconclusive,
            false,
            true,
        );
        if !resolved.candidate.plan.is_inside_window(window) {
            actionability.fail_in_window();
        }
        if actionability.blocking().is_some() {
            return Err(actionability_error(
                FailureSummary {
                    kind: SemanticActionFailureKind::NotActionable,
                    summary: "semantic pointer target is not actionable",
                },
                Some(resolved.resolution),
                actionability,
                resolved.bound,
                RetryGuidance::Reobserve,
                None,
                DispatchStatus::NotDispatched,
            )
            .with_target(resolved.candidate.element.clone()));
        }
        let probe_point = match &resolved.candidate.plan {
            PlannedPointerInput::Click { point } => *point,
            PlannedPointerInput::TrailingToggle { probe_point, .. } => *probe_point,
        };
        let hit = self
            .probe_semantic_pointer(
                &resolved.candidate.target,
                probe_point,
                resolved.bound.deadline,
            )
            .map_err(|source| {
                action_source_error(
                    source,
                    Some(resolved.candidate.element.clone()),
                    Some(resolved.resolution.clone()),
                    actionability.clone(),
                    resolved.bound,
                    false,
                )
            })?;
        actionability = ActionabilityReport::evaluate_click(
            &resolved.candidate.element,
            resolved.coverage,
            Some(true),
            window,
            hit,
            false,
            true,
        );
        if actionability.blocking().is_some() {
            return Err(actionability_error(
                FailureSummary {
                    kind: SemanticActionFailureKind::NotActionable,
                    summary: "semantic pointer target is not actionable",
                },
                Some(resolved.resolution),
                actionability,
                resolved.bound,
                RetryGuidance::Reobserve,
                None,
                DispatchStatus::NotDispatched,
            )
            .with_target(resolved.candidate.element.clone()));
        }
        self.click_element_pointer_only(
            resolved.candidate.element.id,
            Some(&resolved.candidate.plan),
            resolved.bound.deadline,
        )
        .map_err(|source| {
            action_source_error(
                source,
                Some(resolved.candidate.element.clone()),
                Some(resolved.resolution.clone()),
                actionability.clone(),
                resolved.bound,
                true,
            )
        })?;
        Ok(SemanticActionOutcome {
            target: resolved.candidate.element,
            resolution: Some(resolved.resolution),
            actionability,
            focus: None,
            action: MutationReport {
                method: ActionMethod::Pointer { native_fallback },
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::DispatchConfirmed,
            },
            bound: resolved.bound,
        })
    }

    fn legacy_click_snapshot(
        &self,
        id: AxNodeId,
    ) -> Option<(ElementInfo, AxStateCoverage, (u32, u32))> {
        let active = self.active.as_ref()?;
        let node = active.last_ax.as_ref()?.find(id)?;
        let coverage = active
            .accessibility
            .as_ref()
            .map_or(AxStateCoverage::NONE, |reader| reader.state_coverage());
        Some((
            ElementInfo::from_node(node),
            coverage,
            (active.geometry.width, active.geometry.height),
        ))
    }

    fn legacy_click_actionability(&self, id: AxNodeId, pointer: bool) -> ActionabilityReport {
        self.legacy_click_snapshot(id)
            .map(|(element, coverage, window)| {
                ActionabilityReport::evaluate_click(
                    &element,
                    coverage,
                    None,
                    window,
                    crate::PointerHit::Inconclusive,
                    true,
                    pointer,
                )
            })
            .unwrap_or_default()
    }

    fn legacy_click_outcome(
        &self,
        id: AxNodeId,
        method: ActionMethod,
        pointer: bool,
        bound: ActionDeadline,
    ) -> SemanticActionOutcome {
        let (element, coverage, window) = self
            .legacy_click_snapshot(id)
            .expect("a successful legacy click retains its cached target");
        let mut actionability = ActionabilityReport::evaluate_click(
            &element,
            coverage,
            None,
            window,
            crate::PointerHit::Inconclusive,
            true,
            pointer,
        );
        if !pointer {
            actionability.pass_backend_fingerprint();
        }
        SemanticActionOutcome {
            target: element,
            resolution: None,
            actionability,
            focus: None,
            action: MutationReport {
                method,
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::DispatchConfirmed,
            },
            bound,
        }
    }

    fn click_native_once(
        &mut self,
        target: &ActionTarget,
        max_nodes: Option<usize>,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        match target {
            ActionTarget::Id(id) => {
                let actionability = self.legacy_click_actionability(*id, false);
                let actuated = self
                    .try_native_invoke(*id, bound.deadline)
                    .map_err(|source| {
                        action_source_error(source, None, None, actionability, bound, true)
                    })?;
                Ok(self.legacy_click_outcome(
                    *id,
                    ActionMethod::NativeAction { actuated },
                    false,
                    bound,
                ))
            }
            ActionTarget::Semantic(target) => {
                let window = self
                    .active
                    .as_ref()
                    .map(|active| (active.geometry.width, active.geometry.height))
                    .unwrap_or_default();
                let resolved = self.resolve_semantic_target_by_bound(
                    target,
                    max_nodes,
                    sequence_deadline,
                    bound,
                    |element, coverage| {
                        ActionabilityReport::evaluate_click(
                            element,
                            coverage,
                            None,
                            window,
                            crate::PointerHit::Inconclusive,
                            false,
                            false,
                        )
                        .blocking()
                        .is_none()
                    },
                )?;
                self.dispatch_native_click(resolved)
            }
        }
    }

    fn click_pointer_once(
        &mut self,
        target: &ActionTarget,
        max_nodes: Option<usize>,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
        native_fallback: Option<String>,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        match target {
            ActionTarget::Id(id) => {
                let actionability = self.legacy_click_actionability(*id, true);
                self.click_element_pointer_only(*id, None, bound.deadline)
                    .map_err(|source| {
                        action_source_error(source, None, None, actionability, bound, true)
                    })?;
                Ok(self.legacy_click_outcome(
                    *id,
                    ActionMethod::Pointer { native_fallback },
                    true,
                    bound,
                ))
            }
            ActionTarget::Semantic(target) => {
                let resolved = self.resolve_stable_pointer_target(
                    target,
                    max_nodes,
                    sequence_deadline,
                    bound,
                )?;
                self.dispatch_pointer_click(resolved, native_fallback)
            }
        }
    }

    pub fn click_target(
        &mut self,
        params: &ClickTargetParams,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        self.click_target_by(params, Deadline::UNBOUNDED)
    }

    pub fn click_target_by(
        &mut self,
        params: &ClickTargetParams,
        sequence_deadline: Deadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let started = std::time::Instant::now();
        let result = self.click_target_inner(params.clone(), sequence_deadline);
        let element = match &result {
            Ok(outcome) => crate::audit::ElementRef {
                id: outcome.target.id.0,
                role: Some(format!("{:?}", outcome.target.role)),
                name: outcome.target.name.clone(),
            },
            Err(_) => match &params.target {
                ActionTarget::Id(id) => self.element_ref(*id),
                ActionTarget::Semantic(target) => crate::audit::ElementRef {
                    id: 0,
                    role: target.target.role().map(|role| format!("{role:?}")),
                    name: target.target.query().map(str::to_owned),
                },
            },
        };
        let (method, native_fallback, actuated_id, dispatch, confirmation) = match &result {
            Ok(outcome) => {
                let (method, native_fallback, actuated_id) = match &outcome.action.method {
                    ActionMethod::NativeAction { actuated } => {
                        (Some("native-action"), None, actuated.map(|id| id.0))
                    }
                    ActionMethod::Pointer { native_fallback } => {
                        (Some("pointer"), native_fallback.as_deref(), None)
                    }
                    ActionMethod::AccessibilityValue => (Some("accessibility-value"), None, None),
                    ActionMethod::Keyboard => (Some("keyboard"), None, None),
                };
                (
                    method,
                    native_fallback,
                    actuated_id,
                    outcome.action.dispatch.as_str(),
                    outcome.action.confirmation.as_str(),
                )
            }
            Err(error) => (
                None,
                None,
                None,
                error.action_dispatch.as_str(),
                ConfirmationStatus::Unconfirmed.as_str(),
            ),
        };
        self.emit_audit(
            &crate::audit::Actuation::ClickElement {
                element,
                mode: params.mode.as_str(),
                method,
                native_fallback,
                actuated_id,
                dispatch,
                confirmation,
            },
            crate::audit::AuditOutcome::from_result(&result),
            started.elapsed(),
        );
        result
    }

    pub(super) fn click_target_inner(
        &mut self,
        params: ClickTargetParams,
        sequence_deadline: Deadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let bound = target_deadline(
            &params.target,
            params.timeout_ms,
            params.max_nodes,
            sequence_deadline,
        )?;
        match params.mode {
            ActionMode::Native => {
                self.click_native_once(&params.target, params.max_nodes, sequence_deadline, bound)
            }
            ActionMode::Pointer => self.click_pointer_once(
                &params.target,
                params.max_nodes,
                sequence_deadline,
                bound,
                None,
            ),
            ActionMode::Auto => match self.click_native_once(
                &params.target,
                params.max_nodes,
                sequence_deadline,
                bound,
            ) {
                Ok(done) => Ok(done),
                Err(error) if error.proves_pre_dispatch_native_unavailable() => {
                    let reason = error.safe_fallback_reason();
                    self.click_pointer_once(
                        &params.target,
                        params.max_nodes,
                        sequence_deadline,
                        bound,
                        Some(reason),
                    )
                }
                Err(error) => Err(error),
            },
        }
    }

    fn legacy_set_value_snapshot(
        &self,
        id: AxNodeId,
    ) -> Option<(ElementInfo, AxStateCoverage, (u32, u32))> {
        let active = self.active.as_ref()?;
        let node = active.last_ax.as_ref()?.find(id)?;
        let coverage = active
            .accessibility
            .as_ref()
            .map_or(AxStateCoverage::NONE, |reader| reader.state_coverage());
        Some((
            ElementInfo::from_node(node),
            coverage,
            (active.geometry.width, active.geometry.height),
        ))
    }

    fn set_value_once(
        &mut self,
        params: &SetValueTargetParams,
        text: &str,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let (element, resolution, coverage, window, legacy_id, execution) = match &params.target {
            ActionTarget::Id(id) => {
                let before = self.legacy_set_value_snapshot(*id);
                let actionability = before
                    .as_ref()
                    .map(|(element, coverage, window)| {
                        set_value_actionability(element, *coverage, *window, true)
                    })
                    .unwrap_or_default();
                let execution =
                    self.set_value_inner(*id, text, bound.deadline)
                        .map_err(|source| {
                            set_value_source_error(source, None, None, actionability, bound)
                        })?;
                let (element, coverage, window) = self
                    .legacy_set_value_snapshot(*id)
                    .or(before)
                    .expect("a successful ID set-value retained its cached target");
                (element, None, coverage, window, true, execution)
            }
            ActionTarget::Semantic(target) => {
                let (window, coverage) = self
                    .active
                    .as_ref()
                    .map(|active| {
                        (
                            (active.geometry.width, active.geometry.height),
                            active
                                .accessibility
                                .as_ref()
                                .map_or(AxStateCoverage::NONE, |reader| reader.state_coverage()),
                        )
                    })
                    .unwrap_or_default();
                let resolved = self
                    .resolve_semantic_target_by_bound(
                        target,
                        params.max_nodes,
                        sequence_deadline,
                        bound,
                        |element, coverage| {
                            role_supports_set_value(element, coverage)
                                && set_value_actionability(element, coverage, window, false)
                                    .blocking()
                                    .is_none()
                        },
                    )
                    .map_err(|mut error| {
                        if error.kind == SemanticActionFailureKind::NotActionable {
                            let element = error
                                .candidates
                                .first()
                                .map(|candidate| candidate.element.clone());
                            if let Some(element) = element {
                                error.actionability =
                                    set_value_actionability(&element, coverage, window, false);
                            }
                        }
                        error
                    })?;
                let actionability =
                    set_value_actionability(&resolved.element, resolved.coverage, window, false);
                let execution = self
                    .set_value_inner(resolved.element.id, text, resolved.bound.deadline)
                    .map_err(|source| {
                        set_value_source_error(
                            source,
                            Some(resolved.element.clone()),
                            Some(resolved.resolution.clone()),
                            actionability,
                            resolved.bound,
                        )
                    })?;
                (
                    resolved.element,
                    Some(resolved.resolution),
                    resolved.coverage,
                    window,
                    false,
                    execution,
                )
            }
        };
        let mut actionability = set_value_actionability(&element, coverage, window, legacy_id);
        actionability.pass_backend_fingerprint();
        Ok(SemanticActionOutcome {
            target: element,
            resolution,
            actionability,
            focus: None,
            action: MutationReport {
                method: ActionMethod::AccessibilityValue,
                dispatch: match execution {
                    SetValueExecution::AlreadyApplied => DispatchStatus::NotDispatched,
                    SetValueExecution::DispatchedAndConfirmed => DispatchStatus::Dispatched,
                },
                confirmation: ConfirmationStatus::ValueConfirmed,
            },
            bound,
        })
    }

    pub fn set_value_target(
        &mut self,
        params: &SetValueTargetParams,
        text: &str,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        self.set_value_target_by(params, text, Deadline::UNBOUNDED)
    }

    pub fn set_value_target_by(
        &mut self,
        params: &SetValueTargetParams,
        text: &str,
        sequence_deadline: Deadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let started = std::time::Instant::now();
        let result = match target_deadline(
            &params.target,
            params.timeout_ms,
            params.max_nodes,
            sequence_deadline,
        ) {
            Ok(bound) => self.set_value_once(params, text, sequence_deadline, bound),
            Err(error) => Err(error),
        };
        let element = match &result {
            Ok(outcome) => crate::audit::ElementRef {
                id: outcome.target.id.0,
                role: Some(format!("{:?}", outcome.target.role)),
                name: outcome.target.name.clone(),
            },
            Err(_) => match &params.target {
                ActionTarget::Id(id) => self.element_ref(*id),
                ActionTarget::Semantic(target) => crate::audit::ElementRef {
                    id: 0,
                    role: target.target.role().map(|role| format!("{role:?}")),
                    name: target.target.query().map(str::to_owned),
                },
            },
        };
        let (dispatch, confirmation) = match &result {
            Ok(outcome) => (
                outcome.action.dispatch.as_str(),
                outcome.action.confirmation.as_str(),
            ),
            Err(error) => (
                error.action_dispatch.as_str(),
                ConfirmationStatus::Unconfirmed.as_str(),
            ),
        };
        self.emit_audit(
            &crate::audit::Actuation::SetValue {
                element,
                text,
                dispatch,
                confirmation,
            },
            crate::audit::AuditOutcome::from_result(&result),
            started.elapsed(),
        );
        result
    }

    fn confirm_focused_target(
        &mut self,
        target: &AxTarget,
        deadline: ActionDeadline,
    ) -> SemanticActionResult<ElementInfo> {
        let coverage = self
            .active
            .as_ref()
            .and_then(|active| active.accessibility.as_ref())
            .map_or(AxStateCoverage::NONE, |reader| reader.state_coverage());
        let observe = |tree: &crate::AxTree| {
            if !coverage.focused {
                return None;
            }
            match target.relocate(tree) {
                crate::accessibility::Located::AtId(node)
                | crate::accessibility::Located::Moved(node)
                    if node.states.focused =>
                {
                    Some(ElementInfo::from_node(node))
                }
                _ => None,
            }
        };
        if !deadline.allow_wait {
            let tree = self
                .a11y_resnapshot_for_wait(deadline.deadline)
                .map_err(|source| source_error(source, deadline))?;
            return observe(&tree).ok_or_else(|| {
                empty_error(
                    SemanticActionFailureKind::FocusUnconfirmed,
                    "semantic target focus could not be confirmed",
                    deadline,
                    RetryGuidance::Reobserve,
                    None,
                )
            });
        }
        let poll = self
            .poll_accessibility_until_by_deadline(
                a11y_poll::A11yPollCadence {
                    interval_ms: SEMANTIC_ACTION_STABILITY_MS,
                    reread_after: std::time::Duration::from_secs(1),
                },
                a11y_poll::A11yPollBound {
                    action_deadline: deadline.deadline,
                    whose: deadline.owner.unwrap_or(Whose::Callee),
                    allow_wait: true,
                    sequence_deadline: Deadline::UNBOUNDED,
                },
                "confirm semantic target focus",
                observe,
                Option::is_some,
            )
            .map_err(|source| source_error(source, deadline))?;
        if poll.satisfied {
            Ok(poll
                .observation
                .expect("a satisfied focus confirmation observed a focused target"))
        } else {
            Err(empty_error(
                SemanticActionFailureKind::FocusUnconfirmed,
                "semantic target focus could not be confirmed",
                deadline,
                RetryGuidance::Reobserve,
                None,
            ))
        }
    }

    fn native_focus_confirmation_target(
        &self,
        original: &AxTarget,
        actuated: Option<AxNodeId>,
    ) -> Result<AxTarget> {
        let Some(id) = actuated else {
            return Ok(original.clone());
        };
        let active = self.require_active()?;
        let tree = active.last_ax.as_ref().ok_or(GlassError::NoAxSnapshot)?;
        let node = tree.find(id).ok_or(GlassError::AxElementNotFound(id.0))?;
        Ok(AxTarget {
            id,
            role: node.role,
            name: node.name.clone(),
            bounds: node.bounds,
            value: node.value.clone(),
        })
    }

    fn focus_target_native_once(
        &mut self,
        params: &TypeTargetParams,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
    ) -> SemanticActionResult<ConfirmedFocus> {
        let window = self
            .active
            .as_ref()
            .map(|active| (active.geometry.width, active.geometry.height))
            .unwrap_or_default();
        let coverage = self
            .active
            .as_ref()
            .and_then(|active| active.accessibility.as_ref())
            .map_or(AxStateCoverage::NONE, |reader| reader.state_coverage());
        let resolved = self
            .resolve_semantic_target_by_bound(
                &params.target,
                params.max_nodes,
                sequence_deadline,
                bound,
                |element, coverage| {
                    targeted_type_eligibility(element, coverage) == ActionabilityVerdict::Passed
                        && targeted_type_actionability(element, coverage, None, window, false)
                            .blocking()
                            .is_none()
                },
            )
            .map_err(|mut error| {
                if error.kind == SemanticActionFailureKind::NotActionable
                    && let Some(element) = error
                        .candidates
                        .first()
                        .map(|candidate| candidate.element.clone())
                {
                    error.actionability =
                        targeted_type_actionability(&element, coverage, None, window, false);
                }
                error
            })?;
        let mut actionability =
            targeted_type_actionability(&resolved.element, resolved.coverage, None, window, false);
        let actuated = self
            .try_native_focus(resolved.element.id, resolved.bound.deadline)
            .map_err(|source| {
                focus_source_error(
                    source,
                    resolved.element.clone(),
                    Some(resolved.resolution.clone()),
                    actionability.clone(),
                    ActionMethod::NativeAction { actuated: None },
                    resolved.bound,
                    true,
                )
            })?;
        actionability.pass_backend_fingerprint();
        let method = ActionMethod::NativeAction { actuated };
        let confirmation_target = self
            .native_focus_confirmation_target(&resolved.target, actuated)
            .map_err(|source| {
                focus_unconfirmed_error(
                    Some(source),
                    resolved.element.clone(),
                    resolved.resolution.clone(),
                    actionability.clone(),
                    resolved.coverage,
                    method.clone(),
                    resolved.bound,
                )
            })?;
        let confirmed = self
            .confirm_focused_target(&confirmation_target, resolved.bound)
            .map_err(|error| {
                focus_unconfirmed_error(
                    error.source,
                    resolved.element.clone(),
                    resolved.resolution.clone(),
                    actionability.clone(),
                    resolved.coverage,
                    method.clone(),
                    resolved.bound,
                )
            })?;
        record_focus_confirmation(&mut actionability, resolved.coverage, true);
        Ok(ConfirmedFocus {
            element: confirmed,
            resolution: resolved.resolution,
            actionability,
            focus: MutationReport {
                method,
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::FocusConfirmed,
            },
            bound: resolved.bound,
        })
    }

    fn focus_target_pointer_once(
        &mut self,
        params: &TypeTargetParams,
        sequence_deadline: Deadline,
        bound: ActionDeadline,
        native_fallback: Option<String>,
    ) -> SemanticActionResult<ConfirmedFocus> {
        let resolved = self.resolve_stable_pointer_target(
            &params.target,
            params.max_nodes,
            sequence_deadline,
            bound,
        )?;
        let mut pre_dispatch = targeted_type_actionability(
            &resolved.candidate.element,
            resolved.coverage,
            Some(true),
            resolved.candidate.window,
            true,
        );
        if pre_dispatch.blocking().is_some() {
            return Err(actionability_error(
                FailureSummary {
                    kind: SemanticActionFailureKind::NotActionable,
                    summary: "semantic target is not eligible for targeted typing",
                },
                Some(resolved.resolution),
                pre_dispatch,
                resolved.bound,
                RetryGuidance::Reobserve,
                None,
                DispatchStatus::NotDispatched,
            )
            .with_target(resolved.candidate.element.clone()));
        }
        let target = resolved.candidate.target.clone();
        let element = resolved.candidate.element.clone();
        let coverage = resolved.coverage;
        let method = ActionMethod::Pointer {
            native_fallback: native_fallback.clone(),
        };
        let focused = self
            .dispatch_pointer_click(resolved, native_fallback)
            .map_err(|mut error| {
                let focus_dispatch = error.action_dispatch;
                insert_targeted_type_eligibility(&mut error.actionability, &element, coverage);
                error.focus = Some(MutationReport {
                    method: method.clone(),
                    dispatch: focus_dispatch,
                    confirmation: ConfirmationStatus::Unconfirmed,
                });
                error.action_dispatch = DispatchStatus::NotDispatched;
                if focus_dispatch != DispatchStatus::NotDispatched {
                    error.retry = RetryGuidance::DoNotRetry;
                }
                error
            })?;
        pre_dispatch = focused.actionability;
        insert_targeted_type_eligibility(&mut pre_dispatch, &focused.target, coverage);
        let confirmed = self
            .confirm_focused_target(&target, focused.bound)
            .map_err(|error| {
                focus_unconfirmed_error(
                    error.source,
                    focused.target.clone(),
                    focused
                        .resolution
                        .clone()
                        .expect("semantic pointer focus has a resolution report"),
                    pre_dispatch.clone(),
                    coverage,
                    method.clone(),
                    focused.bound,
                )
            })?;
        record_focus_confirmation(&mut pre_dispatch, coverage, true);
        Ok(ConfirmedFocus {
            element: confirmed,
            resolution: focused
                .resolution
                .expect("semantic pointer focus has a resolution report"),
            actionability: pre_dispatch,
            focus: MutationReport {
                method,
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::FocusConfirmed,
            },
            bound: focused.bound,
        })
    }

    fn type_target_inner(
        &mut self,
        params: &TypeTargetParams,
        text: &str,
        sequence_deadline: Deadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let bound = target_deadline(
            &ActionTarget::Semantic(params.target.clone()),
            Some(params.timeout_ms),
            params.max_nodes,
            sequence_deadline,
        )?;
        let focused = match params.focus_mode {
            ActionMode::Native => self.focus_target_native_once(params, sequence_deadline, bound),
            ActionMode::Pointer => {
                self.focus_target_pointer_once(params, sequence_deadline, bound, None)
            }
            ActionMode::Auto => {
                match self.focus_target_native_once(params, sequence_deadline, bound) {
                    Ok(focused) => Ok(focused),
                    Err(error) if error.proves_pre_dispatch_native_unavailable() => {
                        let reason = error.safe_fallback_reason();
                        self.focus_target_pointer_once(
                            params,
                            sequence_deadline,
                            bound,
                            Some(reason),
                        )
                    }
                    Err(error) => Err(error),
                }
            }
        }?;
        if let Err(source) =
            self.key_inner_by(&KeyEvent::Text(text.to_owned()), focused.bound.deadline)
        {
            return Err(key_source_error(source, focused));
        }
        Ok(SemanticActionOutcome {
            target: focused.element,
            resolution: Some(focused.resolution),
            actionability: focused.actionability,
            focus: Some(focused.focus),
            action: MutationReport {
                method: ActionMethod::Keyboard,
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::DispatchConfirmed,
            },
            bound: focused.bound,
        })
    }

    pub fn type_target(
        &mut self,
        params: &TypeTargetParams,
        text: &str,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        self.type_target_by(params, text, Deadline::UNBOUNDED)
    }

    pub fn type_target_by(
        &mut self,
        params: &TypeTargetParams,
        text: &str,
        sequence_deadline: Deadline,
    ) -> SemanticActionResult<SemanticActionOutcome> {
        let started = std::time::Instant::now();
        let result = self.type_target_inner(params, text, sequence_deadline);
        let element = match &result {
            Ok(outcome) => crate::audit::ElementRef {
                id: outcome.target.id.0,
                role: Some(format!("{:?}", outcome.target.role)),
                name: outcome.target.name.clone(),
            },
            Err(error) => error
                .candidates
                .first()
                .map(|candidate| crate::audit::ElementRef {
                    id: candidate.element.id.0,
                    role: Some(format!("{:?}", candidate.element.role)),
                    name: candidate.element.name.clone(),
                })
                .unwrap_or_else(|| crate::audit::ElementRef {
                    id: 0,
                    role: params.target.target.role().map(|role| format!("{role:?}")),
                    name: params.target.target.query().map(str::to_owned),
                }),
        };
        let focus = match &result {
            Ok(outcome) => outcome.focus.as_ref(),
            Err(error) => error.focus.as_ref(),
        };
        let focus_method = focus.map(|report| report.method.as_str());
        let focus_dispatch = focus.map_or(DispatchStatus::NotDispatched, |report| report.dispatch);
        let focus_confirmation = focus.map_or(ConfirmationStatus::Unconfirmed, |report| {
            report.confirmation
        });
        let type_dispatch = match &result {
            Ok(outcome) => outcome.action.dispatch,
            Err(error) => error.action_dispatch,
        };
        self.emit_audit(
            &crate::audit::Actuation::TypeTarget {
                element,
                text,
                focus_mode: params.focus_mode.as_str(),
                focus_method,
                focus_dispatch: focus_dispatch.as_str(),
                focus_confirmation: focus_confirmation.as_str(),
                type_dispatch: type_dispatch.as_str(),
            },
            crate::audit::AuditOutcome::from_result(&result),
            started.elapsed(),
        );
        result
    }
}

#[cfg(test)]
mod tests;
