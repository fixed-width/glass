use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub(super) struct StepError {
    pub code: &'static str,
    pub summary: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum StepOutcome {
    Completed {
        index: usize,
        action: &'static str,
        result: Value,
        content_blocks: Vec<usize>,
    },
    Failed {
        index: usize,
        action: &'static str,
        attempted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        error: StepError,
        side_effects_may_have_occurred: bool,
        content_blocks: Vec<usize>,
    },
    Unexecuted {
        index: usize,
        action: &'static str,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum TerminalOutcome {
    Completed {
        operation: &'static str,
        result: Value,
        content_blocks: Vec<usize>,
    },
    Failed {
        operation: &'static str,
        error: StepError,
        content_blocks: Vec<usize>,
    },
    Unexecuted {
        operation: &'static str,
    },
}
