//! `glass_do`: run an ordered input sequence server-side, then optionally observe.

use glass_core::{BoundDispatch, Deadline, Glass, Whose};
use serde_json::json;
use std::time::{Duration, Instant};

use crate::params::*;
use crate::tools::{
    BatchToolResult, ContextualError, ContextualToolResult, OutContent, SafeErrorCategory,
    ToolContext, ToolOutput, click_element_with, click_with, diff_with, drag_with, key_with,
    mouse_move_with, screenshot_with, scroll_to_element_with, scroll_with, set_value_with,
    type_text_with, wait_for_element_with, wait_stable_with,
};

mod model;

use model::{StepError, StepOutcome, TerminalOutcome};

const MAX_ACTIONS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 65_536;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

fn checked_sequence_deadline(started: Instant, timeout: Duration) -> Option<Deadline> {
    started.checked_add(timeout).map(Deadline::at)
}

fn caller_deadline_won(timed_out_by: Option<Whose>, deadline: Deadline) -> bool {
    timed_out_by == Some(Whose::Caller) || deadline.has_passed()
}

/// Split a sub-tool's enveloped output into (its `result` payload, its non-envelope
/// sibling blocks — images and the IMAGE_NOTE). The envelope text block itself is consumed.
///
/// settle/diff/screenshot are glass's own functions and always emit an `{ok,tool,result}`
/// envelope block — that's an internal invariant, not something driven by untrusted app
/// input. So a sub-tool output with no envelope block is a bug in glass itself, and this
/// panics rather than silently defaulting to `{}` (a silent `{}` here would mask a broken
/// invariant behind a plausible-looking empty result).
fn split_sub(out: ToolOutput) -> (serde_json::Value, Vec<OutContent>) {
    let mut result = None;
    let mut siblings = Vec::new();
    for c in out.0 {
        match c {
            OutContent::Envelope(envelope) => result = Some(envelope.result),
            img => siblings.push(img),
        }
    }
    match result {
        Some(result) => (result, siblings),
        None => panic!("glass_do sub-tool must emit an {{ok,tool,result}} envelope"),
    }
}

/// Build a text-only `WaitStableArgs` from a `SettleArgs` (no image, no crop).
fn settle_args(s: &SettleArgs) -> WaitStableArgs {
    WaitStableArgs {
        max_width: None,
        max_height: None,
        interval_ms: s.interval_ms,
        settle_frames: s.settle_frames,
        tolerance: s.tolerance,
        timeout_ms: s.timeout_ms,
        region: None,
        stability_region: s.stability_region.clone(),
        include_image: Some(false),
        window_id: None,
        ignore: s.ignore.clone(),
    }
}

/// Run an ordered action sequence, then the optional terminal observe.
/// Fail-fast: the first failing action aborts with its index/kind/message and
/// the count of prior completed actions; the failed step separately records
/// whether it was attempted. A `then` failure is reported distinctly after all
/// actions completed.
pub fn do_actions(glass: &mut Glass, a: &DoArgs) -> BatchToolResult {
    if a.actions.is_empty() {
        return Err(validation_error(
            "`actions` must contain at least one action",
        ));
    }
    if a.actions.len() > MAX_ACTIONS {
        return Err(validation_error(&format!(
            "`actions` must contain at most {MAX_ACTIONS} actions"
        )));
    }
    if a.encoded_argument_bytes > MAX_ARGUMENT_BYTES {
        return Err(validation_error(&format!(
            "encoded arguments exceed the {MAX_ARGUMENT_BYTES}-byte limit"
        )));
    }
    let timeout_ms = a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(validation_error(&format!(
            "`timeout_ms` must be between 1 and {MAX_TIMEOUT_MS}"
        )));
    }
    for (index, action) in a.actions.iter().enumerate() {
        if let Err(error) = crate::tools::semantic_action::validate_action(action) {
            return Err(validation_error_at(index, action.kind(), error));
        }
    }
    let started = Instant::now();
    let Some(deadline) = checked_sequence_deadline(started, Duration::from_millis(timeout_ms))
    else {
        return Err(validation_error(
            "`timeout_ms` is outside this platform's monotonic clock range",
        ));
    };
    let context = ToolContext {
        deadline,
        owner: Some(glass_core::Whose::Caller),
        allow_wait: true,
    };
    let n = a.actions.len();
    let mut steps = Vec::with_capacity(n);
    let mut siblings = Vec::new();
    for (i, action) in a.actions.iter().enumerate() {
        let kind = action.kind();
        if context.deadline.has_passed() {
            let error = ContextualError::validation(
                "sequence deadline exceeded before action started".into(),
            )
            .after_sequence_deadline();
            return Err(step_failure(
                &a.actions,
                i,
                action,
                error,
                steps,
                siblings,
                started.elapsed().as_millis(),
            ));
        }
        let result: ContextualToolResult = match action {
            Action::Click(args) => click_with(glass, args, context),
            Action::Move(args) => mouse_move_with(glass, args, context),
            Action::Drag(args) => drag_with(glass, args, context),
            Action::Scroll(args) => scroll_with(glass, args, context),
            Action::Type(args) => type_text_with(glass, args, context),
            Action::Key(args) => key_with(glass, args, context),
            // A settle's result is retained in the step outcome. `settled:false`
            // under its own timeout is completed; only an error or the enclosing
            // sequence deadline aborts.
            Action::Settle(args) => wait_stable_with(glass, &settle_args(args), context),
            Action::ClickElement(args) => click_element_with(glass, args, context),
            Action::SetValue(args) => set_value_with(glass, args, context),
            Action::WaitForElement(args) => wait_for_element_with(glass, args, context),
            Action::ScrollToElement(args) => scroll_to_element_with(glass, args, context),
        };
        match result {
            Ok(out) => {
                let timed_out_by = out.timed_out_by;
                let (result, mut extra) = split_sub(out.output);
                let start = siblings.len() + 1;
                let content_blocks = (start..start + extra.len()).collect();
                siblings.append(&mut extra);
                if caller_deadline_won(timed_out_by, context.deadline) {
                    return Err(predicate_failure(
                        &a.actions,
                        i,
                        kind,
                        action.is_side_effecting(),
                        true,
                        result,
                        content_blocks,
                        steps,
                        siblings,
                        started.elapsed().as_millis(),
                    ));
                }
                let predicate_failed =
                    matches!(
                        action,
                        Action::WaitForElement(_) | Action::ScrollToElement(_)
                    ) && result.get("matched").and_then(serde_json::Value::as_bool) == Some(false);
                if predicate_failed {
                    return Err(predicate_failure(
                        &a.actions,
                        i,
                        kind,
                        action.is_side_effecting(),
                        false,
                        result,
                        content_blocks,
                        steps,
                        siblings,
                        started.elapsed().as_millis(),
                    ));
                }
                steps.push(StepOutcome::Completed {
                    index: i,
                    action: kind,
                    result,
                    content_blocks,
                });
            }
            Err(error) => {
                let error = if context.deadline.has_passed() {
                    error.after_sequence_deadline()
                } else {
                    error
                };
                return Err(step_failure(
                    &a.actions,
                    i,
                    action,
                    error,
                    steps,
                    siblings,
                    started.elapsed().as_millis(),
                ));
            }
        }
    }

    let mut result = json!({ "status": "completed", "executed": n, "steps": steps });
    if let Some(then) = &a.then {
        match run_then(glass, then, context, siblings.len() + 1) {
            Ok(mut terminal) => {
                result["then"] = terminal.meta;
                result["terminal_steps"] = json!(terminal.outcomes);
                siblings.append(&mut terminal.siblings);
            }
            Err(mut terminal) => {
                siblings.append(&mut terminal.siblings);
                let mut outcome = failure_outcome(steps, n, started.elapsed().as_millis());
                outcome["then"] = terminal.meta;
                outcome["terminal_steps"] = json!(terminal.outcomes);
                let (code, summary) = if terminal.sequence_deadline_exceeded {
                    (
                        "sequence_deadline_exceeded",
                        "sequence deadline exceeded after actions completed; do not replay actions",
                    )
                } else {
                    (
                        "terminal_observe_failed",
                        "terminal observation failed after actions completed; do not replay actions",
                    )
                };
                return Err(error_output(
                    json!({
                    "ok": false,
                    "tool": "glass_do",
                    "error": { "code": code, "summary": summary },
                    "outcome": outcome,
                    }),
                    siblings,
                ));
            }
        }
    }
    result["elapsed_ms"] = json!(started.elapsed().as_millis());
    Ok(ToolOutput::result_with("glass_do", result, siblings))
}

#[allow(clippy::too_many_arguments)]
fn predicate_failure(
    actions: &[Action],
    index: usize,
    action: &'static str,
    side_effects_may_have_occurred: bool,
    sequence_deadline_exceeded: bool,
    result: serde_json::Value,
    content_blocks: Vec<usize>,
    mut steps: Vec<StepOutcome>,
    siblings: Vec<OutContent>,
    elapsed_ms: u128,
) -> ToolOutput {
    let (code, summary) = if sequence_deadline_exceeded {
        ("sequence_deadline_exceeded", "sequence deadline exceeded")
    } else {
        ("predicate_not_matched", "element predicate did not match")
    };
    steps.push(StepOutcome::Failed {
        index,
        action,
        attempted: true,
        result: Some(result),
        error: StepError {
            code,
            summary: summary.into(),
            category: sequence_deadline_exceeded
                .then_some(SafeErrorCategory::SequenceDeadlineExceeded),
        },
        side_effects_may_have_occurred,
        content_blocks,
    });
    steps.extend(
        actions[index + 1..]
            .iter()
            .enumerate()
            .map(|(offset, action)| StepOutcome::Unexecuted {
                index: index + offset + 1,
                action: action.kind(),
            }),
    );
    error_output(
        json!({
            "ok": false,
            "tool": "glass_do",
            "error": {
                "code": code,
                "step": index,
                "summary": summary,
            },
            "outcome": failure_outcome(steps, index, elapsed_ms),
        }),
        siblings,
    )
}

fn step_failure(
    actions: &[Action],
    index: usize,
    action: &Action,
    mut error: ContextualError,
    mut steps: Vec<StepOutcome>,
    mut siblings: Vec<OutContent>,
    elapsed_ms: u128,
) -> ToolOutput {
    let attempted = match error.bound_dispatch {
        Some(BoundDispatch::NotDispatched) => false,
        Some(BoundDispatch::MayHaveDispatched) | None => true,
    };
    let side_effects_may_have_occurred = attempted && action.is_side_effecting();
    let detail = match action {
        Action::Type(_) | Action::SetValue(_) => error.safe_summary,
        _ => &error.message,
    };
    let content_start = siblings.len() + 1;
    let content_count = error.siblings.len() + 1;
    siblings.append(&mut error.siblings);
    siblings.push(OutContent::untrusted_observation(detail));
    let content_blocks = (content_start..content_start + content_count).collect();
    steps.push(StepOutcome::Failed {
        index,
        action: action.kind(),
        attempted,
        result: error.result,
        error: StepError {
            code: error.code,
            summary: error.safe_summary.into(),
            category: Some(error.category),
        },
        side_effects_may_have_occurred,
        content_blocks,
    });
    steps.extend(
        actions[index + 1..]
            .iter()
            .enumerate()
            .map(|(offset, action)| StepOutcome::Unexecuted {
                index: index + offset + 1,
                action: action.kind(),
            }),
    );
    error_output(
        json!({
            "ok": false,
            "tool": "glass_do",
            "error": {
                "code": error.code,
                "step": index,
                "summary": error.safe_summary,
            },
            "outcome": failure_outcome(steps, index, elapsed_ms),
        }),
        siblings,
    )
}

fn validation_error(summary: &str) -> ToolOutput {
    error_output(
        json!({
            "ok": false,
            "tool": "glass_do",
            "error": { "code": "invalid_sequence", "summary": summary },
        }),
        Vec::new(),
    )
}

fn validation_error_at(index: usize, action: &'static str, error: ContextualError) -> ToolOutput {
    error_output(
        json!({
            "ok": false,
            "tool": "glass_do",
            "error": {
                "code": "invalid_sequence",
                "step": index,
                "action": action,
                "summary": error.message,
            },
        }),
        Vec::new(),
    )
}

fn error_output(envelope: serde_json::Value, mut siblings: Vec<OutContent>) -> ToolOutput {
    let mut content = vec![OutContent::trusted_error(envelope.to_string())];
    content.append(&mut siblings);
    ToolOutput(content)
}

fn failure_outcome(
    steps: Vec<StepOutcome>,
    executed: usize,
    elapsed_ms: u128,
) -> serde_json::Value {
    json!({
        "status": "failed",
        "executed": executed,
        "steps": steps,
        "effects_rolled_back": false,
        "elapsed_ms": elapsed_ms,
    })
}

impl Action {
    fn kind(&self) -> &'static str {
        match self {
            Action::Click(_) => "click",
            Action::Move(_) => "move",
            Action::Drag(_) => "drag",
            Action::Scroll(_) => "scroll",
            Action::Type(_) => "type",
            Action::Key(_) => "key",
            Action::Settle(_) => "settle",
            Action::ClickElement(args) => {
                let _ = args;
                "click_element"
            }
            Action::SetValue(args) => {
                let _ = args;
                "set_value"
            }
            Action::WaitForElement(args) => {
                let _ = args;
                "wait_for_element"
            }
            Action::ScrollToElement(args) => {
                let _ = args;
                "scroll_to_element"
            }
        }
    }

    fn is_side_effecting(&self) -> bool {
        !matches!(self, Action::Settle(_) | Action::WaitForElement(_))
    }
}

struct ThenRun {
    meta: serde_json::Value,
    outcomes: Vec<TerminalOutcome>,
    siblings: Vec<OutContent>,
    sequence_deadline_exceeded: bool,
}

/// Run terminal observation in fixed order under the sequence's one deadline.
fn run_then(
    glass: &mut Glass,
    then: &ThenArgs,
    context: ToolContext,
    sibling_base: usize,
) -> Result<ThenRun, ThenRun> {
    let mut run = ThenRun {
        meta: json!({}),
        outcomes: Vec::new(),
        siblings: Vec::new(),
        sequence_deadline_exceeded: false,
    };

    macro_rules! terminal_operation {
        ($operation:literal, $call:expr, [$($later:literal),* $(,)?]) => {{
            if context.deadline.has_passed() {
                run.sequence_deadline_exceeded = true;
                run.outcomes.push(TerminalOutcome::Failed {
                    operation: $operation,
                    error: StepError {
                        code: "sequence_deadline_exceeded",
                        summary: "sequence deadline exceeded".into(),
                        category: Some(SafeErrorCategory::SequenceDeadlineExceeded),
                    },
                    content_blocks: Vec::new(),
                });
                $(run.outcomes.push(TerminalOutcome::Unexecuted { operation: $later });)*
                return Err(run);
            }
            match $call {
                Ok(out) if !caller_deadline_won(out.timed_out_by, context.deadline) => {
                    let (result, mut extra) = split_sub(out.output);
                    let start = sibling_base + run.siblings.len();
                    let content_blocks = (start..start + extra.len()).collect();
                    run.siblings.append(&mut extra);
                    run.meta[$operation] = result.clone();
                    run.outcomes.push(TerminalOutcome::Completed { operation: $operation, result, content_blocks });
                }
                Ok(_) => {
                    run.sequence_deadline_exceeded = true;
                    run.outcomes.push(TerminalOutcome::Failed {
                        operation: $operation,
                        error: StepError {
                            code: "sequence_deadline_exceeded",
                            summary: "sequence deadline exceeded".into(),
                            category: Some(SafeErrorCategory::SequenceDeadlineExceeded),
                        },
                        content_blocks: Vec::new(),
                    });
                    $(run.outcomes.push(TerminalOutcome::Unexecuted { operation: $later });)*
                    return Err(run);
                }
                Err(error) => {
                    let error = if context.deadline.has_passed() {
                        error.after_sequence_deadline()
                    } else {
                        error
                    };
                    run.sequence_deadline_exceeded = error.sequence_deadline_exceeded;
                    let code = if error.sequence_deadline_exceeded { "sequence_deadline_exceeded" } else { "action_failed" };
                    let summary = if error.sequence_deadline_exceeded { "sequence deadline exceeded" } else { "terminal observation failed" };
                    let content_blocks = if error.message.is_empty() { Vec::new() } else {
                        let index = sibling_base + run.siblings.len();
                        run.siblings.push(OutContent::untrusted_observation(&error.message));
                        vec![index]
                    };
                    run.outcomes.push(TerminalOutcome::Failed {
                        operation: $operation,
                        error: StepError {
                            code,
                            summary: summary.into(),
                            category: Some(error.category),
                        },
                        content_blocks,
                    });
                    $(run.outcomes.push(TerminalOutcome::Unexecuted { operation: $later });)*
                    return Err(run);
                }
            }
        }};
    }
    if let Some(s) = &then.settle {
        if then.diff.is_some() && then.screenshot.is_some() {
            terminal_operation!(
                "settle",
                wait_stable_with(glass, &settle_args(s), context),
                ["diff", "screenshot"]
            );
        } else if then.diff.is_some() {
            terminal_operation!(
                "settle",
                wait_stable_with(glass, &settle_args(s), context),
                ["diff"]
            );
        } else if then.screenshot.is_some() {
            terminal_operation!(
                "settle",
                wait_stable_with(glass, &settle_args(s), context),
                ["screenshot"]
            );
        } else {
            terminal_operation!(
                "settle",
                wait_stable_with(glass, &settle_args(s), context),
                []
            );
        }
    }
    if let Some(d) = &then.diff {
        if then.screenshot.is_some() {
            terminal_operation!("diff", diff_with(glass, d, context), ["screenshot"]);
        } else {
            terminal_operation!("diff", diff_with(glass, d, context), []);
        }
    }
    if let Some(sc) = &then.screenshot {
        terminal_operation!("screenshot", screenshot_with(glass, sc, context), []);
    }
    Ok(run)
}

#[cfg(test)]
mod tests;
