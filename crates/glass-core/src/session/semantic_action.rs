#![allow(dead_code)]

use super::*;
use crate::{
    ActionabilityReport, AxStateCoverage, ScopeResolution, SemanticMatch, SemanticQuery,
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
    pub bound: ActionDeadline,
    pub retry: RetryGuidance,
    pub source: Option<GlassError>,
}

impl std::fmt::Display for SemanticActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.summary)
    }
}

impl std::error::Error for SemanticActionError {}

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

#[derive(Clone, Debug)]
struct PointerCandidate {
    element: ElementInfo,
    target: AxTarget,
    plan: PlannedPointerInput,
}

#[derive(Debug)]
struct PointerResolutionObservation {
    resolution: ResolutionObservation,
    candidate: Option<PointerCandidate>,
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
) -> SemanticActionError {
    SemanticActionError {
        kind,
        summary,
        resolution: None,
        actionability: ActionabilityReport::default(),
        focus: None,
        action_dispatch: DispatchStatus::NotDispatched,
        candidates: Vec::new(),
        bound,
        retry,
        source,
    }
}

fn request_error(summary: &'static str, sequence_deadline: Deadline) -> SemanticActionError {
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
) -> std::result::Result<ActionDeadline, SemanticActionError> {
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
) -> SemanticActionError {
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
    SemanticActionError {
        kind,
        summary,
        resolution: Some(report),
        actionability: ActionabilityReport::default(),
        focus: None,
        action_dispatch: DispatchStatus::NotDispatched,
        candidates: observation.result.matches,
        bound,
        retry,
        source: None,
    }
}

fn source_error(source: GlassError, bound: ActionDeadline) -> SemanticActionError {
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
    if element.states.checkable
        && trailing_toggle_backend
        && bounds.width > bounds.height.saturating_mul(ROW_ASPECT)
    {
        let segment = bounds.trailing_toggle_swipe(window.0, window.1)?;
        let probe_point = (
            (segment.from_x + segment.to_x) / 2,
            (segment.from_y + segment.to_y) / 2,
        );
        Some(PlannedPointerInput::TrailingToggle {
            segment,
            probe_point,
        })
    } else {
        Some(PlannedPointerInput::Click {
            point: bounds.clamped_center(window.0, window.1)?,
        })
    }
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

fn actionability_error(
    kind: SemanticActionFailureKind,
    summary: &'static str,
    resolution: Option<ResolutionReport>,
    actionability: ActionabilityReport,
    bound: ActionDeadline,
    retry: RetryGuidance,
    source: Option<GlassError>,
    dispatch: DispatchStatus,
) -> SemanticActionError {
    SemanticActionError {
        kind,
        summary,
        resolution,
        actionability,
        focus: None,
        action_dispatch: dispatch,
        candidates: Vec::new(),
        bound,
        retry,
        source,
    }
}

fn action_source_error(
    source: GlassError,
    resolution: Option<ResolutionReport>,
    actionability: ActionabilityReport,
    bound: ActionDeadline,
    dispatch_started: bool,
) -> SemanticActionError {
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

fn native_fallback_reason(source: &GlassError) -> String {
    match source {
        GlassError::AxUnsupported => "native accessibility action is unsupported".into(),
        GlassError::AxActionUnavailable(_) => {
            "target exposes no native accessibility action".into()
        }
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
    ) -> std::result::Result<ResolvedSemanticTarget, SemanticActionError> {
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
    ) -> std::result::Result<ResolvedSemanticTarget, SemanticActionError> {
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
                SEMANTIC_ACTION_STABILITY_MS,
                std::time::Duration::from_secs(1),
                bound.deadline,
                bound.owner.unwrap_or(Whose::Callee),
                bound.allow_wait,
                sequence_deadline,
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
    ) -> std::result::Result<ResolvedPointerTarget, SemanticActionError> {
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
        let (window, trailing_toggle_backend) = {
            let active = self
                .active
                .as_ref()
                .ok_or_else(|| source_error(GlassError::NoActiveSession, bound))?;
            (
                (active.geometry.width, active.geometry.height),
                active.platform.a11y_toggle_control_at_trailing_edge(),
            )
        };
        let mut sample: Option<StabilitySample> = None;
        let poll = self
            .poll_accessibility_until_by_deadline(
                SEMANTIC_ACTION_STABILITY_MS,
                std::time::Duration::from_millis(SEMANTIC_ACTION_STABILITY_MS),
                bound.deadline,
                bound.owner.unwrap_or(Whose::Callee),
                bound.allow_wait,
                sequence_deadline,
                "stabilize semantic pointer target",
                |tree| {
                    let result = tree.semantic_query(&query);
                    let complete_unique = matches!(
                        result.scope,
                        ScopeResolution::Unscoped | ScopeResolution::Resolved(_)
                    ) && result.matches_in_walk == 1
                        && result.search_complete
                        && result.matches.len() == 1;
                    let candidate = complete_unique
                        .then(|| {
                            let element = result.matches[0].element.clone();
                            let plan = pointer_plan(&element, window, trailing_toggle_backend)?;
                            let report = ActionabilityReport::evaluate_click(
                                &element,
                                coverage,
                                Some(true),
                                window,
                                crate::PointerHit::Inconclusive,
                                false,
                                true,
                            );
                            report.blocking().is_none().then(|| PointerCandidate {
                                target: ax_target(&element),
                                element,
                                plan,
                            })
                        })
                        .flatten();
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
                    } else {
                        sample = None;
                    }
                    PointerResolutionObservation {
                        resolution: ResolutionObservation {
                            result,
                            eligible: candidate.is_some(),
                        },
                        candidate,
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
            if let Some(candidate) = poll.observation.candidate {
                let actionability = ActionabilityReport::evaluate_click(
                    &candidate.element,
                    coverage,
                    Some(false),
                    window,
                    crate::PointerHit::Inconclusive,
                    false,
                    true,
                );
                return Err(actionability_error(
                    SemanticActionFailureKind::UnstableTarget,
                    "semantic pointer target did not remain stable",
                    Some(report),
                    actionability,
                    bound,
                    RetryGuidance::WaitOrRefine,
                    None,
                    DispatchStatus::NotDispatched,
                ));
            }
            return Err(classified_resolution_error(
                poll.observation.resolution,
                report,
                bound,
            ));
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

    fn invoke_semantic_target(
        &mut self,
        target: &AxTarget,
        deadline: Deadline,
    ) -> Result<Option<AxNodeId>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "native accessibility action",
            ));
        }
        let ctx = self.accessibility_context_for_action(deadline)?;
        let active = self.active_mut()?;
        let actuated = active
            .accessibility
            .as_mut()
            .ok_or(GlassError::AxUnsupported)?
            .invoke(&ctx, target)?;
        active.pump();
        Ok(actuated)
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
    ) -> std::result::Result<SemanticActionOutcome, SemanticActionError> {
        let window = {
            let active = self
                .active
                .as_ref()
                .ok_or_else(|| source_error(GlassError::NoActiveSession, resolved.bound))?;
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
                SemanticActionFailureKind::NotActionable,
                "semantic target is not actionable",
                Some(resolved.resolution),
                actionability,
                resolved.bound,
                RetryGuidance::Reobserve,
                None,
                DispatchStatus::NotDispatched,
            ));
        }
        let actuated = self
            .invoke_semantic_target(&resolved.target, resolved.bound.deadline)
            .map_err(|source| {
                action_source_error(
                    source,
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
                confirmation: ConfirmationStatus::NotRequested,
            },
            bound: resolved.bound,
        })
    }

    fn dispatch_pointer_click(
        &mut self,
        resolved: ResolvedPointerTarget,
        native_fallback: Option<String>,
    ) -> std::result::Result<SemanticActionOutcome, SemanticActionError> {
        let window = {
            let active = self
                .active
                .as_ref()
                .ok_or_else(|| source_error(GlassError::NoActiveSession, resolved.bound))?;
            (active.geometry.width, active.geometry.height)
        };
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
                    Some(resolved.resolution.clone()),
                    ActionabilityReport::evaluate_click(
                        &resolved.candidate.element,
                        resolved.coverage,
                        Some(true),
                        window,
                        crate::PointerHit::Inconclusive,
                        false,
                        true,
                    ),
                    resolved.bound,
                    false,
                )
            })?;
        let actionability = ActionabilityReport::evaluate_click(
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
                SemanticActionFailureKind::NotActionable,
                "semantic pointer target is not actionable",
                Some(resolved.resolution),
                actionability,
                resolved.bound,
                RetryGuidance::Reobserve,
                None,
                DispatchStatus::NotDispatched,
            ));
        }
        self.click_element_pointer_only(
            resolved.candidate.element.id,
            Some(&resolved.candidate.plan),
            resolved.bound.deadline,
        )
        .map_err(|source| {
            action_source_error(
                source,
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
                confirmation: ConfirmationStatus::NotRequested,
            },
            bound: resolved.bound,
        })
    }

    fn legacy_click_target(
        &mut self,
        id: AxNodeId,
        mode: ActionMode,
        bound: ActionDeadline,
    ) -> std::result::Result<SemanticActionOutcome, SemanticActionError> {
        let (element, target, coverage, window) = {
            let active = self
                .active
                .as_ref()
                .ok_or_else(|| source_error(GlassError::NoActiveSession, bound))?;
            let tree = active
                .last_ax
                .as_ref()
                .ok_or_else(|| source_error(GlassError::NoAxSnapshot, bound))?;
            let node = tree
                .find(id)
                .ok_or_else(|| source_error(GlassError::AxElementNotFound(id.0), bound))?;
            let element = ElementInfo::from_node(node);
            let coverage = active
                .accessibility
                .as_ref()
                .map_or(AxStateCoverage::NONE, |reader| reader.state_coverage());
            (
                element.clone(),
                ax_target(&element),
                coverage,
                (active.geometry.width, active.geometry.height),
            )
        };
        let (method, pointer) = match mode {
            ActionMode::Native => {
                let actuated = self
                    .invoke_semantic_target(&target, bound.deadline)
                    .map_err(|source| {
                        action_source_error(
                            source,
                            None,
                            ActionabilityReport::evaluate_click(
                                &element,
                                coverage,
                                None,
                                window,
                                crate::PointerHit::Inconclusive,
                                true,
                                false,
                            ),
                            bound,
                            true,
                        )
                    })?;
                (ActionMethod::NativeAction { actuated }, false)
            }
            ActionMode::Pointer => {
                self.click_element_pointer_only(id, None, bound.deadline)
                    .map_err(|source| {
                        action_source_error(
                            source,
                            None,
                            ActionabilityReport::evaluate_click(
                                &element,
                                coverage,
                                None,
                                window,
                                crate::PointerHit::Inconclusive,
                                true,
                                true,
                            ),
                            bound,
                            true,
                        )
                    })?;
                (
                    ActionMethod::Pointer {
                        native_fallback: None,
                    },
                    true,
                )
            }
            ActionMode::Auto => {
                let method = self
                    .click_element_by(id, bound.deadline)
                    .map_err(|source| {
                        action_source_error(
                            source,
                            None,
                            ActionabilityReport::evaluate_click(
                                &element,
                                coverage,
                                None,
                                window,
                                crate::PointerHit::Inconclusive,
                                true,
                                false,
                            ),
                            bound,
                            true,
                        )
                    })?;
                match method {
                    ClickMethod::NativeAction { actuated } => {
                        (ActionMethod::NativeAction { actuated }, false)
                    }
                    ClickMethod::Pointer { native_fallback } => (
                        ActionMethod::Pointer {
                            native_fallback: Some(native_fallback),
                        },
                        true,
                    ),
                }
            }
        };
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
        Ok(SemanticActionOutcome {
            target: element,
            resolution: None,
            actionability,
            focus: None,
            action: MutationReport {
                method,
                dispatch: DispatchStatus::Dispatched,
                confirmation: ConfirmationStatus::NotRequested,
            },
            bound,
        })
    }

    pub(super) fn click_target_inner(
        &mut self,
        params: ClickTargetParams,
        sequence_deadline: Deadline,
    ) -> std::result::Result<SemanticActionOutcome, SemanticActionError> {
        let bound = target_deadline(
            &params.target,
            params.timeout_ms,
            params.max_nodes,
            sequence_deadline,
        )?;
        match params.target {
            ActionTarget::Id(id) => self.legacy_click_target(id, params.mode, bound),
            ActionTarget::Semantic(target) => match params.mode {
                ActionMode::Native => {
                    let window = self
                        .active
                        .as_ref()
                        .map(|active| (active.geometry.width, active.geometry.height))
                        .unwrap_or_default();
                    let resolved = self.resolve_semantic_target_by_bound(
                        &target,
                        params.max_nodes,
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
                ActionMode::Pointer => {
                    let resolved = self.resolve_stable_pointer_target(
                        &target,
                        params.max_nodes,
                        sequence_deadline,
                        bound,
                    )?;
                    self.dispatch_pointer_click(resolved, None)
                }
                ActionMode::Auto => {
                    let window = self
                        .active
                        .as_ref()
                        .map(|active| (active.geometry.width, active.geometry.height))
                        .unwrap_or_default();
                    let resolved = self.resolve_semantic_target_by_bound(
                        &target,
                        params.max_nodes,
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
                    match self.invoke_semantic_target(&resolved.target, bound.deadline) {
                        Ok(actuated) => {
                            let mut actionability = ActionabilityReport::evaluate_click(
                                &resolved.element,
                                resolved.coverage,
                                None,
                                window,
                                crate::PointerHit::Inconclusive,
                                false,
                                false,
                            );
                            actionability.pass_backend_fingerprint();
                            Ok(SemanticActionOutcome {
                                target: resolved.element,
                                resolution: Some(resolved.resolution),
                                actionability,
                                focus: None,
                                action: MutationReport {
                                    method: ActionMethod::NativeAction { actuated },
                                    dispatch: DispatchStatus::Dispatched,
                                    confirmation: ConfirmationStatus::NotRequested,
                                },
                                bound,
                            })
                        }
                        Err(source) if source.invoke_fallback_eligible() => {
                            let reason = native_fallback_reason(&source);
                            let resolved = self.resolve_stable_pointer_target(
                                &target,
                                params.max_nodes,
                                sequence_deadline,
                                bound,
                            )?;
                            self.dispatch_pointer_click(resolved, Some(reason))
                        }
                        Err(source) => Err(action_source_error(
                            source,
                            Some(resolved.resolution),
                            ActionabilityReport::evaluate_click(
                                &resolved.element,
                                resolved.coverage,
                                None,
                                window,
                                crate::PointerHit::Inconclusive,
                                false,
                                false,
                            ),
                            bound,
                            true,
                        )),
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
