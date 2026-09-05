use glass_core::{
    ActionMethod, ActionMode, ActionTarget, ActionabilityReport, AxNodeId, ClickTargetParams,
    DispatchStatus, ElementInfo, MatchField, MatchTier, MutationReport, ResolutionReport,
    SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS, SEMANTIC_ACTION_MAX_TIMEOUT_MS, ScopeResolution,
    SemanticActionError, SemanticActionFailureKind, SemanticActionOutcome, SemanticMatch,
    SetValueTargetParams, TypeTargetParams, Whose,
};
use serde_json::{Value, json};

use crate::params::{Action, ActionModeArg, ClickElementArgs, SetValueArgs, TypeArgs};
use crate::tools::find::semantic_target;
use crate::tools::{
    ContextualError, OutContent, SafeErrorCategory, ToolOutput, validate_return,
    validate_settle_args,
};

pub(crate) enum ValidatedType {
    Untargeted,
    Targeted(TypeTargetParams),
}

impl From<ActionModeArg> for ActionMode {
    fn from(value: ActionModeArg) -> Self {
        match value {
            ActionModeArg::Auto => Self::Auto,
            ActionModeArg::Native => Self::Native,
            ActionModeArg::Pointer => Self::Pointer,
        }
    }
}

fn validate_return_arg(return_: Option<&str>) -> Result<(), ContextualError> {
    validate_return(return_)
        .map_err(|message| ContextualError::validation_with_code("invalid_return", message))
}

fn semantic_timeout(timeout_ms: Option<u64>) -> Result<u64, ContextualError> {
    let timeout_ms = timeout_ms.unwrap_or(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS);
    if timeout_ms > SEMANTIC_ACTION_MAX_TIMEOUT_MS {
        return Err(ContextualError::validation_with_code(
            "invalid_action_target",
            format!("`timeout_ms` must be between 0 and {SEMANTIC_ACTION_MAX_TIMEOUT_MS}"),
        ));
    }
    Ok(timeout_ms)
}

pub(crate) fn validate_click_element_args(
    a: &ClickElementArgs,
) -> Result<ClickTargetParams, ContextualError> {
    validate_return_arg(a.return_.as_deref())?;
    let mode = a.mode.map(Into::into).unwrap_or(ActionMode::Auto);
    let target = match (a.id, a.target.as_ref()) {
        (Some(id), None) => {
            if a.timeout_ms.is_some() {
                return Err(ContextualError::validation_with_code(
                    "invalid_action_target",
                    "`timeout_ms` is available only with `target`, not `id`".into(),
                ));
            }
            if a.max_nodes.is_some() {
                return Err(ContextualError::validation_with_code(
                    "invalid_action_target",
                    "`max_nodes` is available only with `target`, not `id`".into(),
                ));
            }
            return Ok(ClickTargetParams {
                target: ActionTarget::Id(AxNodeId(id)),
                mode,
                timeout_ms: None,
                max_nodes: None,
            });
        }
        (None, Some(target)) => {
            ActionTarget::Semantic(semantic_target(target).map_err(|message| {
                ContextualError::validation_with_code("invalid_action_target", message)
            })?)
        }
        _ => {
            return Err(ContextualError::validation_with_code(
                "invalid_action_target",
                "specify exactly one of `id` or `target`".into(),
            ));
        }
    };
    Ok(ClickTargetParams {
        target,
        mode,
        timeout_ms: Some(semantic_timeout(a.timeout_ms)?),
        max_nodes: a.max_nodes.map(|value| value as usize),
    })
}

pub(crate) fn validate_set_value_args(
    a: &SetValueArgs,
) -> Result<SetValueTargetParams, ContextualError> {
    validate_return_arg(a.return_.as_deref())?;
    let target = match (a.id, a.target.as_ref()) {
        (Some(id), None) => {
            if a.timeout_ms.is_some() {
                return Err(ContextualError::validation_with_code(
                    "invalid_action_target",
                    "`timeout_ms` is available only with `target`, not `id`".into(),
                ));
            }
            if a.max_nodes.is_some() {
                return Err(ContextualError::validation_with_code(
                    "invalid_action_target",
                    "`max_nodes` is available only with `target`, not `id`".into(),
                ));
            }
            return Ok(SetValueTargetParams {
                target: ActionTarget::Id(AxNodeId(id)),
                timeout_ms: None,
                max_nodes: None,
            });
        }
        (None, Some(target)) => {
            ActionTarget::Semantic(semantic_target(target).map_err(|message| {
                ContextualError::validation_with_code("invalid_action_target", message)
            })?)
        }
        _ => {
            return Err(ContextualError::validation_with_code(
                "invalid_action_target",
                "specify exactly one of `id` or `target`".into(),
            ));
        }
    };
    Ok(SetValueTargetParams {
        target,
        timeout_ms: Some(semantic_timeout(a.timeout_ms)?),
        max_nodes: a.max_nodes.map(|value| value as usize),
    })
}

pub(crate) fn validate_type_args(a: &TypeArgs) -> Result<ValidatedType, ContextualError> {
    validate_return_arg(a.return_.as_deref())?;
    let Some(target) = a.target.as_ref() else {
        if a.focus_mode.is_some() {
            return Err(ContextualError::validation_with_code(
                "invalid_action_target",
                "`focus_mode` requires a targeted type action".into(),
            ));
        }
        if a.timeout_ms.is_some() {
            return Err(ContextualError::validation_with_code(
                "invalid_action_target",
                "`timeout_ms` requires a targeted type action".into(),
            ));
        }
        if a.max_nodes.is_some() {
            return Err(ContextualError::validation_with_code(
                "invalid_action_target",
                "`max_nodes` requires a targeted type action".into(),
            ));
        }
        return Ok(ValidatedType::Untargeted);
    };
    Ok(ValidatedType::Targeted(TypeTargetParams {
        target: semantic_target(target).map_err(|message| {
            ContextualError::validation_with_code("invalid_action_target", message)
        })?,
        focus_mode: a.focus_mode.map(Into::into).unwrap_or(ActionMode::Auto),
        timeout_ms: semantic_timeout(a.timeout_ms)?,
        max_nodes: a.max_nodes.map(|value| value as usize),
    }))
}

pub(crate) fn validate_action(action: &Action) -> Result<(), ContextualError> {
    let result = match action {
        Action::Click(args) => super::input::validate_click_args(args).map(|_| ()),
        Action::Move(_) => Ok(()),
        Action::Drag(args) => super::input::validate_drag_args(args).map(|_| ()),
        Action::Scroll(args) => super::input::validate_scroll_args(args).map(|_| ()),
        Action::Type(args) => validate_type_args(args).map(|_| ()),
        Action::Key(args) => super::input::validate_key_args(args),
        Action::Settle(args) => validate_settle_args(args),
        Action::ClickElement(args) => validate_click_element_args(args).map(|_| ()),
        Action::SetValue(args) => validate_set_value_args(args).map(|_| ()),
        Action::WaitForElement(args) => {
            super::wait::validate_wait_for_element_args(args).map(|_| ())
        }
        Action::ScrollToElement(args) => {
            super::wait::validate_scroll_to_element_args(args).map(|_| ())
        }
    };
    result.map_err(|error| ContextualError::validation_with_code("invalid_sequence", error.message))
}

const _: fn(&Action) -> Result<(), ContextualError> = validate_action;

fn action_method(method: &ActionMethod) -> &'static str {
    match method {
        ActionMethod::NativeAction { .. } => "native-action",
        ActionMethod::Pointer { .. } => "pointer",
        ActionMethod::AccessibilityValue => "accessibility-value",
        ActionMethod::Keyboard => "keyboard",
    }
}

fn scope_json(scope: ScopeResolution) -> (&'static str, Option<Value>, Option<usize>) {
    match scope {
        ScopeResolution::Unscoped => ("unscoped", None, None),
        ScopeResolution::NotFound => ("not_found", None, None),
        ScopeResolution::Resolved(id) => ("resolved", Some(json!(id.0)), None),
        ScopeResolution::Ambiguous { observed } => ("ambiguous", None, Some(observed)),
    }
}

fn bounds_json(bounds: Option<glass_core::AxRect>) -> Value {
    bounds.map_or(Value::Null, |bounds| {
        json!({
            "x": bounds.x,
            "y": bounds.y,
            "width": bounds.width,
            "height": bounds.height,
        })
    })
}

fn match_field(field: MatchField) -> &'static str {
    match field {
        MatchField::Name => "name",
        MatchField::Description => "description",
        MatchField::Value => "value",
    }
}

fn match_tier(tier: MatchTier) -> &'static str {
    match tier {
        MatchTier::ExactName => "exact_name",
        MatchTier::NameSubstring => "name_substring",
        MatchTier::DescriptionSubstring => "description_substring",
        MatchTier::ValueSubstring => "value_substring",
        MatchTier::FilterOnly => "filter_only",
    }
}

pub(super) fn actionability_json(report: &ActionabilityReport) -> Value {
    Value::Array(
        report
            .checks
            .iter()
            .map(|check| {
                json!({
                    "check": check.name.as_str(),
                    "verdict": check.verdict.as_str(),
                    "required": check.required,
                    "source": check.source.as_str(),
                })
            })
            .collect(),
    )
}

pub(super) fn resolution_json(report: &ResolutionReport) -> Value {
    let (scope_status, resolved_scope_id, ambiguous_scope_matches) = scope_json(report.scope);
    let mut value = json!({
        "source": "semantic",
        "elapsed_ms": report.elapsed_ms,
        "scope_status": scope_status,
        "matches_in_walk": report.matches_in_walk,
        "search_complete": report.search_complete,
        "tree_truncated": report.tree_truncated,
        "unreadable_subtrees": report.unreadable_subtrees,
        "unexposed_placeholders": report.unexposed_placeholders,
    });
    if let Some(id) = resolved_scope_id {
        value["resolved_scope_id"] = id;
    }
    if let Some(matches) = ambiguous_scope_matches {
        value["ambiguous_scope_matches"] = json!(matches);
    }
    if let Some(owner) = report.timed_out_by {
        value["timed_out_by"] = json!(match owner {
            Whose::Callee => "action",
            Whose::Caller => "sequence",
        });
    }
    value
}

pub(super) fn mutation_json(report: &MutationReport) -> Value {
    let mut value = json!({
        "method": action_method(&report.method),
        "dispatch": report.dispatch.as_str(),
        "confirmation": report.confirmation.as_str(),
    });
    match &report.method {
        ActionMethod::NativeAction { actuated: Some(id) } => {
            value["actuated_id"] = json!(id.0);
        }
        ActionMethod::Pointer {
            native_fallback: Some(reason),
        } => {
            value["native_fallback"] = json!(reason);
        }
        ActionMethod::NativeAction { actuated: None }
        | ActionMethod::Pointer {
            native_fallback: None,
        }
        | ActionMethod::AccessibilityValue
        | ActionMethod::Keyboard => {}
    }
    value
}

pub(super) fn element_json(element: &ElementInfo, include_text: bool) -> Value {
    json!({
        "id": element.id.0,
        "role": format!("{:?}", element.role),
        "name": include_text.then(|| element.name.clone()).flatten(),
        "description": include_text.then(|| element.description.clone()).flatten(),
        "value": (include_text && !element.states.secure)
            .then(|| element.value.clone())
            .flatten(),
        "bounds": bounds_json(element.bounds),
        "states": element.states.active(),
    })
}

pub(super) fn candidates_json(candidates: &[SemanticMatch], include_text: bool) -> Value {
    Value::Array(
        candidates
            .iter()
            .map(|candidate| {
                let matched_text = include_text.then(|| match candidate.field {
                    Some(MatchField::Name) => candidate.element.name.clone(),
                    Some(MatchField::Description) => candidate.element.description.clone(),
                    Some(MatchField::Value) if !candidate.element.states.secure => {
                        candidate.element.value.clone()
                    }
                    Some(MatchField::Value) | None => None,
                });
                let mut value = element_json(&candidate.element, include_text);
                value["matched_field"] = candidate.field.map(match_field).into();
                value["matched_text"] = matched_text.flatten().into();
                value["match_tier"] = json!(match_tier(candidate.tier));
                value["context"] = include_text.then(|| candidate.context.clone()).into();
                value
            })
            .collect(),
    )
}

fn merge_object(target: &mut Value, source: Value) {
    let Some(target) = target.as_object_mut() else {
        return;
    };
    let Value::Object(source) = source else {
        return;
    };
    target.extend(source);
}

pub(crate) fn mutation_provenance(
    focus: Option<&MutationReport>,
    action_dispatch: DispatchStatus,
) -> (glass_core::BoundDispatch, bool) {
    let may_have_dispatched = action_dispatch != DispatchStatus::NotDispatched
        || focus.is_some_and(|report| report.dispatch != DispatchStatus::NotDispatched);
    (
        if may_have_dispatched {
            glass_core::BoundDispatch::MayHaveDispatched
        } else {
            glass_core::BoundDispatch::NotDispatched
        },
        may_have_dispatched,
    )
}

pub(crate) fn success_output(
    tool: &'static str,
    outcome: &SemanticActionOutcome,
    observed: Option<Value>,
    mut extra: Vec<OutContent>,
) -> ToolOutput {
    let mut result = json!({
        "id": outcome.target.id.0,
        "actionability": actionability_json(&outcome.actionability),
    });
    if let Some(focus) = &outcome.focus {
        result["focus_method"] = json!(action_method(&focus.method));
        result["focus_dispatch"] = json!(focus.dispatch.as_str());
        result["focus_confirmation"] = json!(focus.confirmation.as_str());
        result["type_dispatch"] = json!(outcome.action.dispatch.as_str());
    } else {
        merge_object(&mut result, mutation_json(&outcome.action));
    }
    if let Some(resolution) = &outcome.resolution {
        result["resolution"] = resolution_json(resolution);
        let include_text = tool != "glass_type";
        extra.insert(
            0,
            OutContent::untrusted_observation(
                &json!({ "target": element_json(&outcome.target, include_text) }).to_string(),
            ),
        );
        result["content_blocks"] = json!([1]);
    }
    if let Some(observed) = observed {
        result["observed"] = observed;
    }
    ToolOutput::result_with(tool, result, extra)
}

fn semantic_category(error: &SemanticActionError) -> SafeErrorCategory {
    let sequence_deadline = error
        .resolution
        .as_ref()
        .is_some_and(|report| report.timed_out_by == Some(Whose::Caller))
        || (error.bound.owner == Some(Whose::Caller) && error.bound.deadline.has_passed());
    if sequence_deadline {
        return SafeErrorCategory::SequenceDeadlineExceeded;
    }
    match error.kind {
        SemanticActionFailureKind::NoMatch => SafeErrorCategory::NoMatch,
        SemanticActionFailureKind::AmbiguousTarget => SafeErrorCategory::AmbiguousTarget,
        SemanticActionFailureKind::AmbiguousScope => SafeErrorCategory::AmbiguousScope,
        SemanticActionFailureKind::IncompleteTree => SafeErrorCategory::IncompleteTree,
        SemanticActionFailureKind::UnprovenSelectorState => {
            SafeErrorCategory::UnprovenSelectorState
        }
        SemanticActionFailureKind::NotActionable => SafeErrorCategory::NotActionable,
        SemanticActionFailureKind::UnstableTarget => SafeErrorCategory::UnstableTarget,
        SemanticActionFailureKind::FocusUnconfirmed => SafeErrorCategory::FocusUnconfirmed,
        SemanticActionFailureKind::UnsupportedMode => SafeErrorCategory::UnsupportedMode,
        SemanticActionFailureKind::ActionDeadlineExceeded => {
            SafeErrorCategory::ActionDeadlineExceeded
        }
        SemanticActionFailureKind::SequenceDeadlineExceeded => {
            SafeErrorCategory::SequenceDeadlineExceeded
        }
        SemanticActionFailureKind::ActionFailed => error
            .source
            .as_ref()
            .map_or(SafeErrorCategory::Other, SafeErrorCategory::from_error),
    }
}

fn failure_result(error: &SemanticActionError) -> Value {
    let (_, side_effects_may_have_occurred) =
        mutation_provenance(error.focus.as_ref(), error.action_dispatch);
    let mut result = json!({
        "dispatch": error.action_dispatch.as_str(),
        "side_effects_may_have_occurred": side_effects_may_have_occurred,
        "retry": error.retry.as_str(),
        "actionability": actionability_json(&error.actionability),
        "candidate_count": error.candidates.len(),
    });
    if let Some(resolution) = &error.resolution {
        result["resolution"] = resolution_json(resolution);
    }
    if let Some(focus) = &error.focus {
        result["focus"] = mutation_json(focus);
    }
    result
}

pub(crate) fn semantic_error(
    tool: &'static str,
    error: impl Into<Box<SemanticActionError>>,
) -> ContextualError {
    let error = error.into();
    let category = semantic_category(&error);
    let include_text = tool != "glass_type";
    let mut siblings = Vec::new();
    if !error.candidates.is_empty() {
        siblings.push(OutContent::untrusted_observation(
            &json!({ "candidates": candidates_json(&error.candidates, include_text) }).to_string(),
        ));
    } else if let Some(target) = &error.target {
        siblings.push(OutContent::untrusted_observation(
            &json!({ "target": element_json(target, include_text) }).to_string(),
        ));
    }
    let (bound_dispatch, _) = mutation_provenance(error.focus.as_ref(), error.action_dispatch);
    ContextualError {
        code: category.code(),
        message: category.summary().into(),
        category,
        safe_summary: category.summary(),
        sequence_deadline_exceeded: category == SafeErrorCategory::SequenceDeadlineExceeded,
        bound_dispatch: Some(bound_dispatch),
        result: Some(failure_result(&error)),
        siblings,
        post_write: false,
    }
}

#[cfg(test)]
pub(crate) mod tests;
