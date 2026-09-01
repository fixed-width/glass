use crate::artifacts::{
    ArtifactDraft, ArtifactError, ArtifactStore, PreparedArtifact, ResponsePin,
};
use crate::output::{
    ArtifactDescriptor, EnvelopeBlock, OutContent, TargetAccess, TextRole, TextTrust, ToolEffect,
    ToolOutput,
};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const MAX_TEXT_BYTES: usize = 8_192;
const READ_RECOVERY: &str =
    "Correct artifact storage or narrow the read, then retry this read-only operation.";
const MUTATE_RECOVERY: &str = "Correct artifact storage, then use a read-only inspection tool. Do not repeat the action solely to recover this observation.";

#[derive(Clone, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Reserved for crate-internal response integration."
    )
)]
pub(crate) struct ToolCallOutcome {
    pub tool: &'static str,
    pub effect: ToolEffect,
    pub is_error: bool,
    pub target_access: TargetAccess,
    pub output: ToolOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetrySafety {
    SafeToRetryRead,
    DoNotRepeatAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OutputDeliveryError {
    pub category: &'static str,
    pub action_outcome_preserved: bool,
    pub retry_safety: RetrySafety,
    pub recovery: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputMode {
    ContentBlocks,
    ResponseManifest,
    Incomplete,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OutputMetadata {
    pub mode: OutputMode,
    pub budget_bytes: usize,
    pub original_text_bytes: usize,
    pub inline_text_bytes: usize,
    pub complete: bool,
    pub target_access: TargetAccess,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub externalized: Vec<ArtifactDescriptor>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub omitted_content_blocks: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OutputDeliveryError>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Reserved for crate-internal response integration."
    )
)]
pub(crate) struct AppliedOutcome {
    pub output: ToolOutput,
    pub is_error: bool,
    metadata: Option<OutputMetadata>,
    response_pin: Option<ResponsePin>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Reserved for crate-internal response integration."
    )
)]
pub(crate) struct OutputPolicy {
    store: Option<ArtifactStore>,
    permanently_disabled: AtomicBool,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Reserved for crate-internal response integration."
    )
)]
impl OutputPolicy {
    pub(crate) fn new(store: ArtifactStore) -> Self {
        Self {
            store: Some(store),
            permanently_disabled: AtomicBool::new(false),
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self {
            store: None,
            permanently_disabled: AtomicBool::new(true),
        }
    }

    pub(crate) fn apply(&self, outcome: ToolCallOutcome) -> AppliedOutcome {
        let original_text_bytes = outcome.output.text_bytes();
        if original_text_bytes <= MAX_TEXT_BYTES {
            return AppliedOutcome {
                output: outcome.output,
                is_error: outcome.is_error,
                metadata: None,
                response_pin: None,
            };
        }

        if has_pre_policy_link(&outcome.output) || has_output_collision(&outcome.output) {
            return emergency(&outcome, original_text_bytes);
        }
        if !self.permanently_disabled.load(Ordering::Acquire)
            && let Some(store) = &self.store
        {
            if let Some(applied) = self.try_content_blocks(store, &outcome, original_text_bytes) {
                return applied;
            }
            if let Some(applied) = self.try_manifest(store, &outcome, original_text_bytes) {
                return applied;
            }
        }
        incomplete(&outcome, original_text_bytes)
    }

    fn try_content_blocks(
        &self,
        store: &ArtifactStore,
        outcome: &ToolCallOutcome,
        original_text_bytes: usize,
    ) -> Option<AppliedOutcome> {
        let envelope_index = usable_envelope_index(&outcome.output)?;
        let mut eligible = outcome
            .output
            .0
            .iter()
            .enumerate()
            .filter_map(|(index, content)| match content {
                OutContent::Text(text)
                    if matches!(
                        text.role,
                        TextRole::Observation | TextRole::Guidance | TextRole::ErrorDetail
                    ) =>
                {
                    Some((index, text.body.len()))
                }
                OutContent::Envelope(_)
                | OutContent::Text(_)
                | OutContent::Image(_)
                | OutContent::ResourceLink(_) => None,
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        if eligible.is_empty() {
            return None;
        }

        let mut prepared = Vec::new();
        for (index, _) in eligible {
            let OutContent::Text(text) = outcome.output.0.get(index)? else {
                return None;
            };
            let draft = ArtifactDraft::content_block(
                text.body.clone(),
                mime_for_role(text.role),
                text.trust == TextTrust::UntrustedApplication,
                index,
            );
            let item = match store.prepare(draft) {
                Ok(item) => item,
                Err(error) => {
                    self.disable_for_invariant(store, error);
                    return None;
                }
            };
            prepared.push(item);
            let descriptors = prepared
                .iter()
                .map(PreparedArtifact::descriptor)
                .cloned()
                .collect::<Vec<_>>();
            let metadata = complete_metadata(
                OutputMode::ContentBlocks,
                original_text_bytes,
                outcome.target_access,
                descriptors,
            );
            let candidate =
                build_block_candidate(&outcome.output, envelope_index, &prepared, &metadata)?;
            let (candidate, metadata) = stabilize(candidate, metadata, envelope_index)?;
            if candidate.text_bytes() <= MAX_TEXT_BYTES {
                let published = match store.publish(prepared) {
                    Ok(batch) => batch,
                    Err(error) => {
                        self.disable_for_invariant(store, error);
                        return Some(incomplete(outcome, original_text_bytes));
                    }
                };
                return Some(AppliedOutcome {
                    output: candidate,
                    is_error: outcome.is_error,
                    metadata: Some(metadata),
                    response_pin: Some(published.into_pin()),
                });
            }
        }
        None
    }

    fn try_manifest(
        &self,
        store: &ArtifactStore,
        outcome: &ToolCallOutcome,
        original_text_bytes: usize,
    ) -> Option<AppliedOutcome> {
        let manifest = serde_json::to_string(&ResponseManifest::from_outcome(outcome)?).ok()?;
        let indices = (0..outcome.output.0.len()).collect::<Vec<_>>();
        let prepared = match store.prepare(ArtifactDraft::response_manifest(manifest, indices)) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.disable_for_invariant(store, error);
                return None;
            }
        };
        let descriptor = prepared.descriptor().clone();
        let metadata = complete_metadata(
            OutputMode::ResponseManifest,
            original_text_bytes,
            outcome.target_access,
            vec![descriptor.clone()],
        );
        let candidate = manifest_candidate(outcome, descriptor, &metadata);
        let (candidate, metadata) = stabilize(candidate, metadata, 0)?;
        if candidate.text_bytes() > MAX_TEXT_BYTES {
            self.permanently_disabled.store(true, Ordering::Release);
            store.mark_unavailable(ArtifactError::InvalidOutputState);
            return None;
        }
        let published = match store.publish(vec![prepared]) {
            Ok(batch) => batch,
            Err(error) => {
                self.disable_for_invariant(store, error);
                return Some(incomplete(outcome, original_text_bytes));
            }
        };
        Some(AppliedOutcome {
            output: candidate,
            is_error: outcome.is_error,
            metadata: Some(metadata),
            response_pin: Some(published.into_pin()),
        })
    }

    fn disable_for_invariant(&self, store: &ArtifactStore, error: ArtifactError) {
        if matches!(
            error,
            ArtifactError::RollbackFailed
                | ArtifactError::PathRepresentationFailed
                | ArtifactError::InvalidOutputState
                | ArtifactError::MetadataDidNotStabilize
        ) {
            self.permanently_disabled.store(true, Ordering::Release);
            store.mark_unavailable(error);
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Reserved for crate-internal response integration."
    )
)]
impl AppliedOutcome {
    pub(crate) fn output_metadata(&self) -> Option<&OutputMetadata> {
        self.metadata.as_ref()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ToolOutput,
        bool,
        Option<OutputMetadata>,
        Option<ResponsePin>,
    ) {
        (self.output, self.is_error, self.metadata, self.response_pin)
    }
}

fn complete_metadata(
    mode: OutputMode,
    original_text_bytes: usize,
    target_access: TargetAccess,
    externalized: Vec<ArtifactDescriptor>,
) -> OutputMetadata {
    OutputMetadata {
        mode,
        budget_bytes: MAX_TEXT_BYTES,
        original_text_bytes,
        inline_text_bytes: 0,
        complete: true,
        target_access,
        externalized,
        omitted_content_blocks: Vec::new(),
        error: None,
    }
}

fn build_block_candidate(
    original: &ToolOutput,
    envelope_index: usize,
    prepared: &[PreparedArtifact],
    metadata: &OutputMetadata,
) -> Option<ToolOutput> {
    let mut contents = original.0.clone();
    for item in prepared {
        let descriptor = item.descriptor();
        let index = descriptor_content_block(descriptor)?;
        let slot = contents.get_mut(index)?;
        *slot = OutContent::ResourceLink(descriptor.clone());
    }
    set_envelope_metadata(&mut contents, envelope_index, metadata)?;
    Some(ToolOutput(contents))
}

fn manifest_candidate(
    outcome: &ToolCallOutcome,
    descriptor: ArtifactDescriptor,
    metadata: &OutputMetadata,
) -> ToolOutput {
    let envelope = OutContent::Envelope(EnvelopeBlock {
        tool: bounded_tool(outcome.tool).to_owned(),
        result: serde_json::json!({"is_error": outcome.is_error, "output": metadata}),
    });
    let mut contents = vec![envelope, OutContent::ResourceLink(descriptor)];
    contents.extend(outcome.output.0.iter().filter_map(|content| match content {
        OutContent::Image(bytes) => Some(OutContent::Image(bytes.clone())),
        OutContent::Envelope(_) | OutContent::Text(_) | OutContent::ResourceLink(_) => None,
    }));
    ToolOutput(contents)
}

fn stabilize(
    mut output: ToolOutput,
    mut metadata: OutputMetadata,
    envelope_index: usize,
) -> Option<(ToolOutput, OutputMetadata)> {
    for _ in 0..=6 {
        let exact = output.text_bytes();
        if metadata.inline_text_bytes == exact {
            return Some((output, metadata));
        }
        metadata.inline_text_bytes = exact;
        set_envelope_metadata(&mut output.0, envelope_index, &metadata)?;
    }
    None
}

fn set_envelope_metadata(
    contents: &mut [OutContent],
    envelope_index: usize,
    metadata: &OutputMetadata,
) -> Option<()> {
    let OutContent::Envelope(envelope) = contents.get_mut(envelope_index)? else {
        return None;
    };
    match &mut envelope.result {
        serde_json::Value::Object(result) => {
            result.insert("output".to_owned(), serde_json::to_value(metadata).ok()?);
        }
        old => {
            let value = std::mem::take(old);
            *old = serde_json::json!({"value": value, "output": metadata});
        }
    }
    Some(())
}

fn incomplete(outcome: &ToolCallOutcome, original_text_bytes: usize) -> AppliedOutcome {
    let envelope_index = usable_envelope_index(&outcome.output);
    let omitted = outcome
        .output
        .0
        .iter()
        .enumerate()
        .filter_map(|(index, _)| (Some(index) != envelope_index).then_some(index))
        .collect::<Vec<_>>();
    let metadata = OutputMetadata {
        mode: OutputMode::Incomplete,
        budget_bytes: MAX_TEXT_BYTES,
        original_text_bytes,
        inline_text_bytes: 0,
        complete: false,
        target_access: outcome.target_access,
        externalized: Vec::new(),
        omitted_content_blocks: omitted,
        error: Some(delivery_error(outcome.effect, envelope_index.is_some())),
    };
    if let Some(index) = envelope_index {
        let envelope_fits = outcome
            .output
            .0
            .get(index)
            .and_then(OutContent::render_text)
            .is_some_and(|text| text.len() <= MAX_TEXT_BYTES);
        if !envelope_fits {
            return emergency(outcome, original_text_bytes);
        }
        let Some(envelope) = outcome.output.0.get(index).cloned() else {
            return emergency(outcome, original_text_bytes);
        };
        let mut contents = vec![envelope];
        if set_envelope_metadata(&mut contents, 0, &metadata).is_some()
            && let Some((output, metadata)) = stabilize(ToolOutput(contents), metadata, 0)
            && output.text_bytes() <= MAX_TEXT_BYTES
        {
            return AppliedOutcome {
                output,
                is_error: outcome.is_error,
                metadata: Some(metadata),
                response_pin: None,
            };
        }
    }
    emergency(outcome, original_text_bytes)
}

fn emergency(outcome: &ToolCallOutcome, original_text_bytes: usize) -> AppliedOutcome {
    let omitted_count = outcome.output.0.len();
    let omitted_content_blocks = (0..omitted_count.min(64)).collect::<Vec<_>>();
    let error = delivery_error_with_category(outcome.effect, false, "output_policy_failed");
    let metadata = OutputMetadata {
        mode: OutputMode::Incomplete,
        budget_bytes: MAX_TEXT_BYTES,
        original_text_bytes,
        inline_text_bytes: 0,
        complete: false,
        target_access: outcome.target_access,
        externalized: Vec::new(),
        omitted_content_blocks,
        error: Some(error),
    };
    let effect = match outcome.effect {
        ToolEffect::ReadOnly => "read_only",
        ToolEffect::MayMutate => "may_mutate",
    };
    let result = serde_json::json!({
        "effect": effect,
        "is_error": outcome.is_error,
        "omitted_content_block_count": omitted_count,
        "output": metadata,
    });
    let mut output = ToolOutput::result(bounded_tool(outcome.tool), result);
    let mut metadata = metadata;
    for _ in 0..=6 {
        let exact = output.text_bytes();
        if metadata.inline_text_bytes == exact {
            break;
        }
        metadata.inline_text_bytes = exact;
        if set_envelope_metadata(&mut output.0, 0, &metadata).is_none() {
            output = ToolOutput::result(
                "glass_output_policy",
                serde_json::json!({"complete": false, "error": "output_policy_failed"}),
            );
            metadata.inline_text_bytes = output.text_bytes();
            break;
        }
    }
    if output.text_bytes() > MAX_TEXT_BYTES {
        output = ToolOutput::result(
            "glass_output_policy",
            serde_json::json!({"complete": false, "error": "output_policy_failed"}),
        );
        metadata.inline_text_bytes = output.text_bytes();
    }
    AppliedOutcome {
        output,
        is_error: outcome.is_error,
        metadata: Some(metadata),
        response_pin: None,
    }
}

fn delivery_error(effect: ToolEffect, preserved: bool) -> OutputDeliveryError {
    delivery_error_with_category(effect, preserved, "artifact_storage_unavailable")
}

fn delivery_error_with_category(
    effect: ToolEffect,
    preserved: bool,
    category: &'static str,
) -> OutputDeliveryError {
    match effect {
        ToolEffect::ReadOnly => OutputDeliveryError {
            category,
            action_outcome_preserved: preserved,
            retry_safety: RetrySafety::SafeToRetryRead,
            recovery: READ_RECOVERY,
        },
        ToolEffect::MayMutate => OutputDeliveryError {
            category,
            action_outcome_preserved: preserved,
            retry_safety: RetrySafety::DoNotRepeatAction,
            recovery: MUTATE_RECOVERY,
        },
    }
}

fn usable_envelope_index(output: &ToolOutput) -> Option<usize> {
    let mut indices =
        output.0.iter().enumerate().filter_map(|(index, content)| {
            matches!(content, OutContent::Envelope(_)).then_some(index)
        });
    let first = indices.next()?;
    indices.next().is_none().then_some(first)
}

fn has_output_collision(output: &ToolOutput) -> bool {
    output.0.iter().any(|content| {
        matches!(content, OutContent::Envelope(EnvelopeBlock { result: serde_json::Value::Object(result), .. }) if result.contains_key("output"))
    })
}

fn has_pre_policy_link(output: &ToolOutput) -> bool {
    output
        .0
        .iter()
        .any(|content| matches!(content, OutContent::ResourceLink(_)))
}

fn descriptor_content_block(descriptor: &ArtifactDescriptor) -> Option<usize> {
    serde_json::to_value(descriptor)
        .ok()?
        .get("content_block")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

fn mime_for_role(role: TextRole) -> &'static str {
    match role {
        TextRole::Envelope => "application/json; charset=utf-8",
        TextRole::Observation | TextRole::Guidance | TextRole::ErrorDetail => {
            "text/plain; charset=utf-8"
        }
    }
}

fn bounded_tool(tool: &'static str) -> &'static str {
    if tool.len() <= 128 {
        tool
    } else {
        "glass_output_policy"
    }
}

#[derive(Serialize)]
struct ResponseManifest<'a> {
    schema: &'static str,
    tool: &'a str,
    is_error: bool,
    blocks: Vec<ManifestBlock>,
}

impl<'a> ResponseManifest<'a> {
    fn from_outcome(outcome: &'a ToolCallOutcome) -> Option<Self> {
        let blocks = outcome
            .output
            .0
            .iter()
            .enumerate()
            .map(|(index, content)| match content {
                OutContent::Envelope(envelope) => Some(ManifestBlock::Text {
                    index,
                    trust: TextTrust::Trusted,
                    role: TextRole::Envelope,
                    mime_type: mime_for_role(TextRole::Envelope),
                    text: envelope.render(),
                }),
                OutContent::Text(text) => Some(ManifestBlock::Text {
                    index,
                    trust: text.trust,
                    role: text.role,
                    mime_type: mime_for_role(text.role),
                    text: text.body.clone(),
                }),
                OutContent::Image(_) => Some(ManifestBlock::Image {
                    index,
                    retained_inline: true,
                }),
                OutContent::ResourceLink(_) => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            schema: "glass.output-manifest.v1",
            tool: outcome.tool,
            is_error: outcome.is_error,
            blocks,
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ManifestBlock {
    Text {
        index: usize,
        trust: TextTrust,
        role: TextRole,
        mime_type: &'static str,
        text: String,
    },
    Image {
        index: usize,
        retained_inline: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::FaultStage;
    use crate::output::{TextBlock, TextRole, TextTrust};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    const TEST_STORE_BYTES: u64 = 1 << 30;

    fn store() -> Result<(tempfile::TempDir, ArtifactStore), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let store = ArtifactStore::for_test(root.path(), TEST_STORE_BYTES)
            .map_err(|error| format!("store setup failed: {error:?}"))?;
        Ok((root, store))
    }

    fn policy_outcome(output: ToolOutput) -> ToolCallOutcome {
        ToolCallOutcome {
            tool: "glass_test",
            effect: ToolEffect::ReadOnly,
            is_error: false,
            target_access: TargetAccess::NoActiveTarget,
            output,
        }
    }

    fn text(body: String, trust: TextTrust, role: TextRole) -> OutContent {
        OutContent::Text(TextBlock { body, trust, role })
    }

    fn descriptor_value(content: &OutContent) -> Option<Value> {
        match content {
            OutContent::ResourceLink(descriptor) => serde_json::to_value(descriptor).ok(),
            _ => None,
        }
    }

    #[test]
    fn exact_byte_boundaries_preserve_only_under_budget_outputs()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        for bytes in [8_191, 8_192, 8_193] {
            let output = ToolOutput(vec![text(
                "x".repeat(bytes),
                TextTrust::Trusted,
                TextRole::Observation,
            )]);
            let original = output.render_text_blocks();
            let applied = policy.apply(policy_outcome(output));
            assert!(applied.output.text_bytes() <= MAX_TEXT_BYTES);
            assert_eq!(applied.output_metadata().is_none(), bytes <= MAX_TEXT_BYTES);
            if bytes <= MAX_TEXT_BYTES {
                assert_eq!(applied.output.render_text_blocks(), original);
            }
        }
        Ok(())
    }

    #[test]
    fn multibyte_and_multiblock_accounting_uses_utf8_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput(vec![
            text("é".repeat(2_048), TextTrust::Trusted, TextRole::Guidance),
            text(
                "🦀".repeat(1_024),
                TextTrust::Trusted,
                TextRole::Observation,
            ),
        ]);
        assert_eq!(output.text_bytes(), MAX_TEXT_BYTES);
        let original = output.render_text_blocks();
        let applied = policy.apply(policy_outcome(output));
        assert_eq!(applied.output.render_text_blocks(), original);
        assert!(applied.output_metadata().is_none());
        Ok(())
    }

    #[test]
    fn under_budget_error_image_first_and_multiblock_outputs_are_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput(vec![
            OutContent::Image(vec![1, 2, 3]),
            OutContent::Envelope(EnvelopeBlock {
                tool: "glass_test".into(),
                result: json!({"a": 1}),
            }),
            text("detail".into(), TextTrust::Trusted, TextRole::ErrorDetail),
        ]);
        let before = format!("{output:?}");
        let mut outcome = policy_outcome(output);
        outcome.is_error = true;
        let applied = policy.apply(outcome);
        assert_eq!(format!("{:?}", applied.output), before);
        assert!(applied.is_error);
        assert!(applied.output_metadata().is_none());
        Ok(())
    }

    #[test]
    fn applied_outcome_parts_keep_metadata_and_response_pin_together()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({}),
            vec![text(
                "x".repeat(8_300),
                TextTrust::Trusted,
                TextRole::Observation,
            )],
        );
        let applied = policy.apply(policy_outcome(output));
        let (output, is_error, metadata, response_pin) = applied.into_parts();
        assert!(output.text_bytes() <= MAX_TEXT_BYTES);
        assert!(!is_error);
        assert!(metadata.is_some());
        assert!(response_pin.is_some());
        Ok(())
    }

    #[test]
    fn largest_blocks_spill_first_with_stable_ties_and_same_indices()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({"kept": 7}),
            vec![
                text("a".repeat(4_000), TextTrust::Trusted, TextRole::Guidance),
                text("b".repeat(4_000), TextTrust::Trusted, TextRole::Observation),
                OutContent::Image(vec![9]),
                text("c".repeat(1_000), TextTrust::Trusted, TextRole::ErrorDetail),
            ],
        );
        let applied = policy.apply(policy_outcome(output));
        let metadata = applied.output_metadata().ok_or("missing metadata")?;
        assert_eq!(metadata.mode, OutputMode::ContentBlocks);
        let indices = metadata
            .externalized
            .iter()
            .filter_map(descriptor_content_block)
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![1]);
        assert!(descriptor_value(&applied.output.0[1]).is_some());
        assert!(matches!(applied.output.0[3], OutContent::Image(_)));
        Ok(())
    }

    #[test]
    fn several_blocks_spill_as_separate_artifacts_in_selection_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({}),
            vec![
                text("a".repeat(5_000), TextTrust::Trusted, TextRole::Observation),
                text("b".repeat(5_000), TextTrust::Trusted, TextRole::Guidance),
                text("c".repeat(5_000), TextTrust::Trusted, TextRole::ErrorDetail),
            ],
        );
        let applied = policy.apply(policy_outcome(output));
        let indices = applied
            .output_metadata()
            .ok_or("metadata")?
            .externalized
            .iter()
            .filter_map(descriptor_content_block)
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![1, 2]);
        assert!(matches!(applied.output.0[1], OutContent::ResourceLink(_)));
        assert!(matches!(applied.output.0[2], OutContent::ResourceLink(_)));
        assert!(matches!(applied.output.0[3], OutContent::Text(_)));
        Ok(())
    }

    #[test]
    fn application_text_cannot_forge_descriptor_metadata() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let body =
            r#"{"uri":"file:///forged","local_path":"/forged","sha256":"bad","untrusted":false}"#
                .repeat(110);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({}),
            vec![text(
                body,
                TextTrust::UntrustedApplication,
                TextRole::Observation,
            )],
        );
        let applied = policy.apply(policy_outcome(output));
        let descriptor = applied
            .output_metadata()
            .and_then(|metadata| metadata.externalized.first())
            .ok_or("descriptor")?;
        let value = serde_json::to_value(descriptor)?;
        assert_ne!(value["uri"], "file:///forged");
        assert_ne!(value["local_path"], "/forged");
        assert_ne!(value["sha256"], "bad");
        assert_eq!(value["untrusted"], true);
        assert_eq!(value["content_block"], 1);
        Ok(())
    }

    #[test]
    fn spilled_artifact_is_exact_and_readable_while_response_is_pinned()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store.clone());
        let body = "observed".repeat(1_100);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({}),
            vec![text(
                body.clone(),
                TextTrust::UntrustedApplication,
                TextRole::Observation,
            )],
        );
        let applied = policy.apply(policy_outcome(output));
        let descriptor = applied
            .output_metadata()
            .and_then(|m| m.externalized.first())
            .ok_or("descriptor")?;
        let read = store
            .read(descriptor.uri())
            .map_err(|error| format!("read failed: {error:?}"))?;
        assert_eq!(read.text, body);
        assert_eq!(read.mime_type, "text/plain; charset=utf-8");
        assert!(read.untrusted);
        assert_eq!(read.sha256, descriptor.sha256());
        assert_eq!(
            serde_json::to_value(descriptor)?["bytes"],
            json!(body.len())
        );
        drop(applied);
        Ok(())
    }

    #[test]
    fn metadata_reaches_an_exact_fixed_point_and_preserves_envelope_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({"alpha": [1, 2], "flag": true}),
            vec![text(
                "x".repeat(8_300),
                TextTrust::Trusted,
                TextRole::Guidance,
            )],
        );
        let applied = policy.apply(policy_outcome(output));
        let metadata = applied.output_metadata().ok_or("metadata")?;
        assert_eq!(metadata.inline_text_bytes, applied.output.text_bytes());
        let rendered: Value = serde_json::from_str(&applied.output.render_text_blocks()[0])?;
        assert_eq!(rendered["result"]["alpha"], json!([1, 2]));
        assert_eq!(rendered["result"]["flag"], json!(true));
        assert_eq!(
            rendered["result"]["output"]["inline_text_bytes"],
            json!(applied.output.text_bytes())
        );
        Ok(())
    }

    #[test]
    fn non_object_result_is_wrapped_exactly_only_for_block_spilling()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result_with(
            "glass_test",
            json!([1, {"x": 2}]),
            vec![text(
                "x".repeat(8_300),
                TextTrust::Trusted,
                TextRole::Guidance,
            )],
        );
        let applied = policy.apply(policy_outcome(output));
        let rendered: Value = serde_json::from_str(&applied.output.render_text_blocks()[0])?;
        assert_eq!(rendered["result"]["value"], json!([1, {"x": 2}]));
        Ok(())
    }

    #[test]
    fn no_envelope_uses_exact_response_manifest_and_preserves_image_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store.clone());
        let output = ToolOutput(vec![
            text(
                "α".repeat(4_200),
                TextTrust::UntrustedApplication,
                TextRole::Observation,
            ),
            OutContent::Image(vec![1]),
            text("guide".into(), TextTrust::Trusted, TextRole::Guidance),
            OutContent::Image(vec![2]),
        ]);
        let applied = policy.apply(policy_outcome(output));
        let metadata = applied.output_metadata().ok_or("metadata")?;
        assert_eq!(metadata.mode, OutputMode::ResponseManifest);
        let descriptor = metadata.externalized.first().ok_or("descriptor")?;
        assert!(descriptor.untrusted());
        assert_eq!(
            descriptor.mime_type(),
            "application/vnd.glass.output-manifest+json; charset=utf-8"
        );
        let manifest = store
            .read(descriptor.uri())
            .map_err(|error| format!("read: {error:?}"))?;
        let value: Value = serde_json::from_str(&manifest.text)?;
        assert_eq!(value["schema"], "glass.output-manifest.v1");
        assert_eq!(value["blocks"][0]["index"], 0);
        assert_eq!(value["blocks"][0]["kind"], "text");
        assert_eq!(value["blocks"][0]["trust"], "untrusted_application");
        assert_eq!(
            value["blocks"][1],
            json!({"kind":"image","index":1,"retained_inline":true})
        );
        assert_eq!(value["blocks"][2]["role"], "guidance");
        assert_eq!(
            value["blocks"][3],
            json!({"kind":"image","index":3,"retained_inline":true})
        );
        assert!(matches!(applied.output.0[2], OutContent::Image(ref bytes) if bytes == &[1]));
        assert!(matches!(applied.output.0[3], OutContent::Image(ref bytes) if bytes == &[2]));
        assert_eq!(
            serde_json::to_value(descriptor)?["original_content_blocks"],
            json!([0, 1, 2, 3])
        );
        Ok(())
    }

    #[test]
    fn rollback_failure_disables_externalization_but_not_under_budget_output()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let store = ArtifactStore::for_test_with_fault(
            root.path(),
            TEST_STORE_BYTES,
            FaultStage::PublicationRollbackCleanupFails,
        )
        .map_err(|error| format!("setup: {error:?}"))?;
        let seed = store
            .prepare(ArtifactDraft::content_block(
                "seed",
                "text/plain; charset=utf-8",
                false,
                0,
            ))
            .map_err(|error| format!("prepare: {error:?}"))?;
        let _seed_pin = store
            .publish(vec![seed])
            .map_err(|error| format!("publish: {error:?}"))?;
        let policy = OutputPolicy::new(store.clone());
        let large = ToolOutput::result_with(
            "glass_test",
            json!({}),
            vec![text(
                "x".repeat(8_300),
                TextTrust::Trusted,
                TextRole::Observation,
            )],
        );
        let failed = policy.apply(policy_outcome(large));
        assert!(failed.output.text_bytes() <= MAX_TEXT_BYTES);
        assert!(
            !failed
                .output
                .0
                .iter()
                .any(|content| matches!(content, OutContent::ResourceLink(_)))
        );
        assert_eq!(
            store.availability_error(),
            Some(ArtifactError::RollbackFailed)
        );

        let small = ToolOutput::result("glass_test", json!({"unchanged": true}));
        let before = format!("{small:?}");
        let applied = policy.apply(policy_outcome(small));
        assert_eq!(format!("{:?}", applied.output), before);
        assert!(applied.output_metadata().is_none());
        Ok(())
    }

    #[test]
    fn oversized_envelope_without_spillable_sibling_uses_manifest()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result("glass_test", json!({"body": "x".repeat(8_300)}));
        let applied = policy.apply(policy_outcome(output));
        assert_eq!(
            applied.output_metadata().map(|metadata| &metadata.mode),
            Some(&OutputMode::ResponseManifest)
        );
        assert!(applied.output.text_bytes() <= MAX_TEXT_BYTES);
        Ok(())
    }

    #[test]
    fn collision_and_pre_policy_link_use_bounded_emergency_output()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput::result_with(
            "glass_test",
            json!({"output": "application-owned"}),
            vec![text(
                "x".repeat(8_300),
                TextTrust::Trusted,
                TextRole::Observation,
            )],
        );
        let applied = policy.apply(policy_outcome(output));
        let metadata = applied.output_metadata().ok_or("metadata")?;
        assert_eq!(
            metadata.error.as_ref().ok_or("error")?.category,
            "output_policy_failed"
        );
        assert!(applied.output.text_bytes() < MAX_TEXT_BYTES);
        Ok(())
    }

    #[test]
    fn unavailable_storage_preserves_retry_safety_and_original_error_state() {
        for (effect, is_error, expected) in [
            (ToolEffect::ReadOnly, true, RetrySafety::SafeToRetryRead),
            (ToolEffect::MayMutate, false, RetrySafety::DoNotRepeatAction),
        ] {
            let policy = OutputPolicy::unavailable();
            let mut outcome = policy_outcome(ToolOutput::result_with(
                "glass_test",
                json!({"bounded": true}),
                vec![text(
                    "x".repeat(8_300),
                    TextTrust::Trusted,
                    TextRole::Observation,
                )],
            ));
            outcome.effect = effect;
            outcome.is_error = is_error;
            let applied = policy.apply(outcome);
            assert_eq!(applied.is_error, is_error);
            assert_eq!(
                applied
                    .output_metadata()
                    .and_then(|m| m.error.as_ref())
                    .map(|e| &e.retry_safety),
                Some(&expected)
            );
            assert!(applied.output.text_bytes() <= MAX_TEXT_BYTES);
        }
    }

    #[test]
    fn every_publication_fault_returns_bounded_output_without_links_or_registry_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        for fault in FaultStage::publication_stages(1) {
            let root = tempfile::tempdir()?;
            let store = ArtifactStore::for_test_with_fault(root.path(), TEST_STORE_BYTES, fault)
                .map_err(|error| format!("setup: {error:?}"))?;
            let policy = OutputPolicy::new(store.clone());
            let output = ToolOutput::result_with(
                "glass_test",
                json!({}),
                vec![text(
                    "x".repeat(8_300),
                    TextTrust::Trusted,
                    TextRole::Observation,
                )],
            );
            let applied = policy.apply(policy_outcome(output));
            assert!(applied.output.text_bytes() <= MAX_TEXT_BYTES);
            assert!(
                !applied
                    .output
                    .0
                    .iter()
                    .any(|content| matches!(content, OutContent::ResourceLink(_)))
            );
            assert_eq!(store.registry_len(), 0);
        }
        Ok(())
    }

    #[test]
    fn images_and_links_do_not_count_but_image_note_text_does()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, store) = store()?;
        let policy = OutputPolicy::new(store);
        let output = ToolOutput(vec![
            OutContent::Image(vec![0; 20_000]),
            text(
                "x".repeat(MAX_TEXT_BYTES),
                TextTrust::Trusted,
                TextRole::Guidance,
            ),
        ]);
        assert_eq!(output.text_bytes(), MAX_TEXT_BYTES);
        assert!(
            policy
                .apply(policy_outcome(output))
                .output_metadata()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn emergency_envelope_is_bounded_even_for_huge_tool_and_block_count() {
        static HUGE_TOOL: &str = include_str!("../../../README.md");
        let output = ToolOutput(
            (0..10_000)
                .map(|_| OutContent::Image(vec![]))
                .chain(std::iter::once(text(
                    "x".repeat(8_193),
                    TextTrust::Trusted,
                    TextRole::Observation,
                )))
                .collect(),
        );
        let outcome = ToolCallOutcome {
            tool: HUGE_TOOL,
            effect: ToolEffect::MayMutate,
            is_error: false,
            target_access: TargetAccess::DeniedBySandbox,
            output,
        };
        let applied = emergency(&outcome, 8_193);
        assert!(applied.output.text_bytes() <= MAX_TEXT_BYTES);
        assert!(!applied.output.render_text_blocks()[0].contains(HUGE_TOOL));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn arbitrary_outputs_always_fit_and_preserve_under_budget(
            chunks in prop::collection::vec((0usize..4000, any::<bool>(), 0u8..3), 0..8),
            image_positions in prop::collection::vec(0usize..8, 0..4),
            multibyte in any::<bool>(),
        ) {
            let root = tempfile::tempdir().map_err(|error| TestCaseError::fail(error.to_string()))?;
            let store = ArtifactStore::for_test(root.path(), TEST_STORE_BYTES)
                .map_err(|error| TestCaseError::fail(format!("{error:?}")))?;
            let policy = OutputPolicy::new(store);
            let mut contents = vec![OutContent::Envelope(EnvelopeBlock { tool: "glass_prop".into(), result: json!({}) })];
            for (index, (length, untrusted, role)) in chunks.into_iter().enumerate() {
                if image_positions.contains(&index) {
                    contents.push(OutContent::Image(vec![index as u8; index + 1]));
                }
                let unit = if multibyte { "é" } else { "x" };
                let role = match role { 0 => TextRole::Observation, 1 => TextRole::Guidance, _ => TextRole::ErrorDetail };
                let trust = if untrusted { TextTrust::UntrustedApplication } else { TextTrust::Trusted };
                contents.push(text(unit.repeat(length), trust, role));
            }
            let output = ToolOutput(contents);
            let original_bytes = output.text_bytes();
            let original_debug = format!("{output:?}");
            let original_images = output.0.iter().filter_map(|content| match content { OutContent::Image(bytes) => Some(bytes.clone()), _ => None }).collect::<Vec<_>>();
            let applied = policy.apply(policy_outcome(output));
            prop_assert!(applied.output.text_bytes() <= MAX_TEXT_BYTES);
            let final_images = applied.output.0.iter().filter_map(|content| match content { OutContent::Image(bytes) => Some(bytes.clone()), _ => None }).collect::<Vec<_>>();
            prop_assert_eq!(final_images, original_images);
            if original_bytes <= MAX_TEXT_BYTES {
                prop_assert!(applied.output_metadata().is_none());
                prop_assert_eq!(format!("{:?}", applied.output), original_debug);
            }
        }
    }
}
