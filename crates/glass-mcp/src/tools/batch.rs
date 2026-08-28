//! `glass_do`: run an ordered input sequence server-side, then optionally observe.

use glass_core::{Deadline, Glass, Whose};
use serde_json::json;
use std::time::{Duration, Instant};

use crate::params::*;
use crate::tools::{
    BatchToolResult, ContextualToolResult, OutContent, ToolContext, ToolOutput, click_element_with,
    click_with, diff, drag_with, key_with, mouse_move_with, screenshot, scroll_to_element_with,
    scroll_with, set_value_with, type_text_with, wait_for_element_with, wait_stable,
    wait_stable_with,
};

mod model;

use model::{StepError, StepOutcome};

const MAX_ACTIONS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 65_536;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

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
            OutContent::Text(t) => match serde_json::from_str::<serde_json::Value>(&t) {
                // Require the real envelope shape (`ok` + `tool`), not just any JSON
                // object that happens to have a `result` key — a future JSON-shaped
                // untrusted sibling must not be misclassified as the envelope.
                Ok(v) if v.get("ok").is_some() && v.get("tool").is_some() => {
                    result = Some(v["result"].clone());
                }
                _ => siblings.push(OutContent::Text(t)), // e.g. IMAGE_NOTE (not JSON)
            },
            img => siblings.push(img),
        }
    }
    let result = result.expect("glass_do sub-tool must emit an {ok,tool,result} envelope");
    (result, siblings)
}

/// Build a text-only `WaitStableArgs` from a `SettleArgs` (no image, no crop).
fn settle_args(s: &SettleArgs) -> WaitStableArgs {
    WaitStableArgs {
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
/// the count that ran. A `then` failure is reported distinctly (the actions
/// already executed).
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
    let started = Instant::now();
    let context = ToolContext {
        deadline: Deadline::at(started + Duration::from_millis(timeout_ms)),
    };
    let n = a.actions.len();
    let mut steps = Vec::with_capacity(n);
    let mut siblings = Vec::new();
    for (i, action) in a.actions.iter().enumerate() {
        let kind = action.kind();
        if context.deadline.has_passed() {
            return Err(step_failure(
                &a.actions,
                i,
                kind,
                false,
                false,
                "sequence deadline exceeded before action started",
                true,
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
            // A settle's text-only output is discarded mid-sequence; only its
            // Err (bad region / capture failure) aborts. A non-settle (timeout)
            // is Ok and proceeds.
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
                        action.is_mutating(),
                        timed_out_by == Some(Whose::Caller),
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
                return Err(step_failure(
                    &a.actions,
                    i,
                    kind,
                    true,
                    action.is_mutating(),
                    &error.message,
                    error.sequence_deadline_exceeded,
                    steps,
                    siblings,
                    started.elapsed().as_millis(),
                ));
            }
        }
    }

    let mut result = json!({ "executed": n, "steps": steps });
    if let Some(then) = &a.then {
        match run_then(glass, then) {
            Ok((meta, mut extra)) => {
                result["then"] = meta;
                siblings.append(&mut extra);
            }
            Err(detail) => {
                siblings.push(OutContent::Text(crate::untrusted::wrap_untrusted(&detail)));
                return Err(error_output(
                    json!({
                    "ok": false,
                    "tool": "glass_do",
                    "error": { "code": "terminal_observe_failed", "summary": "terminal observation failed" },
                    "outcome": failure_outcome(steps, n, started.elapsed().as_millis()),
                    }),
                    siblings,
                ));
            }
        }
    }
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

#[allow(clippy::too_many_arguments)]
fn step_failure(
    actions: &[Action],
    index: usize,
    action: &'static str,
    attempted: bool,
    side_effects_may_have_occurred: bool,
    detail: &str,
    sequence_deadline_exceeded: bool,
    mut steps: Vec<StepOutcome>,
    mut siblings: Vec<OutContent>,
    elapsed_ms: u128,
) -> ToolOutput {
    let (code, summary) = if sequence_deadline_exceeded {
        ("sequence_deadline_exceeded", "sequence deadline exceeded")
    } else {
        ("action_failed", "action execution failed")
    };
    let content_block = siblings.len() + 1;
    siblings.push(OutContent::Text(crate::untrusted::wrap_untrusted(detail)));
    steps.push(StepOutcome::Failed {
        index,
        action,
        attempted,
        result: None,
        error: StepError {
            code,
            summary: summary.into(),
        },
        side_effects_may_have_occurred,
        content_blocks: vec![content_block],
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
            "error": { "code": code, "step": index, "summary": summary },
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

fn error_output(envelope: serde_json::Value, mut siblings: Vec<OutContent>) -> ToolOutput {
    let mut content = vec![OutContent::Text(envelope.to_string())];
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

    fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Action::Move(_) | Action::Settle(_) | Action::WaitForElement(_)
        )
    }
}

/// Run the terminal observe in fixed order: settle → diff → screenshot. Returns
/// the `then` metadata object (each ran sub-tool's `result` payload keyed by
/// name) and the collected image/IMAGE_NOTE sibling blocks, in run order.
fn run_then(
    glass: &mut Glass,
    then: &ThenArgs,
) -> Result<(serde_json::Value, Vec<OutContent>), String> {
    let mut meta = json!({});
    let mut siblings = Vec::new();
    if let Some(s) = &then.settle {
        let (r, mut sib) = split_sub(wait_stable(glass, &settle_args(s))?);
        meta["settle"] = r;
        siblings.append(&mut sib);
    }
    if let Some(d) = &then.diff {
        let (r, mut sib) = split_sub(diff(glass, d)?);
        meta["diff"] = r;
        siblings.append(&mut sib);
    }
    if let Some(sc) = &then.screenshot {
        let (r, mut sib) = split_sub(screenshot(glass, sc)?);
        meta["screenshot"] = r;
        siblings.append(&mut sib);
    }
    Ok((meta, siblings))
}

#[cfg(test)]
mod tests;
