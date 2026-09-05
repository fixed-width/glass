use serde::Serialize;
use serde_json::json;

use crate::artifacts::ArtifactStore;
use crate::output::{OutContent, TargetAccess, TextRole, TextTrust, ToolOutput};
use crate::output_policy::ToolCallOutcome;

use super::CallTrace;
use super::config::MAX_PAYLOAD_BYTES;
use super::recorder::evidence;

tokio::task_local! {
    pub(crate) static ACTIVE_CALL: CallTrace;
}

pub(crate) fn current_call() -> Option<CallTrace> {
    ACTIVE_CALL.try_with(Clone::clone).ok()
}

pub(crate) fn arguments(value: &impl Serialize) {
    if let Some(call) = current_call() {
        call.arguments(value);
    }
}

pub(crate) fn start_arguments(args: &crate::params::StartArgs) {
    #[derive(Serialize)]
    struct Args<'a> {
        build: &'a Option<String>,
        run: &'a [String],
        backend: &'a Option<String>,
        sandbox: &'a Option<String>,
        cwd: &'a Option<String>,
        env_entry_count: usize,
        window_hint: &'a Option<crate::params::WindowHintArgs>,
        timeout_ms: Option<u64>,
        a11y: Option<bool>,
    }
    arguments(&Args {
        build: &args.build,
        run: &args.run,
        backend: &args.backend,
        sandbox: &args.sandbox,
        cwd: &args.cwd,
        env_entry_count: args.env.len(),
        window_hint: &args.window_hint,
        timeout_ms: args.timeout_ms,
        a11y: args.a11y,
    });
}

impl CallTrace {
    pub fn logical_outcome(&self, outcome: &ToolCallOutcome) {
        self.output(
            "logical_outcome",
            &outcome.output,
            outcome.is_error,
            outcome.target_access,
            None,
        );
    }

    pub fn output(
        &self,
        kind: &str,
        output: &ToolOutput,
        is_error: bool,
        access: TargetAccess,
        store: Option<&ArtifactStore>,
    ) {
        self.capture(kind, json!({"is_error": is_error, "target_access": access, "content_blocks": output.0.len(), "client_delivery": "unknown"}), |capture| {
            for (index, block) in output.0.iter().enumerate() {
                let entries = if matches!(block, OutContent::ResourceLink(_)) { 2 } else { 1 };
                if capture.entries() + entries >= 128 {
                    capture.omission(evidence("remaining_content_blocks", "application/json", "mixed", Some(index)), "block_limit");
                    break;
                }
                match block {
                    OutContent::Envelope(envelope) => {
                        #[derive(Serialize)]
                        struct Envelope<'a> { ok: bool, result: &'a serde_json::Value, tool: &'a str }
                        capture.json(evidence("envelope", "application/json", "glass", Some(index)), &Envelope { ok: true, tool: &envelope.tool, result: &envelope.result });
                    }
                    OutContent::Text(text) => {
                        let trust = match text.trust { TextTrust::Trusted => "glass", TextTrust::UntrustedApplication => "untrusted_application" };
                        let role = match text.role { TextRole::Envelope => "envelope", TextRole::Observation => "observation", TextRole::Guidance => "guidance", TextRole::ErrorDetail => "error_detail" };
                        capture.bytes(evidence(role, "text/plain; charset=utf-8", trust, Some(index)), text.body.as_bytes());
                    }
                    OutContent::Image(bytes) => capture.bytes(evidence("image", "image/webp", "untrusted_application", Some(index)), bytes),
                    OutContent::ResourceLink(descriptor) => {
                        capture.json(evidence("resource_descriptor", "application/json", "glass", Some(index)), &rmcp::model::ContentBlock::ResourceLink(descriptor.to_resource(access)));
                        let mut metadata = evidence("resource", descriptor.mime_type(), if descriptor.untrusted() { "untrusted_application" } else { "glass" }, Some(index));
                        metadata.source_uri = Some(descriptor.uri().to_owned());
                        metadata.original_bytes = Some(descriptor.bytes());
                        if descriptor.bytes() > MAX_PAYLOAD_BYTES as u64 {
                            capture.omission(metadata, "payload_limit");
                        } else if let Some(store) = store {
                            capture.read_bytes(metadata, descriptor.bytes() as usize, || store.read(descriptor.uri()).ok().map(|read| read.text.into_bytes()));
                        } else { capture.omission(metadata, "artifact_unavailable"); }
                    }
                }
            }
        });
    }

    pub fn resource_read(&self, result: &Result<rmcp::model::ReadResourceResult, rmcp::ErrorData>) {
        self.capture(
            "resource_read",
            json!({"is_error": result.is_err()}),
            |capture| {
                if let Ok(result) = result {
                    for (index, content) in result.contents.iter().take(127).enumerate() {
                        if let rmcp::model::ResourceContents::TextResourceContents {
                            text,
                            uri,
                            mime_type,
                            meta,
                        } = content
                        {
                            let mut metadata = evidence(
                                "resource",
                                mime_type.as_deref().unwrap_or("text/plain"),
                                if meta
                                    .as_ref()
                                    .and_then(|meta| meta.0.get("glass"))
                                    .and_then(|glass| glass.get("untrusted"))
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(false)
                                {
                                    "glass"
                                } else {
                                    "untrusted_application"
                                },
                                Some(index),
                            );
                            metadata.source_uri = Some(uri.clone());
                            capture.bytes(metadata, text.as_bytes());
                        }
                    }
                }
            },
        );
        self.record("logical_outcome", json!({"is_error": result.is_err()}));
    }
}

pub(crate) struct RequestGuard {
    call: CallTrace,
    completed: bool,
}

impl RequestGuard {
    pub fn new(call: CallTrace) -> Self {
        Self {
            call,
            completed: false,
        }
    }
    pub fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.call.record(
                "request_abandoned",
                json!({"execution_outcome": "observe_worker_outcome"}),
            );
        }
    }
}
