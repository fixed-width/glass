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

impl Glass {
    pub(super) fn resolve_semantic_target(
        &mut self,
        target: &SemanticTarget,
        max_nodes: Option<usize>,
        timeout_ms: u64,
        sequence_deadline: Deadline,
        eligibility: impl Fn(&ElementInfo, AxStateCoverage) -> bool,
    ) -> std::result::Result<ResolvedSemanticTarget, SemanticActionError> {
        let coverage = {
            let active = self.active_mut().map_err(|source| {
                source_error(
                    source,
                    ActionDeadline {
                        deadline: sequence_deadline,
                        owner: sequence_deadline.instant().map(|_| Whose::Caller),
                        allow_wait: timeout_ms > 0,
                    },
                )
            })?;
            active
                .accessibility
                .as_ref()
                .ok_or_else(|| {
                    source_error(
                        GlassError::AxUnsupported,
                        ActionDeadline {
                            deadline: sequence_deadline,
                            owner: sequence_deadline.instant().map(|_| Whose::Caller),
                            allow_wait: timeout_ms > 0,
                        },
                    )
                })?
                .state_coverage()
        };
        let bound = target_deadline(
            &ActionTarget::Semantic(target.clone()),
            Some(timeout_ms),
            max_nodes,
            sequence_deadline,
        )?;
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
            .poll_accessibility_until(
                SEMANTIC_ACTION_STABILITY_MS,
                timeout_ms,
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
}

#[cfg(test)]
mod tests;
