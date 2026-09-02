use glass_core::{
    ActionMode, ActionTarget, AxNodeId, ClickTargetParams, SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS,
    SEMANTIC_ACTION_MAX_TIMEOUT_MS, SetValueTargetParams, TypeTargetParams,
};

use crate::params::{Action, ActionModeArg, ClickElementArgs, SetValueArgs, TypeArgs};
use crate::tools::find::semantic_target;
use crate::tools::{ContextualError, validate_return, validate_settle_args};

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
    validate_return(return_).map_err(ContextualError::validation)
}

fn semantic_timeout(timeout_ms: Option<u64>) -> Result<u64, ContextualError> {
    let timeout_ms = timeout_ms.unwrap_or(SEMANTIC_ACTION_DEFAULT_TIMEOUT_MS);
    if timeout_ms > SEMANTIC_ACTION_MAX_TIMEOUT_MS {
        return Err(ContextualError::validation(format!(
            "`timeout_ms` must be between 0 and {SEMANTIC_ACTION_MAX_TIMEOUT_MS}"
        )));
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
                return Err(ContextualError::validation(
                    "`timeout_ms` is available only with `target`, not `id`".into(),
                ));
            }
            if a.max_nodes.is_some() {
                return Err(ContextualError::validation(
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
            ActionTarget::Semantic(semantic_target(target).map_err(ContextualError::validation)?)
        }
        _ => {
            return Err(ContextualError::validation(
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
                return Err(ContextualError::validation(
                    "`timeout_ms` is available only with `target`, not `id`".into(),
                ));
            }
            if a.max_nodes.is_some() {
                return Err(ContextualError::validation(
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
            ActionTarget::Semantic(semantic_target(target).map_err(ContextualError::validation)?)
        }
        _ => {
            return Err(ContextualError::validation(
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
            return Err(ContextualError::validation(
                "`focus_mode` requires a targeted type action".into(),
            ));
        }
        if a.timeout_ms.is_some() {
            return Err(ContextualError::validation(
                "`timeout_ms` requires a targeted type action".into(),
            ));
        }
        if a.max_nodes.is_some() {
            return Err(ContextualError::validation(
                "`max_nodes` requires a targeted type action".into(),
            ));
        }
        return Ok(ValidatedType::Untargeted);
    };
    Ok(ValidatedType::Targeted(TypeTargetParams {
        target: semantic_target(target).map_err(ContextualError::validation)?,
        focus_mode: a.focus_mode.map(Into::into).unwrap_or(ActionMode::Auto),
        timeout_ms: semantic_timeout(a.timeout_ms)?,
        max_nodes: a.max_nodes.map(|value| value as usize),
    }))
}

pub(crate) fn validate_action(action: &Action) -> Result<(), ContextualError> {
    match action {
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
    }
}

const _: fn(&Action) -> Result<(), ContextualError> = validate_action;

#[cfg(test)]
mod tests;
