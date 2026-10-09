//! Lossless, bounded compact-outline revisions; native reads remain fresh.

use std::sync::{Arc, Mutex, Weak};

use glass_core::{AxObservation, ObservationContext, ObservationEpoch, ObservationInvalidation};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{OutContent, ToolOutput};
use crate::params::A11ySnapshotDiffArgs;

pub(crate) const TOOL: &str = "glass_a11y_snapshot_diff";
const MAX_BASELINE_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 16_384;

struct Baseline {
    outline: Box<str>,
    fingerprint: Box<str>,
    context: ObservationContext,
    context_id: Box<str>,
    revision: Box<str>,
}

struct State {
    sequence: u64,
    baseline: Option<Arc<Baseline>>,
    published: bool,
    continuity: Arc<()>,
}

impl State {
    fn invalidate(&mut self) {
        self.baseline = None;
        self.published = false;
        self.continuity = Arc::new(());
    }
}

pub(crate) struct SnapshotRevisionStore {
    server_epoch: String,
    epoch: ObservationEpoch,
    state: Mutex<State>,
}

impl ObservationInvalidation for SnapshotRevisionStore {
    fn invalidate(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
    }
}

impl SnapshotRevisionStore {
    pub(crate) fn new(epoch: ObservationEpoch) -> Arc<Self> {
        let store = Arc::new(Self {
            server_epoch: crate::artifacts::new_server_id(),
            epoch: epoch.clone(),
            state: Mutex::new(State {
                sequence: 0,
                baseline: None,
                published: false,
                continuity: Arc::new(()),
            }),
        });
        let listener: Arc<dyn ObservationInvalidation> = store.clone();
        epoch.subscribe(&listener);
        store
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        args: &A11ySnapshotDiffArgs,
    ) -> Result<PendingRevision, String> {
        validate_revision(args.base_revision.as_deref())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "snapshot revision state unavailable")?;
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or("snapshot revision sequence exhausted")?;
        Ok(PendingRevision {
            request: Arc::new(Reservation {
                store: self.clone(),
                sequence: state.sequence,
                revision: format!("{}-{:016x}", self.server_epoch, state.sequence),
                requested_base: args.base_revision.clone(),
                baseline: state
                    .published
                    .then(|| state.baseline.as_ref().map(Arc::downgrade))
                    .flatten(),
                continuity: state.continuity.clone(),
                candidate: Mutex::new(None),
            }),
            completed: false,
        })
    }

    fn fail(&self, sequence: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.sequence == sequence {
            state.invalidate();
        }
    }
}

pub(crate) fn validate_revision(revision: Option<&str>) -> Result<(), String> {
    if revision.is_some_and(|value| value.is_empty() || value.len() > 128 || !value.is_ascii()) {
        Err("base_revision must be nonempty ASCII of at most 128 bytes; omit it for a fresh full observation".into())
    } else {
        Ok(())
    }
}

pub(crate) struct PendingRevision {
    pub(crate) request: Arc<Reservation>,
    completed: bool,
}

impl PendingRevision {
    /// Called only after the complete MCP response has been constructed, without another await.
    pub(crate) fn complete(mut self, complete: bool) {
        if complete {
            let candidate = self
                .request
                .candidate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(candidate) = candidate {
                let generation = candidate.generation;
                let published = self.request.store.epoch.if_current(&generation, || {
                    let mut state = self
                        .request
                        .store
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if state.sequence == self.request.sequence
                        && Arc::ptr_eq(&state.continuity, &candidate.continuity)
                        && state
                            .baseline
                            .as_ref()
                            .is_some_and(|base| base.revision.as_ref() == self.request.revision)
                    {
                        state.published = true;
                        true
                    } else {
                        false
                    }
                });
                self.completed = published == Some(true);
            }
        }
    }
}

impl Drop for PendingRevision {
    fn drop(&mut self) {
        if !self.completed {
            self.request.store.fail(self.request.sequence);
        }
    }
}

pub(crate) struct Reservation {
    store: Arc<SnapshotRevisionStore>,
    sequence: u64,
    revision: String,
    requested_base: Option<String>,
    baseline: Option<Weak<Baseline>>,
    continuity: Arc<()>,
    candidate: Mutex<Option<DeliveryCandidate>>,
}

struct DeliveryCandidate {
    generation: glass_core::ObservationGeneration,
    continuity: Arc<()>,
}

impl Reservation {
    pub(crate) fn read(
        &self,
        glass: &mut glass_core::Glass,
        args: &A11ySnapshotDiffArgs,
    ) -> super::ToolResult {
        let observation = glass
            .a11y_observation(args.max_nodes.map(|n| n as usize))
            .map_err(|e| e.to_string())?;
        self.encode(observation)
    }

    fn encode(&self, observation: AxObservation) -> super::ToolResult {
        let outline = glass_core::outline::render_compact(&observation.tree);
        self.encode_outline(observation, outline)
    }

    fn encode_outline(&self, observation: AxObservation, outline: String) -> super::ToolResult {
        let tree = observation.tree;
        let line_count = outline.split_inclusive('\n').count();
        let completeness = json!({
            "count": tree.count,
            "truncated": tree.truncated.map(|t| json!({
                "limit": match t.limit {
                    glass_core::TruncationLimit::Nodes => "nodes",
                    glass_core::TruncationLimit::Depth => "depth",
                    glass_core::TruncationLimit::Siblings => "siblings",
                },
                "limit_value": t.limit_value,
                "nodes_walked": t.nodes_walked,
            })),
            "unreadable": tree.unreadable,
            "unexposed": tree.unexposed,
            "subject_mismatch": tree.subject.is_some(),
        });
        let subject = tree
            .subject
            .as_ref()
            .map(|s| json!({"asked": s.asked, "actual": s.actual}));
        let mut guidance: Vec<String> = [
            tree.empty_guidance().map(str::to_owned),
            super::a11y_truncation_steer(&tree),
            tree.unreadable_notice(),
            tree.unexposed_notice(),
            tree.document_guidance(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if subject.is_some() {
            guidance.push("The observed application subject differs from the requested subject; inspect the untrusted subject fields and recover with a full observation.".into());
        }
        let fingerprint =
            json!({"completeness": completeness, "subject": subject, "guidance": guidance})
                .to_string();
        let context = observation.context.filter(|_| tree.subject.is_none());
        let retained_bytes = retained_size(&outline, &fingerprint, context.as_ref());
        let cacheable =
            context.is_some() && retained_bytes <= MAX_BASELINE_BYTES && line_count <= MAX_LINES;
        let baseline_owner = {
            let state = self
                .store
                .state
                .lock()
                .map_err(|_| "snapshot revision state unavailable")?;
            if state.published && Arc::ptr_eq(&state.continuity, &self.continuity) {
                self.baseline.as_ref().and_then(Weak::upgrade)
            } else {
                None
            }
        };
        let baseline = baseline_owner.as_deref();
        let same_context = context
            .as_ref()
            .is_some_and(|ctx| baseline.is_some_and(|base| &base.context == ctx));
        let context_id = if same_context {
            baseline
                .expect("same_context requires a baseline")
                .context_id
                .to_string()
        } else {
            crate::artifacts::new_server_id()
        };
        // An incompatible observation releases retained state before output processing.
        if !cacheable || (baseline.is_some() && !same_context) {
            self.store.invalidate();
        }
        let reason = if context.is_none() {
            "context_unproven"
        } else if !cacheable {
            "size_limit"
        } else if self.requested_base.is_none() {
            "requested"
        } else if baseline
            .is_none_or(|base| Some(base.revision.as_ref()) != self.requested_base.as_deref())
        {
            "baseline_unavailable"
        } else if !same_context {
            "context_changed"
        } else if baseline.is_some_and(|base| base.fingerprint.as_ref() != fingerprint) {
            "metadata_changed"
        } else {
            "not_smaller"
        };
        let mut result = json!({
            "version": 1, "kind": "full", "revision": self.revision,
            "context_id": context_id, "outline_format": "compact-v1",
            "body_sha256": Sha256::digest(outline.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "body_bytes": outline.len(), "line_count": line_count, "cacheable": cacheable,
            "completeness": completeness, "full_reason": reason,
        });
        let mut payload = json!({"outline": outline});
        if let Some(subject) = &subject {
            payload["subject"] = subject.clone();
        }
        if reason == "not_smaller" {
            let base = baseline.expect("diff eligibility requires a matching baseline");
            let edits = splice(&base.outline, &outline);
            let mut delta_result = result.clone();
            delta_result["kind"] = json!(if edits.is_empty() {
                "unchanged"
            } else {
                "diff"
            });
            delta_result
                .as_object_mut()
                .expect("result is an object")
                .remove("full_reason");
            delta_result["base_revision"] = json!(base.revision);
            let mut delta_payload = json!({"edits": edits});
            if let Some(subject) = &subject {
                delta_payload["subject"] = subject.clone();
            }
            if logical_size(&delta_result, &delta_payload, &guidance)?
                < logical_size(&result, &payload, &guidance)?
            {
                result = delta_result;
                payload = delta_payload;
            }
        }
        let mut content = vec![OutContent::untrusted_observation(&payload.to_string())];
        content.extend(guidance.into_iter().map(OutContent::trusted_guidance));
        drop(baseline_owner);
        if cacheable {
            let mut state = self
                .store
                .state
                .lock()
                .map_err(|_| "snapshot revision state unavailable")?;
            if state.sequence == self.sequence {
                state.baseline = None;
                state.published = false;
                let context = context.expect("cacheable context is proven");
                *self
                    .candidate
                    .lock()
                    .map_err(|_| "snapshot revision candidate unavailable")? =
                    Some(DeliveryCandidate {
                        generation: context.generation.clone(),
                        continuity: state.continuity.clone(),
                    });
                state.baseline = Some(Arc::new(Baseline {
                    outline: outline.into_boxed_str(),
                    fingerprint: fingerprint.into_boxed_str(),
                    context,
                    context_id: context_id.into_boxed_str(),
                    revision: self.revision.clone().into_boxed_str(),
                }));
            }
        }
        Ok(ToolOutput::result_with(TOOL, result, content))
    }
}

fn retained_size(outline: &str, fingerprint: &str, context: Option<&ObservationContext>) -> usize {
    let context_bytes = context.map_or(0, |ctx| {
        ctx.backend
            .capacity()
            .saturating_add(ctx.pids.capacity().saturating_mul(size_of::<u32>()))
            .saturating_add(ctx.a11y_bus_addr.as_ref().map_or(0, String::capacity))
    });
    outline
        .len()
        .saturating_add(fingerprint.len())
        .saturating_add(context_bytes)
        .saturating_add(size_of::<Baseline>())
        .saturating_add(32 + 49 + 32)
}

#[derive(Debug, Serialize)]
struct Edit<'a> {
    start: usize,
    delete: usize,
    insert: Vec<&'a str>,
}

fn splice<'a>(base: &str, current: &'a str) -> Vec<Edit<'a>> {
    let old: Vec<_> = base.split_inclusive('\n').collect();
    let new: Vec<_> = current.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if prefix == old.len() && prefix == new.len() {
        return Vec::new();
    }
    vec![Edit {
        start: prefix,
        delete: old.len() - prefix - suffix,
        insert: new[prefix..new.len() - suffix].to_vec(),
    }]
}

fn logical_size(result: &Value, payload: &Value, guidance: &[String]) -> Result<usize, String> {
    let envelope = json!({"ok": true, "tool": TOOL, "result": result}).to_string();
    let body =
        crate::untrusted::wrap_with_nonce(&payload.to_string(), "00000000000000000000000000000000");
    let mut content = vec![ContentBlock::text(envelope), ContentBlock::text(body)];
    content.extend(guidance.iter().map(ContentBlock::text));
    serde_json::to_vec(&CallToolResult::success(content))
        .map(|bytes| bytes.len())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
